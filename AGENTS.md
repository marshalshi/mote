# Mote Project — Agent Guidelines

This file defines expectations for AI coding agents contributing to the mote project.

## Code Style

- **Rust edition 2024**. Use idiomatic Rust: `match`, iterators, `Option::map`, `Result::and_then`.
- Use `anyhow::Result` for application-level error handling. Avoid `unwrap()` and `expect()` in production code.
- Format with `rustfmt`. Zero compiler warnings is a hard requirement.

## Architecture

Mote is split into three crates in a workspace:

```
protocol/     # Shared types (ServerEvent, ChatRequest, UiConfig) — serde only, no tokio
server/       # axum daemon: LLM providers, agent loop, tools, config, sessions
client/       # ratatui TUI: event loop, state machine, rendering, keybindings
```

### Server/Client contract

- **HTTP** endpoints (`GET /health`, `GET /config`, `GET /sessions`) for request/response.
- **WebSocket** (`/chat`) for streaming chat — server sends `ServerEvent` JSON messages.
- The `protocol/` crate is the single source of truth for these types — never duplicate them.

### What runs where

| Concern | Runs in |
|---------|---------|
| Config loading, env var expansion | server |
| LLM provider calls (DeepSeek, Ollama) | server |
| Agent loop, tool execution | server |
| Session persistence (disk I/O) | server |
| TUI rendering, keybindings | client |
| User input handling | client |

The client never imports from the server crate or calls LLM providers directly.

## Prompt system

The `prompts/` directory is organized into two subfolders:

```
prompts/
  system/    # shared system prompt files (.md)
  agents/    # built-in agent definitions shipped with the repo (.toml)
```

The system prompt is assembled by `PromptAssembler` (`server/src/prompt.rs`) into
ordered layers, each a separate `ChatMessage::system(...)`. Both the primary
agent and subagents build their layers via `PromptAssembler::for_agent()`.

Layers (in order):
1. Environment block (model id, working dir, git status, platform, date)
2. Shared system prompt (`prompts/system/mote.md` by default). The path is
   configurable via `[prompts].default`.
3. Global `~/.config/mote/AGENTS.md` if present (user's personal policy)
4. Workspace `AGENTS.md` — the repo's `AGENTS.md`, sent by the client
5. Agent-specific instructions — the `instructions` string field on
   `[agents.<name>]`, injected for that agent only
6. Skills — names + descriptions from `~/.config/mote/skills/*/SKILL.md`

A 7th dynamic layer is **not** part of the assembled system prompt: the agent
loop rebuilds a `<system-reminder>` block every turn (`build_system_reminder`)
with the current step, available tools, last turn's tool results, and the most
recent user message.

## Agents

Agents are TOML files (`<name>.toml`) containing an `AgentConfig`: `mode`,
`temperature`, `instructions`, `permissions`, and optional `model` /
`max_tokens` overrides. When `model` is omitted, the agent uses the default
model from `[model]` in `config.toml`.

Agent files are loaded from two locations (later sources override earlier on
name collision):
1. Built-in agents shipped in the repo: `prompts/agents/*.toml`
2. User agents: `~/.config/mote/agents/*.toml`

Config.toml `[agents.<name>]` entries override file-based agents on collision.

The default agent (used when no agent is specified) is `build` by default,
defined in the repo by `prompts/agents/build.toml`. The fallback agent name is
configurable via `server.default_agent` in `config.toml`; when omitted it
defaults to `build` via `marshaling_protocol::DEFAULT_AGENT_NAME`.

## Testing

- `cargo test -p mote-server` — config, prompt, session, tools, llm tests
- `cargo test -p mote-client` — state machine, keybindings, suggestions tests
- Tests that spawn external tools (`rg`, `bash`) require those tools to be on `$PATH`.

## Config

- `config.toml` and `keybindings.toml` live in `~/.config/mote/`. Templates are at the repo root.
- The server loads `config.toml` for providers, prompts, and history.
- The client loads `keybindings.toml` locally and fetches UI settings from the server via `GET /config`.
- System prompts live in `prompts/system/`; built-in agent definitions live in `prompts/agents/`.
- `[prompts].default` points to the single shared system prompt file, which defaults to `prompts/system/mote.md`.
- `server.default_agent` controls which agent is used when a request omits an agent name, and which `/model <default-agent>` alias is reserved.

## Dependencies

- **axum** for HTTP + WebSocket (server).
- **ratatui + crossterm** for TUI (client).
- **tokio** for async everywhere.
- **reqwest** for HTTP calls in both server and client.
- No runtime bridging — everything runs on tokio.

## Safety

- Never commit API keys or secrets.
- Do not rewrite git history.
- Ask before destructive operations (deleting files, refactoring large sections).
