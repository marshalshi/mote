mod client;
mod config;
mod llm;
mod tui;

use anyhow::{Context, Result};
use clap::Parser;

/// Mote client — connects to a mote-server.
#[derive(Parser, Debug)]
#[command(name = "mote", version, about)]
struct Cli {
    /// Server address (default: http://127.0.0.1:9847).
    #[arg(short, long, default_value = "http://127.0.0.1:9847")]
    server: String,

    /// Single message mode.
    #[arg(short = 'M', long)]
    message: Option<String>,

    /// Resume a saved session by ID.
    #[arg(short = 'r', long)]
    resume: Option<String>,

    /// Login to a provider (e.g., github).
    #[arg(short = 'L', long)]
    login: Option<String>,

    /// Verbose logging.
    #[arg(short = 'v', long, global = true)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Logging setup: verbose/debug → file, otherwise → stderr
    let env_log = std::env::var("RUST_LOG").unwrap_or_default();
    let wants_debug = cli.verbose
        || env_log.eq_ignore_ascii_case("debug")
        || env_log.eq_ignore_ascii_case("trace")
        || env_log.contains("mote=debug");

    if wants_debug {
        let log_dir = std::path::PathBuf::from("logs");
        std::fs::create_dir_all(&log_dir).ok();
        let log_path = log_dir.join("mote.log");
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap_or_else(|_| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/null")
                    .unwrap()
            });
        let (non_blocking, _guard) = tracing_appender::non_blocking(log_file);
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "debug".into()),
            )
            .with_writer(non_blocking)
            .with_ansi(false)
            .init();
        Box::leak(Box::new(_guard));
        tracing::info!(
            "Verbose logging enabled, writing to {}",
            log_path.display()
        );
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "warn".into()),
            )
            .init();
    }

    // Create client
    let client = client::MoteClient::new(&cli.server);

    // Wait for server to be ready
    for _ in 0..30 {
        if client.health().await {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }
    if !client.health().await {
        anyhow::bail!(
            "Could not connect to mote-server at {}",
            cli.server
        );
    }

    // Handle login first (no TUI needed)
    if let Some(provider) = &cli.login {
        match provider.as_str() {
            "github" => {
                return login_github(&client).await;
            }
            "deepseek" => {
                return login_deepseek(&client).await;
            }
            other => {
                anyhow::bail!(
                    "Unknown provider: {}. Supported: github, deepseek",
                    other
                );
            }
        }
    }

    // Fetch UI config from server
    let ui_config = client
        .get_config()
        .await
        .context("Failed to fetch UI config from server")?;

    let model_info = ui_config.model_info.clone();

    // Handle single message mode
    if let Some(msg) = &cli.message {
        return single_message(&client, &ui_config, msg).await;
    }

    // Start TUI, optionally resuming a session
    let mut app = tui::state::App::new(&ui_config, model_info);

    // Resume a saved session if requested
    if let Some(session_id) = &cli.resume {
        match client.load_session(session_id).await {
            Ok(session) => {
                for hm in &session.messages {
                    let role = match hm.role.as_str() {
                        "user" => crate::llm::Role::User,
                        "assistant" => crate::llm::Role::Assistant,
                        _ => continue,
                    };
                    app.messages.push(tui::state::DisplayMessage {
                        role,
                        content: hm.content.clone(),
                        thinking: None,
                        source: tui::state::MessageSource::Conversation,
                    });
                }
                tracing::info!(
                    "Resumed session {} with {} messages",
                    session_id,
                    session.messages.len()
                );
            }
            Err(e) => {
                eprintln!("Failed to load session '{}': {:#}", session_id, e);
                return Err(e);
            }
        }
    }

    let _app = tui::run_tui(app, &client).await?;
    tracing::info!("TUI exited");

    // On quit, just exit
    Ok(())
}

async fn single_message(
    client: &client::MoteClient,
    _ui: &marshaling_protocol::UiConfig,
    msg: &str,
) -> Result<()> {
    let request = marshaling_protocol::ChatRequest {
        message: msg.to_string(),
        agent: "default".into(),
        model_override: None,
        provider_override: None,
        history: vec![],
        session_id: None,
    };
    let mut stream = client
        .chat_stream(request)
        .await
        .context("Failed to start chat stream")?;

    let mut content = String::new();
    while let Some(event) = stream.rx.recv().await {
        match event {
            marshaling_protocol::ServerEvent::TextDelta { data } => {
                print!("{}", data);
                content.push_str(&data);
            }
            marshaling_protocol::ServerEvent::Done { .. } => break,
            marshaling_protocol::ServerEvent::Error { message } => {
                eprintln!("Error: {}", message);
                return Err(anyhow::anyhow!(message));
            }
            _ => {} // ignore tool events, reasoning, etc.
        }
    }
    // Ensure final newline
    if !content.ends_with('\n') {
        println!();
    }
    Ok(())
}

/// Save a GitHub PAT token to the server's auth.json.
async fn login_github(client: &client::MoteClient) -> Result<()> {
    println!();
    println!("  🔑 GitHub Models Authentication");
    println!();
    println!(
        "  GitHub Models requires a Personal Access Token with 'models:read' scope."
    );
    println!();
    println!("  1. Go to:  https://github.com/settings/tokens?type=beta");
    println!("  2. Click \"Generate new token\" → \"Fine-grained token\"");
    println!("  3. Name: \"mote\", Expiration: 90 days");
    println!("  4. Repository access: \"Public repositories only\"");
    println!("  5. Account permissions → Models → Read");
    println!("  6. Click \"Generate token\" and copy it");
    println!();
    print!("  Enter your GitHub token: ");
    use std::io::Write;
    std::io::stdout().flush().ok();

    let mut token = String::new();
    std::io::stdin().read_line(&mut token).ok();
    let token = token.trim().to_string();

    if token.is_empty() {
        anyhow::bail!("No token entered.");
    }

    client
        .save_credential("github", "token", &token)
        .await
        .context("Failed to save GitHub token")?;

    println!();
    println!("  ✅ GitHub token saved to ~/.config/mote/auth.json");
    println!("  You can now use GitHub models.");
    println!("  Switch with: /model github/gpt-4o-mini");
    println!();
    Ok(())
}

/// Save a DeepSeek API key to the server's auth.json.
async fn login_deepseek(client: &client::MoteClient) -> Result<()> {
    println!();
    println!("  🔑 DeepSeek API Key Setup");
    println!();
    println!("  Get your API key at: https://platform.deepseek.com/api_keys");
    println!();
    print!("  Enter your DeepSeek API key: ");
    use std::io::Write;
    std::io::stdout().flush().ok();

    let mut key = String::new();
    std::io::stdin().read_line(&mut key).ok();
    let key = key.trim().to_string();

    if key.is_empty() {
        anyhow::bail!("No API key entered.");
    }
    if !key.starts_with("sk-") {
        eprintln!("  Warning: DeepSeek API keys usually start with 'sk-'");
    }

    client
        .save_credential("deepseek", "api_key", &key)
        .await
        .context("Failed to save DeepSeek API key")?;

    println!();
    println!("  ✅ DeepSeek API key saved to ~/.config/mote/auth.json");
    println!("  You can now use DeepSeek models.");
    println!();
    Ok(())
}
