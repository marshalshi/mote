use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomSlashCommand {
    pub name: String,
    pub template: String,
    pub description: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CustomCommandInvocation {
    pub command: CustomSlashCommand,
    pub arguments: String,
}

#[derive(Debug, Clone, Default)]
pub struct CustomCommandLoadResult {
    pub commands: Vec<CustomSlashCommand>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct PartialCommandConfig {
    template: Option<String>,
    description: Option<String>,
    agent: Option<String>,
    model: Option<String>,
}

pub fn load_custom_commands(_workspace_root: &Path) -> CustomCommandLoadResult {
    let mut commands = HashMap::new();
    let mut warnings = Vec::new();

    if let Some(home) = dirs::home_dir() {
        load_command_dir(
            &home.join(".config").join("mote").join("commands"),
            &mut commands,
            &mut warnings,
        );
    }
    let mut commands: Vec<_> = commands.into_values().collect();
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    CustomCommandLoadResult { commands, warnings }
}

fn load_command_dir(
    dir: &Path,
    commands: &mut HashMap<String, CustomSlashCommand>,
    warnings: &mut Vec<String>,
) {
    if !dir.exists() {
        return;
    }

    load_command_dir_recursive(dir, dir, commands, warnings);
}

fn load_command_dir_recursive(
    root: &Path,
    dir: &Path,
    commands: &mut HashMap<String, CustomSlashCommand>,
    warnings: &mut Vec<String>,
) {
    if !dir.is_dir() {
        warnings.push(format!(
            "Custom command path is not a directory: {}",
            dir.display()
        ));
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            warnings.push(format!(
                "Failed to read custom command directory {}: {e}",
                dir.display()
            ));
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            load_command_dir_recursive(root, &path, commands, warnings);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        match load_markdown_command(root, &path) {
            Ok(command) => {
                commands.insert(command.name.clone(), command);
            }
            Err(e) => warnings.push(format!(
                "Failed to load custom command {}: {e:#}",
                path.display()
            )),
        }
    }
}

fn load_markdown_command(
    root: &Path,
    path: &Path,
) -> Result<CustomSlashCommand> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let content = normalize_line_endings(&content);
    let relative = path.strip_prefix(root).with_context(|| {
        format!("{} is not under {}", path.display(), root.display())
    })?;
    let name = command_name_from_relative_path(relative)?;
    validate_command_name(&name)?;

    let (frontmatter, template) = split_frontmatter(&content);
    let mut config = frontmatter
        .map(parse_frontmatter)
        .transpose()?
        .unwrap_or_default();
    config.template = Some(template.trim().to_string());
    command_from_config(name, config)
}

fn command_name_from_relative_path(relative: &Path) -> Result<String> {
    let without_extension = relative.with_extension("");
    let mut parts = Vec::new();
    for component in without_extension.components() {
        let std::path::Component::Normal(part) = component else {
            anyhow::bail!(
                "Custom command path must not contain traversal: {}",
                relative.display()
            );
        };
        let part = part.to_str().context("Command path must be valid UTF-8")?;
        validate_command_segment(part)?;
        parts.push(part);
    }
    if parts.is_empty() {
        anyhow::bail!("Custom command path cannot be empty");
    }
    Ok(parts.join("/"))
}

fn command_from_config(
    name: String,
    config: PartialCommandConfig,
) -> Result<CustomSlashCommand> {
    let template = config
        .template
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .context("Custom command template cannot be empty")?;
    Ok(CustomSlashCommand {
        name,
        template,
        description: config.description.filter(|s| !s.trim().is_empty()),
        agent: config.agent.filter(|s| !s.trim().is_empty()),
        model: config.model.filter(|s| !s.trim().is_empty()),
    })
}

fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let Some(rest) = content.strip_prefix("---\n") else {
        return (None, content);
    };
    let Some(end) = rest.find("\n---") else {
        return (None, content);
    };
    let frontmatter = &rest[..end];
    let mut template = &rest[end + "\n---".len()..];
    if let Some(stripped) = template.strip_prefix('\n') {
        template = stripped;
    }
    (Some(frontmatter), template)
}

fn normalize_line_endings(content: &str) -> String {
    content.replace("\r\n", "\n").replace('\r', "\n")
}

fn parse_frontmatter(raw: &str) -> Result<PartialCommandConfig> {
    let mut config = PartialCommandConfig::default();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            anyhow::bail!(
                "Invalid frontmatter line `{line}`; expected `key: value`"
            );
        };
        let value = unquote(value.trim()).to_string();
        match key.trim() {
            "description" => config.description = Some(value),
            "agent" => config.agent = Some(value),
            "model" => config.model = Some(value),
            "template" => config.template = Some(value),
            _ => {}
        }
    }
    Ok(config)
}

fn unquote(value: &str) -> &str {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return &value[1..value.len() - 1];
        }
    }
    value
}

fn validate_command_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!(
            "Invalid command name `{name}`; command name cannot be empty"
        );
    }
    for segment in name.split('/') {
        validate_command_segment(segment)?;
    }
    Ok(())
}

fn validate_command_segment(segment: &str) -> Result<()> {
    if segment.is_empty()
        || !segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!(
            "Invalid command segment `{segment}`; use letters, numbers, '-' or '_'"
        );
    }
    Ok(())
}

pub async fn expand_custom_command(
    invocation: &CustomCommandInvocation,
    workspace_root: &Path,
) -> Result<String> {
    let args = parse_arguments(&invocation.arguments);
    let with_args = expand_argument_placeholders(
        &invocation.command.template,
        &invocation.arguments,
        &args,
    );
    let with_files = expand_file_references(&with_args, workspace_root).await?;
    expand_shell_output(&with_files, workspace_root).await
}

fn expand_argument_placeholders(
    template: &str,
    raw_arguments: &str,
    args: &[String],
) -> String {
    let mut output = String::new();
    let mut chars = template.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch != '$' {
            output.push(ch);
            continue;
        }
        let remaining = chars.clone().next().map(|(idx, _)| &template[idx..]);
        if remaining.is_some_and(|s| s.starts_with("ARGUMENTS")) {
            for _ in 0.."ARGUMENTS".len() {
                chars.next();
            }
            output.push_str(raw_arguments);
            continue;
        }
        let mut digits = String::new();
        while let Some((_, next)) = chars.peek().copied() {
            if next.is_ascii_digit() {
                digits.push(next);
                chars.next();
            } else {
                break;
            }
        }
        if digits.is_empty() {
            output.push('$');
        } else if let Ok(index) = digits.parse::<usize>()
            && index > 0
            && let Some(arg) = args.get(index - 1)
        {
            output.push_str(arg);
        }
    }
    output
}

fn parse_arguments(raw: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escape = false;

    for ch in raw.chars() {
        if escape {
            current.push(ch);
            escape = false;
            continue;
        }
        if ch == '\\' {
            escape = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if escape {
        current.push('\\');
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

async fn expand_file_references(
    template: &str,
    workspace_root: &Path,
) -> Result<String> {
    let workspace_root = canonicalize_existing_dir(workspace_root)?;
    let mut output = String::new();
    let mut chars = template.char_indices().peekable();

    while let Some((_, ch)) = chars.next() {
        if ch != '@' {
            output.push(ch);
            continue;
        }

        let mut path = String::new();
        while let Some((_, next)) = chars.peek().copied() {
            if is_file_reference_char(next) {
                path.push(next);
                chars.next();
            } else {
                break;
            }
        }

        if path.is_empty() {
            output.push('@');
            continue;
        }

        match read_workspace_file_reference(&workspace_root, &path).await? {
            Some(content) => {
                output.push_str(&format!("@{path}\n```\n{content}\n```"));
            }
            None => {
                output.push('@');
                output.push_str(&path);
            }
        }
    }

    Ok(output)
}

fn canonicalize_existing_dir(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("Failed to resolve {}", path.display()))
}

fn is_file_reference_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(ch, '_' | '-' | '.' | '/' | ':' | '+')
}

async fn read_workspace_file_reference(
    workspace_root: &Path,
    path: &str,
) -> Result<Option<String>> {
    let candidate = workspace_root.join(path);
    if !candidate.exists() || !candidate.is_file() {
        return Ok(None);
    }
    let canonical = candidate.canonicalize().with_context(|| {
        format!("Failed to resolve {}", candidate.display())
    })?;
    if !canonical.starts_with(workspace_root) {
        return Ok(None);
    }
    let content = tokio::fs::read_to_string(&canonical)
        .await
        .with_context(|| format!("Failed to read {}", canonical.display()))?;
    Ok(Some(content))
}

async fn expand_shell_output(
    template: &str,
    workspace_root: &Path,
) -> Result<String> {
    let mut output = String::new();
    let mut rest = template;

    while let Some(start) = rest.find("!`") {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('`') else {
            output.push_str(&rest[start..]);
            return Ok(output);
        };
        let command = &after_start[..end];
        let shell_output = run_shell_capture(command, workspace_root).await?;
        output.push_str(&shell_output);
        rest = &after_start[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

async fn run_shell_capture(
    command: &str,
    workspace_root: &Path,
) -> Result<String> {
    let output = tokio::process::Command::new("/bin/bash")
        .arg("-lc")
        .arg(command)
        .current_dir(workspace_root)
        .output()
        .await
        .with_context(|| format!("Failed to run `{command}`"))?;

    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        if !text.ends_with('\n') && !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("[command exited with {}]", output.status));
    }
    Ok(text.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_markdown_command_with_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.md");
        std::fs::write(
            &path,
            "---\ndescription: Run tests\nagent: build\nmodel: deepseek/chat\n---\nRun $ARGUMENTS",
        )
        .unwrap();

        let command = load_markdown_command(dir.path(), &path).unwrap();

        assert_eq!(command.name, "test");
        assert_eq!(command.description.as_deref(), Some("Run tests"));
        assert_eq!(command.agent.as_deref(), Some("build"));
        assert_eq!(command.model.as_deref(), Some("deepseek/chat"));
        assert_eq!(command.template, "Run $ARGUMENTS");
    }

    #[test]
    fn parses_markdown_command_with_crlf_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.md");
        std::fs::write(
            &path,
            "---\r\ndescription: Run tests\r\nagent: build\r\n---\r\nRun it",
        )
        .unwrap();

        let command = load_markdown_command(dir.path(), &path).unwrap();

        assert_eq!(command.description.as_deref(), Some("Run tests"));
        assert_eq!(command.agent.as_deref(), Some("build"));
        assert_eq!(command.template, "Run it");
    }

    #[test]
    fn loads_nested_command_as_subcommand() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("xxx").join("yyy.md");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, "Run nested").unwrap();

        let command = load_markdown_command(dir.path(), &nested).unwrap();

        assert_eq!(command.name, "xxx/yyy");
        assert_eq!(command.template, "Run nested");
    }

    #[test]
    fn recursively_loads_nested_commands() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("xxx").join("yyy.md");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, "Run nested").unwrap();
        let mut commands = HashMap::new();
        let mut warnings = Vec::new();

        load_command_dir(dir.path(), &mut commands, &mut warnings);

        assert!(warnings.is_empty());
        assert_eq!(commands["xxx/yyy"].template, "Run nested");
    }

    #[test]
    fn expands_arguments_and_quoted_positionals() {
        let args = parse_arguments("Button src 'hello world'");
        let expanded = expand_argument_placeholders(
            "Create $1 in $2 with $3 from $ARGUMENTS",
            "Button src 'hello world'",
            &args,
        );

        assert_eq!(
            expanded,
            "Create Button in src with hello world from Button src 'hello world'"
        );
    }

    #[tokio::test]
    async fn expands_file_references_inside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), "hello").unwrap();

        let expanded = expand_file_references("Read @note.txt", dir.path())
            .await
            .unwrap();

        assert!(expanded.contains("@note.txt\n```\nhello\n```"));
    }

    #[tokio::test]
    async fn does_not_expand_file_references_outside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        let input = "Read @../secret.txt";

        let expanded = expand_file_references(input, workspace.path())
            .await
            .unwrap();

        assert_eq!(expanded, input);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn does_not_expand_symlink_escape() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, "secret").unwrap();
        std::os::unix::fs::symlink(
            &outside_file,
            workspace.path().join("secret-link.txt"),
        )
        .unwrap();

        let expanded =
            expand_file_references("Read @secret-link.txt", workspace.path())
                .await
                .unwrap();

        assert_eq!(expanded, "Read @secret-link.txt");
    }

    #[tokio::test]
    async fn expands_shell_output() {
        let dir = tempfile::tempdir().unwrap();

        let expanded = expand_shell_output("Result: !`printf hi`", dir.path())
            .await
            .unwrap();

        assert_eq!(expanded, "Result: hi");
    }
}
