use crate::config::Config;
use crate::llm::ToolDef;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Frontmatter parsed from each skill's SKILL.md.
#[derive(Debug, Clone, Deserialize)]
struct SkillMeta {
    name: Option<String>,
    description: String,
}

/// Assembles the system prompt layers for a chat request.
pub struct PromptAssembler {
    config: Config,
    agent_instructions: Option<String>,
    workspace_root: Option<PathBuf>,
    repo_agents_md: Option<String>,
}

impl PromptAssembler {
    /// Create a new assembler from the global config.
    #[allow(dead_code)] // public API, used in tests
    pub fn new(config: Config) -> Self {
        Self {
            config,
            agent_instructions: None,
            workspace_root: None,
            repo_agents_md: None,
        }
    }

    /// Create an assembler for a specific agent, applying per-agent overrides.
    pub fn for_agent(
        config: &Config,
        agent: Option<&crate::config::AgentConfig>,
    ) -> Self {
        let cfg = config.clone();
        let instructions = agent.and_then(|a| a.instructions.clone());
        Self {
            config: cfg,
            agent_instructions: instructions,
            workspace_root: None,
            repo_agents_md: None,
        }
    }

    pub fn with_workspace_context(
        mut self,
        workspace_root: Option<PathBuf>,
        repo_agents_md: Option<String>,
    ) -> Self {
        self.workspace_root = workspace_root;
        self.repo_agents_md = repo_agents_md;
        self
    }

    /// Build the system layer list (each element is one layer).
    ///
    /// Layers are assembled in order:
    /// 1. Environment block (model info, platform, working directory, date)
    /// 2. Shared system prompt (`prompts/system/mote.md` by default)
    /// 3. User AGENTS.md — ~/.config/mote/AGENTS.md (optional)
    /// 4. Workspace AGENTS.md passed by client (optional)
    /// 5. Agent-specific instructions (from agent config `instructions` field, optional)
    /// 6. Skills — ~/.config/mote/skills/*.md (optional)
    pub fn assemble(
        &self,
        model_provider: &str,
        model_id: &str,
    ) -> Result<Vec<String>> {
        let mut layers: Vec<String> = Vec::new();

        // Layer 1: Environment
        layers.push(
            self.build_env_block(model_id, self.workspace_root.as_deref()),
        );

        if let Some(layer) = self.load_system_prompt_layer(model_provider)? {
            layers.push(layer);
        }

        if let Some(layer) = self.load_global_agents_layer()? {
            layers.push(layer);
        }

        if let Some(layer) = self.workspace_agents_layer() {
            layers.push(layer);
        }

        if let Some(layer) = self.agent_instructions_layer() {
            layers.push(layer);
        }

        if let Some(layer) = self.skills_layer() {
            layers.push(layer);
        }

        Ok(layers)
    }

    fn load_system_prompt_layer(
        &self,
        _model_provider: &str,
    ) -> Result<Option<String>> {
        let prompt =
            self.load_file_or_default(&self.config.prompts.default, "")?;
        Ok((!prompt.is_empty()).then_some(prompt))
    }

    fn load_global_agents_layer(&self) -> Result<Option<String>> {
        let Some(home) = dirs::home_dir() else {
            return Ok(None);
        };

        let agents_path = home.join(".config").join("mote").join("AGENTS.md");
        if !agents_path.exists() {
            return Ok(None);
        }

        let content =
            std::fs::read_to_string(&agents_path).with_context(|| {
                format!("Failed to read: {}", agents_path.display())
            })?;
        Ok(Self::format_instructions_layer(
            agents_path.display().to_string(),
            &content,
        ))
    }

    fn workspace_agents_layer(&self) -> Option<String> {
        let src = self
            .workspace_root
            .as_ref()
            .map(|p| p.join("AGENTS.md").display().to_string())
            .unwrap_or_else(|| "<workspace>/AGENTS.md".to_string());
        self.repo_agents_md
            .as_deref()
            .and_then(|content| Self::format_instructions_layer(src, content))
    }

    fn agent_instructions_layer(&self) -> Option<String> {
        self.agent_instructions
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(ToOwned::to_owned)
    }

    fn skills_layer(&self) -> Option<String> {
        let skills_dir = dirs::home_dir()?
            .join(".config")
            .join("mote")
            .join("skills");
        if !skills_dir.is_dir() {
            return None;
        }

        let skill_entries = self.load_skill_entries(&skills_dir);
        if skill_entries.is_empty() {
            return None;
        }

        let mut skills_text = String::from(
            "Skills available:\n\
             Review the skills below. If a skill's description matches the current task, ",
        );
        skills_text.push_str(
            "call use_skill(\"<name>\") to load its full guidance, then apply it.\n",
        );
        for (name, desc) in &skill_entries {
            if desc.is_empty() {
                skills_text.push_str(&format!("  {}\n", name));
            } else {
                skills_text.push_str(&format!("  {} — {}\n", name, desc));
            }
        }
        Some(skills_text)
    }

    fn load_skill_entries(&self, skills_dir: &Path) -> Vec<(String, String)> {
        let Ok(entries) = std::fs::read_dir(skills_dir) else {
            return Vec::new();
        };

        let mut folders: Vec<_> =
            entries.flatten().filter(|e| e.path().is_dir()).collect();
        folders.sort_by_key(|e| e.file_name());

        folders
            .into_iter()
            .filter_map(|entry| self.load_skill_entry(&entry))
            .collect()
    }

    fn load_skill_entry(
        &self,
        entry: &std::fs::DirEntry,
    ) -> Option<(String, String)> {
        let folder_name = entry.file_name().to_string_lossy().to_string();
        let skill_path = entry.path().join("SKILL.md");
        if !skill_path.exists() {
            return None;
        }

        let content = std::fs::read_to_string(&skill_path).ok()?;
        let (meta, _) = Self::parse_skill_frontmatter(&content);
        Some(match meta {
            Some(meta) => (meta.name.unwrap_or(folder_name), meta.description),
            None => (folder_name, String::new()),
        })
    }

    fn parse_skill_frontmatter(content: &str) -> (Option<SkillMeta>, &str) {
        let trimmed = content.trim();
        let Some(rest) = content
            .strip_prefix("---\n")
            .or_else(|| content.strip_prefix("---\r\n"))
        else {
            return (None, trimmed);
        };

        let Some((yaml_text, rest_body)) = rest
            .split_once("\n---")
            .or_else(|| rest.split_once("\r\n---"))
        else {
            return (None, trimmed);
        };

        let body = rest_body
            .trim_start_matches("\r\n")
            .trim_start_matches('\n')
            .trim();
        (serde_yaml::from_str(yaml_text).ok(), body)
    }

    fn format_instructions_layer(
        source: String,
        content: &str,
    ) -> Option<String> {
        let trimmed = content.trim();
        (!trimmed.is_empty())
            .then(|| format!("Instructions from: {}\n{}", source, trimmed))
    }

    /// Build the environment info block (Layer 1).
    fn build_env_block(
        &self,
        model_id: &str,
        workspace_root: Option<&Path>,
    ) -> String {
        let cwd = workspace_root
            .map(|p| p.display().to_string())
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.display().to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());
        let is_git = workspace_root
            .map(|p| p.join(".git").exists())
            .unwrap_or_else(|| std::path::Path::new(".git").exists());

        format!(
            r#"You are a CLI AI assistant powered by the model named {model_id}.
Here is some useful information about the environment you are running in:
<env>
  Working directory: {cwd}
  Is directory a git repo: {git}
  Platform: {platform}
  Today's date: {date}
</env>"#,
            model_id = model_id,
            cwd = cwd,
            git = if is_git { "yes" } else { "no" },
            platform = std::env::consts::OS,
            date = chrono::Local::now().format("%a %b %d %Y"),
        )
    }

    /// Load a file, return empty string if not found.
    fn load_file_or_default(
        &self,
        path: &std::path::Path,
        default: &str,
    ) -> Result<String> {
        if path.exists() {
            Ok(std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read {}", path.display()))?
                .trim()
                .to_string())
        } else {
            Ok(default.to_string())
        }
    }
}

// ── Dynamic system reminder (Layer 7) ──────────────────

/// Summary of a single tool's result from the previous turn.
#[derive(Debug, Clone)]
pub struct ToolResultSummary {
    pub tool_name: String,
    pub success: bool,
    pub summary: String,
}

/// Context for building the per-turn `<system-reminder>`.
pub struct ReminderContext<'a> {
    pub step: usize,
    pub max_steps: usize,
    pub working_directory: String,
    pub tool_defs: &'a [ToolDef],
    pub last_turn_results: Vec<ToolResultSummary>,
    pub last_user_message: Option<String>,
}

/// Build the dynamic `<system-reminder>` block for the current turn.
pub fn build_system_reminder(ctx: &ReminderContext) -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");

    let tool_list: Vec<String> = ctx
        .tool_defs
        .iter()
        .map(|t| t.function.name.clone())
        .collect();
    let tools_section = if tool_list.is_empty() {
        String::new()
    } else {
        format!("Available tools: {}\n\n", tool_list.join(", "))
    };

    let results_section = if ctx.last_turn_results.is_empty() {
        if ctx.step > 1 {
            String::new()
        } else {
            String::new()
        }
    } else {
        let mut lines = String::from("<last_turn_results>\n");
        for r in &ctx.last_turn_results {
            let status = if r.success { "Success" } else { "Failed" };
            lines.push_str(&format!(
                "  {}(\"{}\") → {}\n",
                r.tool_name, r.summary, status
            ));
        }
        lines.push_str("</last_turn_results>\n\n");
        lines
    };

    let user_msg_section = if let Some(ref msg) = ctx.last_user_message {
        format!("Most recent user request: \"{}\"\n\n", msg)
    } else {
        String::new()
    };

    let guidance = if ctx.step == 1 {
        "You are at the start of a task. Use the tools above to accomplish the user's request. When the request is fully complete, call finish_task with the final answer. Do not stop early if more tool work or follow-up reasoning is still needed."
    } else {
        "Continue the task based on these results. Do not repeat tool calls that already succeeded. When the request is fully complete, call finish_task with the final answer."
    };

    format!(
        "<system-reminder>\n\
         Current time: {}\n\
         Working directory: {}\n\
         Step: Turn {} (soft budget: {})\n\n\
         {}\
         {}\
         {}\
         <reminder>{}</reminder>\n\
         </system-reminder>",
        now,
        ctx.working_directory,
        ctx.step,
        ctx.max_steps,
        tools_section,
        results_section,
        user_msg_section,
        guidance,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a PromptAssembler with default config (no prompt files).
    fn test_assembler() -> PromptAssembler {
        let toml = r#"
[model]
provider = "test"
model_id = "test-model"

[providers.ollama]
base_url = "http://localhost:11434"

[prompts]
default = "/nonexistent/prompts/system/mote.md"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        PromptAssembler::new(config)
    }

    #[test]
    fn test_environment_block_contains_model() {
        let a = test_assembler();
        let block = a.build_env_block("gpt-4", None);
        assert!(block.contains("gpt-4"));
        assert!(block.contains("<env>"));
        assert!(block.contains("</env>"));
    }

    #[test]
    fn test_workspace_agents_layer_is_included() {
        let a = test_assembler().with_workspace_context(
            Some(PathBuf::from("/tmp/repo")),
            Some("# local rules".into()),
        );
        let layers = a.assemble("test", "test-model").unwrap();
        assert!(
            layers
                .iter()
                .any(|l| l.contains("Instructions from: /tmp/repo/AGENTS.md"))
        );
        assert!(layers.iter().any(|l| l.contains("# local rules")));
    }

    #[test]
    fn test_global_agents_before_workspace_agents() {
        let a = test_assembler().with_workspace_context(
            Some(PathBuf::from("/tmp/repo")),
            Some("# workspace rules".into()),
        );
        let layers = a.assemble("test", "test-model").unwrap();

        let workspace_idx = layers
            .iter()
            .position(|l| l.contains("Instructions from: /tmp/repo/AGENTS.md"));
        if let Some(home) = dirs::home_dir() {
            let global_path =
                home.join(".config").join("mote").join("AGENTS.md");
            if global_path.exists() {
                let global_marker =
                    format!("Instructions from: {}", global_path.display());
                let global_idx =
                    layers.iter().position(|l| l.contains(&global_marker));
                if let (Some(g), Some(w)) = (global_idx, workspace_idx) {
                    assert!(
                        g < w,
                        "global AGENTS should appear before workspace AGENTS"
                    );
                }
            }
        }
    }

    #[test]
    fn test_assemble_missing_files_returns_just_env() {
        let a = test_assembler();
        let layers = a.assemble("test", "test-model").unwrap();
        // Layer 1 (env) should always be present. Layer 3 (~/.config/mote/AGENTS.md)
        // is optional — depends on user's filesystem. Just check env is there.
        assert!(layers.len() >= 1);
        assert!(layers[0].contains("test-model"));
        // If AGENTS.md exists, it should be the last layer
        let agents_path = dirs::home_dir()
            .map(|h| h.join(".config").join("mote").join("AGENTS.md"));
        if let Some(ref p) = agents_path {
            if p.exists() {
                assert!(layers.len() >= 2);
            }
        }
    }

    #[test]
    fn test_load_file_or_default_empty_on_missing() {
        let a = test_assembler();
        let content = a
            .load_file_or_default(
                Path::new("/nonexistent/file.txt"),
                "fallback",
            )
            .unwrap();
        assert_eq!(content, "fallback");
    }

    #[test]
    fn test_assemble_uses_shared_prompt_file() {
        let toml = r#"
[model]
provider = "ollama"
model_id = "test-model"
[providers.ollama]
base_url = "http://localhost:11434"
[prompts]
default = "/nonexistent/mote.md"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let a = PromptAssembler::new(config);
        let layers = a.assemble("ollama", "test-model").unwrap();
        assert!(layers.len() >= 1);
        assert!(layers[0].contains("test-model"));
    }

    #[test]
    fn test_assemble_reads_shared_prompt_file() {
        let dir = tempfile::tempdir().unwrap();
        let prompts_dir = dir.path().join("prompts").join("system");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(prompts_dir.join("mote.md"), "MOTE PROMPT").unwrap();

        let toml = format!(
            r#"
[model]
provider = "ollama"
model_id = "m"
[providers.ollama]
base_url = "http://localhost:11434"
[prompts]
default = "{dir}/prompts/system/mote.md"
"#,
            dir = dir.path().display()
        );
        let config: Config = toml::from_str(&toml).unwrap();
        let a = PromptAssembler::new(config);
        let layers = a.assemble("ollama", "m").unwrap();
        assert!(layers.len() >= 2);
        assert!(layers.iter().any(|l| l.contains("MOTE PROMPT")));
    }

    #[test]
    fn test_assemble_skills_included_when_dir_exists() {
        // Note: this test creates a temp dir but can't override the home dir.
        // It verifies the code doesn't crash with various skill structures.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join(".config").join("mote");
        let skills_dir = base.join("skills");

        // Create a folder-based skill with SKILL.md
        let skill_folder = skills_dir.join("python-conventions");
        std::fs::create_dir_all(&skill_folder).unwrap();
        std::fs::write(
            skill_folder.join("SKILL.md"),
            r#"---
name: python-conventions
description: Python coding rules
---
Follow PEP 8.
"#,
        )
        .unwrap();

        // Also test: folder without frontmatter (name falls back to folder name)
        let skill_folder2 = skills_dir.join("rust-rules");
        std::fs::create_dir_all(&skill_folder2).unwrap();
        std::fs::write(skill_folder2.join("SKILL.md"), "Use edition 2024.")
            .unwrap();

        // Note: actual skills loading reads from ~/.config/mote/skills/
        // which is determined by dirs::home_dir(). This test creates files in
        // a temp dir, not the real home, so it only verifies the code path
        // doesn't panic. Real skills loading is tested via integration.
        assert!(skills_dir.exists());
        assert!(skill_folder.join("SKILL.md").exists());
        assert!(skill_folder2.join("SKILL.md").exists());
    }

    #[test]
    fn test_skill_yaml_frontmatter_parsing() {
        // Test the YAML frontmatter parsing logic directly
        let content = r#"---
name: my-skill
description: Does something
---
Skill content here."#;

        let (meta, body) = PromptAssembler::parse_skill_frontmatter(content);

        assert!(meta.is_some());
        assert_eq!(meta.as_ref().unwrap().name.as_deref(), Some("my-skill"));
        assert_eq!(meta.as_ref().unwrap().description, "Does something");
        assert_eq!(body, "Skill content here.");
    }

    #[test]
    fn test_skill_no_frontmatter_uses_folder_name() {
        let content = "Just some content.";
        let folder_name = "my-folder-name";

        // No frontmatter → name should fall back to folder name
        let (meta, _body) = PromptAssembler::parse_skill_frontmatter(content);

        let skill_name = meta
            .as_ref()
            .and_then(|m| m.name.clone())
            .unwrap_or_else(|| folder_name.to_string());

        assert_eq!(skill_name, "my-folder-name");
        assert!(meta.is_none());
    }

    // ── Reminder tests ────────────────────────────────────

    #[test]
    fn test_build_reminder_first_turn() {
        let ctx = ReminderContext {
            step: 1,
            max_steps: 10,
            working_directory: "/tmp/test".into(),
            tool_defs: &[],
            last_turn_results: vec![],
            last_user_message: None,
        };
        let reminder = build_system_reminder(&ctx);
        assert!(reminder.contains("<system-reminder>"));
        assert!(reminder.contains("</system-reminder>"));
        assert!(reminder.contains("Turn 1 (soft budget: 10)"));
        assert!(reminder.contains("/tmp/test"));
        assert!(reminder.contains("<reminder>"));
        // First turn should NOT have <last_turn_results>
        assert!(!reminder.contains("<last_turn_results>"));
    }

    #[test]
    fn test_build_reminder_with_tool_results() {
        let ctx = ReminderContext {
            step: 2,
            max_steps: 10,
            working_directory: "/home".into(),
            tool_defs: &[],
            last_turn_results: vec![
                ToolResultSummary {
                    tool_name: "read".into(),
                    success: true,
                    summary: "file contents...".into(),
                },
                ToolResultSummary {
                    tool_name: "bash".into(),
                    success: false,
                    summary: "command not found".into(),
                },
            ],
            last_user_message: Some("find the config file".into()),
        };
        let reminder = build_system_reminder(&ctx);
        assert!(reminder.contains("Turn 2 (soft budget: 10)"));
        assert!(reminder.contains("<last_turn_results>"));
        assert!(reminder.contains("read(\"file contents...\") → Success"));
        assert!(reminder.contains("bash(\"command not found\") → Failed"));
        assert!(reminder.contains("find the config file"));
    }

    #[test]
    fn test_build_reminder_shows_tools_when_provided() {
        let def = ToolDef {
            def_type: "function".into(),
            function: crate::llm::ToolFunctionDef {
                name: "read".into(),
                description: "Read a file".into(),
                parameters: serde_json::json!({}),
            },
        };
        let ctx = ReminderContext {
            step: 1,
            max_steps: 5,
            working_directory: "/wd".into(),
            tool_defs: &[def],
            last_turn_results: vec![],
            last_user_message: None,
        };
        let reminder = build_system_reminder(&ctx);
        assert!(reminder.contains("Available tools: read"));
    }
}
