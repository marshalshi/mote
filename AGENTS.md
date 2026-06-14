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

Layers (in order):
1. Environment block (model, working dir, platform, date)
2. Default prompt (`prompts/default.txt`)
3. Model-specific prompt (`prompts/<provider>.txt`)
4. User instructions from `prompts/instructions/` (markdown files)
5. `~/.config/mote/AGENTS.md` if present (user's personal policy)

## Testing

- `cargo test -p mote-server` — 49 tests (config, prompt, session, tools, llm)
- `cargo test -p mote-client` — 33 tests (state machine, keybindings, suggestions)
- Tests that spawn external tools (`rg`, `bash`) require those tools to be on `$PATH`.

## Config

- `config.toml` and `keybindings.toml` live in `~/.config/mote/`. Templates are at the repo root.
- The server loads `config.toml` for providers, prompts, and history.
- The client loads `keybindings.toml` locally and fetches UI settings from the server via `GET /config`.

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
