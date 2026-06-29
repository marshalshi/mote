mod client;
mod config;
mod llm;
mod tui;
mod workspace;

use anyhow::{Context, Result};
use clap::Parser;
use std::process::Stdio;
use tokio::process::{Child, Command};

const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:9847";
const API_KEY_PROVIDERS: &[ProviderLogin] = &[
    ProviderLogin {
        name: "deepseek",
        display_name: "DeepSeek",
        key_url: "https://platform.deepseek.com/api_keys",
        example_model: "deepseek/deepseek-v4-flash",
    },
    ProviderLogin {
        name: "glm",
        display_name: "GLM (Z.ai)",
        key_url: "https://z.ai/manage-apikey/apikey-list",
        example_model: "glm/glm-5.2",
    },
    ProviderLogin {
        name: "kimi",
        display_name: "Kimi (Moonshot)",
        key_url: "https://platform.kimi.ai/console/api-keys",
        example_model: "kimi/kimi-k2.6",
    },
    ProviderLogin {
        name: "minimax",
        display_name: "MiniMax",
        key_url: "https://platform.minimax.io/user-center/basic-information/interface-key",
        example_model: "minimax/MiniMax-M3",
    },
];

struct ProviderLogin {
    name: &'static str,
    display_name: &'static str,
    key_url: &'static str,
    example_model: &'static str,
}

/// Mote — starts a local server and opens the TUI by default.
#[derive(Parser, Debug)]
#[command(name = "mote", version, about)]
struct Cli {
    /// Run only the server daemon.
    #[arg(long, conflicts_with = "tui")]
    server: bool,

    /// Run only the TUI frontend, connecting to an existing server.
    #[arg(long, conflicts_with = "server")]
    tui: bool,

    /// Server address for TUI-only mode.
    #[arg(long, default_value = DEFAULT_SERVER_URL)]
    server_url: String,

    /// Single message mode.
    #[arg(short = 'M', long)]
    message: Option<String>,

    /// Resume a saved session by ID.
    #[arg(short = 'r', long)]
    resume: Option<String>,

    /// Login to a provider. Omit provider to choose interactively.
    #[arg(short = 'L', long, num_args = 0..=1)]
    login: Option<Option<String>>,

    /// Verbose logging.
    #[arg(short = 'v', long, global = true)]
    verbose: bool,

    /// Override runtime session key (used for history namespace).
    #[arg(long)]
    session_key: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.server {
        return run_server_only().await;
    }

    let workspace_ctx =
        workspace::resolve_workspace_context(cli.session_key.as_deref())?;

    // Logging setup: verbose/debug → file, otherwise → stderr
    let env_log = std::env::var("RUST_LOG").unwrap_or_default();
    let wants_debug = cli.verbose
        || env_log.eq_ignore_ascii_case("debug")
        || env_log.eq_ignore_ascii_case("trace")
        || env_log.contains("mote=debug");

    if wants_debug {
        let log_dir = resolve_log_dir_from_config();
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

    let (_server_child, server_url) = if cli.tui {
        (None, cli.server_url.clone())
    } else {
        let port = reserve_local_port()?;
        let url = format!("http://127.0.0.1:{port}");
        (Some(spawn_local_server(port, cli.verbose)?), url)
    };

    // Create client
    let client = client::MoteClient::new(&server_url);

    // Wait for server to be ready
    for _ in 0..30 {
        if client.health().await {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }
    if !client.health().await {
        anyhow::bail!("Could not connect to mote-server at {}", server_url);
    }

    // Handle login first (no TUI needed)
    if let Some(provider) = &cli.login {
        let provider = match provider.as_deref() {
            Some(name) => provider_login(name)?,
            None => select_login_provider()?,
        };
        return login_api_key_provider(&client, provider).await;
    }

    // Fetch UI config from server
    let ui_config = client
        .get_config()
        .await
        .context("Failed to fetch UI config from server")?;

    let model_info = ui_config.model_info.clone();

    // Handle single message mode
    if let Some(msg) = &cli.message {
        return single_message(&client, &ui_config, msg, &workspace_ctx).await;
    }

    // Start TUI, optionally resuming a session
    let mut app = tui::state::App::new_with_workspace(
        &ui_config,
        model_info,
        workspace_ctx.root.to_string_lossy().to_string(),
        workspace_ctx.repo_agents_md.clone(),
        workspace_ctx.runtime_session_key.clone(),
    );

    // Resume a saved session if requested
    if let Some(session_id) = &cli.resume {
        match client
            .load_session(&workspace_ctx.runtime_session_key, session_id)
            .await
        {
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
                app.compaction_state = session.compaction;
                app.active_session_id = Some(session_id.clone());
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

fn resolve_log_dir_from_config() -> std::path::PathBuf {
    let default_dir = dirs::home_dir()
        .map(|h| h.join(".config").join("mote").join("logs"))
        .unwrap_or_else(|| std::path::PathBuf::from("logs"));
    let config_path = if let Some(home) = dirs::home_dir() {
        let p = home.join(".config").join("mote").join("config.toml");
        if p.exists() {
            p
        } else {
            std::path::PathBuf::from("config.toml")
        }
    } else {
        std::path::PathBuf::from("config.toml")
    };
    let Ok(raw) = std::fs::read_to_string(&config_path) else {
        return default_dir;
    };
    let Ok(v) = toml::from_str::<toml::Value>(&raw) else {
        return default_dir;
    };
    let Some(dir_str) = v
        .get("logging")
        .and_then(|l| l.get("dir"))
        .and_then(|d| d.as_str())
    else {
        return default_dir;
    };
    let p = std::path::PathBuf::from(dir_str);
    if p.is_relative() {
        config_path
            .parent()
            .map(|base| base.join(p))
            .unwrap_or(default_dir)
    } else {
        p
    }
}

struct ManagedServer {
    child: Child,
}

impl Drop for ManagedServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn reserve_local_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .context("Failed to reserve a local server port")?;
    let port = listener
        .local_addr()
        .context("Failed to read reserved local server address")?
        .port();
    drop(listener);
    Ok(port)
}

fn spawn_local_server(port: u16, verbose: bool) -> Result<ManagedServer> {
    let mut cmd = server_command()?;
    cmd.env("MOTE_SERVER_PORT", port.to_string());
    if verbose {
        cmd.stderr(Stdio::inherit()).stdout(Stdio::inherit());
    } else {
        cmd.stderr(Stdio::null()).stdout(Stdio::null());
    }
    let child = cmd.spawn().context("Failed to start local mote-server")?;
    Ok(ManagedServer { child })
}

async fn run_server_only() -> Result<()> {
    let mut cmd = server_command()?;
    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("Failed to run mote-server")?;
    if !status.success() {
        anyhow::bail!("mote-server exited with {status}");
    }
    Ok(())
}

fn server_command() -> Result<Command> {
    if let Some(path) = sibling_server_binary()? {
        return Ok(Command::new(path));
    }

    if command_exists("mote-server") {
        return Ok(Command::new("mote-server"));
    }

    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(std::path::Path::to_path_buf)
        .context("Failed to resolve workspace root")?;
    let mut cmd = Command::new("cargo");
    cmd.arg("run")
        .arg("-p")
        .arg("mote-server")
        .arg("--quiet")
        .current_dir(workspace);
    Ok(cmd)
}

fn sibling_server_binary() -> Result<Option<std::path::PathBuf>> {
    let current = std::env::current_exe()
        .context("Failed to resolve current executable")?;
    let Some(dir) = current.parent() else {
        return Ok(None);
    };
    let candidate = dir.join(if cfg!(windows) {
        "mote-server.exe"
    } else {
        "mote-server"
    });
    Ok(candidate.is_file().then_some(candidate))
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
    })
}

async fn single_message(
    client: &client::MoteClient,
    ui: &marshaling_protocol::UiConfig,
    msg: &str,
    workspace_ctx: &workspace::WorkspaceContext,
) -> Result<()> {
    let request = marshaling_protocol::ChatRequest {
        message: msg.to_string(),
        agent: ui.default_agent.clone(),
        model_override: None,
        provider_override: None,
        history: vec![],
        session_id: None,
        workspace_root: Some(workspace_ctx.root.to_string_lossy().to_string()),
        repo_agents_md: workspace_ctx.repo_agents_md.clone(),
        runtime_session_key: Some(workspace_ctx.runtime_session_key.clone()),
        run_id: None,
        compaction: None,
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
            marshaling_protocol::ServerEvent::Done { .. }
            | marshaling_protocol::ServerEvent::Cancelled { .. }
            | marshaling_protocol::ServerEvent::NeedsContinuation { .. } => {
                break;
            }
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

fn provider_login(name: &str) -> Result<&'static ProviderLogin> {
    API_KEY_PROVIDERS
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Unknown provider: {}. Supported: {}",
                name,
                API_KEY_PROVIDERS
                    .iter()
                    .map(|p| p.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn select_login_provider() -> Result<&'static ProviderLogin> {
    println!();
    println!("  Select provider to login:");
    for (idx, provider) in API_KEY_PROVIDERS.iter().enumerate() {
        println!(
            "  {}. {} ({})",
            idx + 1,
            provider.display_name,
            provider.name
        );
    }
    println!();
    print!("  Provider [1-{}]: ", API_KEY_PROVIDERS.len());
    use std::io::Write;
    std::io::stdout().flush().ok();

    let mut choice = String::new();
    std::io::stdin().read_line(&mut choice).ok();
    let idx: usize = choice
        .trim()
        .parse()
        .context("Invalid provider selection")?;
    API_KEY_PROVIDERS
        .get(idx.saturating_sub(1))
        .ok_or_else(|| anyhow::anyhow!("Provider selection out of range"))
}

/// Save a provider API key to the server's auth.json.
async fn login_api_key_provider(
    client: &client::MoteClient,
    provider: &ProviderLogin,
) -> Result<()> {
    println!();
    println!("  🔑 {} API Key Setup", provider.display_name);
    println!();
    println!("  Get your API key at: {}", provider.key_url);
    println!();
    print!("  Enter your {} API key: ", provider.display_name);
    use std::io::Write;
    std::io::stdout().flush().ok();

    let key = rpassword::read_password()
        .unwrap_or_default()
        .trim()
        .to_string();

    if key.is_empty() {
        anyhow::bail!("No API key entered.");
    }

    client
        .save_credential(provider.name, "api_key", &key)
        .await
        .with_context(|| {
            format!("Failed to save {} API key", provider.display_name)
        })?;

    println!();
    println!(
        "  ✅ {} API key saved to ~/.config/mote/auth.json",
        provider.display_name
    );
    println!("  You can now use {} models.", provider.display_name);
    println!("  Switch with: /model {}", provider.example_model);
    println!();
    Ok(())
}
