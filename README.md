# Mote

A Rust AI coding assistant — split-server architecture with a TUI client.

## Quick start

### Prerequisites
- Rust 1.75+
- [ripgrep](https://github.com/BurntSushi/ripgrep) (`rg`) — used by the built-in `grep` tool
- [Ollama](https://ollama.com) with a model, or a DeepSeek API key

### Configuration

Config files live in `~/.config/mote/`:

```bash
mkdir -p ~/.config/mote
cp config.toml.example ~/.config/mote/config.toml
cp keybindings.toml.example ~/.config/mote/keybindings.toml
```

#### Auth (API keys, tokens)

Secrets are stored separately in `~/.config/mote/auth.json`:

```bash
cp auth.json.example ~/.config/mote/auth.json
```

Or use the `--login` CLI flag to set them interactively:

```bash
# DeepSeek — enter your API key
cargo run -p mote-client -- --login deepseek

# GitHub — enter a Personal Access Token with models:read scope
cargo run -p mote-client -- --login github
```

### Run

Start the **server**, then the **client**:

```bash
# Terminal 1: server daemon
cargo run -p mote-server

# Terminal 2: TUI client
cargo run -p mote-client
```

The server binds to `127.0.0.1:9847`. The client auto-detects it.

## Usage

### Keybindings

Default keys (configurable via `keybindings.toml`):

| Action | Default key | Config key |
|--------|-------------|------------|
| Send message | `enter` | `send_message` |
| Newline | `alt+enter` | `insert_newline` |
| Quit | `esc`, `ctrl+c` | `quit` |
| Cursor left/right | `left`/`right` | `cursor_left`/`cursor_right` |
| Cursor home/end | `home`/`end` | `cursor_home`/`cursor_end` |
| Delete before/after | `backspace`/`delete` | `delete_before`/`delete_after` |
| History up/down | `up`/`down` | `history_up`/`history_down` |
| Scroll up | `pageup`, `ctrl+up` | `scroll_up` |
| Scroll down | `pagedown`, `ctrl+down` | `scroll_down` |
| Scroll to bottom | `ctrl+end` | `scroll_to_bottom` |
| Slash command | `ctrl+p` | `agent_command` |
| Complete / Tab | `tab` | `complete` |
| Switch agent view | `F5` | `switch_view` |
| Cancel agent | `Esc` (during streaming) | `cancel_agent` |

Customize via `~/.config/mote/keybindings.toml`:

```toml
send_message = "enter"
quit = ["esc", "ctrl+q"]
agent_command = "ctrl+space"
```

### Slash commands

| Command | Description |
|---------|-------------|
| `/help` | Show help |
| `/agent` | List / switch agents |
| `/model` | Show / switch model |
| `/tokens` | Show token usage |
| `/session list` | List saved sessions |
| `/session delete <id>` | Delete a session |
| `/session info` | Show current session info |
| `/login github` | GitHub OAuth device flow |
| `/login deepseek <key>` | Save DeepSeek API key |
| `/subagents` | List active subagents |
| `/rollback last` | Roll back latest tracked file changes |

### Agents

Define agents as TOML files in `~/.config/mote/agents/`:

```toml
# ~/.config/mote/agents/plan.toml
model = "deepseek/deepseek-v4-flash"
mode = "primary"
temperature = 0.1
instructions = "You are a planning agent."

[permissions]
read = "allow"
write = "allow"
edit = "ask"
```

Agent modes control visibility:

| Mode | `/agent` list | Subagent target |
|------|---------------|-----------------|
| `primary` | ✅ Yes | ❌ No |
| `subagent` | ❌ No | ✅ Yes |
| `all` | ✅ Yes | ✅ Yes |

Default mode is `primary`.

### Skills

Place skill folders in `~/.config/mote/skills/`. Each skill is a folder with a `SKILL.md` file containing YAML frontmatter:

```markdown
---
name: python-conventions
description: Use when working on Python files, pytest, uv, .venv, or Python project conventions.
---

- Follow PEP 8 and clear naming.
- Prefer type hints for non-trivial code.
- Use `pytest` for tests.
```

Skills are injected into the system prompt. Use the `use_skill` tool to activate them.

### Permission system

Each agent has per-tool permissions in its agent TOML:

| Level | Behavior |
|-------|----------|
| `allow` | Tool runs automatically |
| `ask` | TUI prompts for Y/N approval |
| `deny` | Tool is blocked |

Tools: `read`, `glob`, `grep`, `write`, `edit`, `bash`, `subagent`. `use_skill` is always allowed.

`delete` is available as a built-in file tool (file-only in v1) and should usually be configured as `ask`.

Permission prompts support three choices:
- `[Y] Allow Once`
- `[A] Allow Always` (session-scoped, with confirmation)
- `[N] Deny`

### File change diff display and rollback

For successful file-mutating tool calls (`write`, `edit`, `delete`), Mote shows a git-diff-like summary:
- modified files: `-` removed lines and `+` added lines
- new file: alert line (`! new file added: ...`)
- removed file: alert line (`! file removed: ...`)

Roll back the latest tracked change-set with:

```bash
/rollback last
```

Rollback is conflict-safe: if files changed since the original mutation, rollback is blocked with an explanatory message.

### Subagents

Agents can delegate to other agents via the `subagent` tool:

```
subagent(agent="review", task="Check this code for bugs")
```

Each subagent gets its own background "screen". Press **F5** to cycle through views:
- Primary agent → Subagent 1 → Subagent 2 → ... → Primary agent

The status bar shows which subagent you're viewing (`Sub: review (1/2) running`).

Type `/subagents` to list all active subagents with their status and see which one is currently selected.

When a subagent finishes, its output is automatically added to the primary conversation as an assistant message, so you don't lose context. You can still view the subagent's full screen (with tool calls and reasoning) via F5.

Recursion is limited to 3 levels.

### Debug logging

```bash
# Server: set RUST_LOG=debug for verbose output
RUST_LOG=debug cargo run -p mote-server

# Client: use -v or RUST_LOG=debug
cargo run -p mote-client -- -v
```

## Architecture

Mote is split into a **server** (LLM providers, agent loop, tool execution) and a **client** (TUI, keybindings, rendering), communicating via WebSocket + HTTP.

```
mote/
├── Cargo.toml              # workspace: [protocol, server, client]
├── protocol/               # shared types (ServerEvent, ChatRequest)
├── server/                 # axum HTTP + WS daemon
│   ├── main.rs             # routes, WS handler, cancel guard
│   ├── auth.rs             # auth.json loader
│   ├── config.rs           # config.toml loading
│   ├── prompt.rs           # prompt assembly (6 layers)
│   ├── agent.rs            # agent run loop
│   ├── tools.rs            # built-in tools
│   ├── session.rs          # session persistence
│   ├── history.rs          # markdown history writer
│   └── llm/                # provider implementations
│       ├── mod.rs          # provider factory
│       ├── deepseek.rs     # DeepSeek API
│       ├── ollama.rs       # Ollama (local)
│       └── github.rs       # GitHub Models
└── client/                 # TUI + CLI
    ├── main.rs             # entry point
    ├── client.rs           # WebSocket client + chat stream
    ├── config.rs           # keybinding loading
    ├── llm.rs              # Role enum
    └── tui/
        ├── mod.rs          # event loop
        ├── state.rs        # app state machine
        ├── render.rs       # ratatui rendering
        └── keybinding.rs   # key action mapping
```

### Server endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/health` | Health check |
| `GET` | `/config` | UI settings (accent colors, agent names, model info) |
| `GET` | `/models` | Available models from all providers |
| `GET` | `/sessions` | Saved session list |
| `POST` | `/rollback/last` | Roll back latest tracked file changes |
| `WS` | `/chat` | Streaming chat |

### Data flow

```
Terminal 2 (client)              Terminal 1 (server)
┌─────────────────────────┐      ┌──────────────────────────┐
│ TUI event loop          │      │                          │
│   → user types + Enter  │  WS  │   axum /chat             │
│   → client.chat_stream()│─────►│   → agent::run_loop()    │
│   → recv ServerEvent    │◄─────│   → sends ServerEvent    │
│   → update App state    │ JSON │     via WebSocket        │
│   → render frame        │      │                          │
└─────────────────────────┘      └──────────────────────────┘
```

The client never calls LLM providers directly — all agent logic runs server-side.

### Prompt assembly (6 layers)

1. **Environment** — model name, working directory, platform, date
2. **Provider prompt** — `prompts/<provider>.txt`, falls back to `prompts/default.txt`
3. **Agent instructions** — from agent TOML's `instructions` field
4. **User AGENTS.md** — `~/.config/mote/AGENTS.md` (optional)
5. **Skills** — `~/.config/mote/skills/` (optional)
6. **Dynamic system reminder** — generated fresh each turn with time, progress, tool results, and guidance

## Tests

```bash
cargo test                     # all crates
cargo test -p mote-server
cargo test -p mote-client
```

Target: **zero warnings** across the workspace.
