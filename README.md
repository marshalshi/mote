# Mote

A Rust AI coding assistant with a local server and a terminal UI.

## Highlights

- Build the client/server once, then run the compiled binaries directly.
- Works with local Ollama models or remote providers like DeepSeek, GLM/Z.ai, Kimi, and MiniMax.
- Supports agents, subagents, skills, session history, and rollback.
- Each agent gets a stable, distinct terminal color within the current app session.
- Keeps the terminal workflow lightweight and keyboard-driven.

## Quick start

### Requirements
- Rust 1.75+
- [ripgrep](https://github.com/BurntSushi/ripgrep) (`rg`) — preferred by the built-in `grep` tool when available; otherwise falls back to `grep`
- [Ollama](https://ollama.com) with a model, or an API key for DeepSeek, GLM/Z.ai, Kimi, or MiniMax

### First run

### Build

Build debug binaries:

```bash
cargo build --workspace
```

Build optimized release binaries:

```bash
cargo build --workspace --release
```

Compiled binaries:

- TUI: `./target/debug/mote-tui` or `./target/release/mote-tui`
- Server: `./target/debug/mote-server` or `./target/release/mote-server`

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
# Choose a provider interactively (debug build)
./target/debug/mote-tui --login

# Choose a provider interactively (release build)
./target/release/mote-tui --login

# Or specify one directly
./target/debug/mote-tui --login deepseek
./target/debug/mote-tui --login glm
./target/debug/mote-tui --login kimi
./target/debug/mote-tui --login minimax

./target/release/mote-tui --login deepseek
./target/release/mote-tui --login glm
./target/release/mote-tui --login kimi
./target/release/mote-tui --login minimax
```

### Run

Start Mote with the compiled client binary:

```bash
# Starts a local server in the background, then shows the TUI (debug build)
./target/debug/mote-tui

# Starts a local server in the background, then shows the TUI (release build)
./target/release/mote-tui

# Standalone server (debug build)
./target/debug/mote-server

# Standalone server (release build)
./target/release/mote-server

# TUI-only mode, connecting to an existing server (debug build)
./target/debug/mote-tui --tui --server-url http://127.0.0.1:9847

# TUI-only mode, connecting to an existing server (release build)
./target/release/mote-tui --tui --server-url http://127.0.0.1:9847

# Optional: force a specific session key namespace (debug build)
./target/debug/mote-tui --session-key team-a

# Optional: force a specific session key namespace (release build)
./target/release/mote-tui --session-key team-a
```

`mote-tui` starts a local server on a free localhost port by default and shows the TUI frontend. Use `mote-server` when you want to run the server separately, and `mote-tui --tui --server-url http://127.0.0.1:<port>` to connect the TUI to an already-running server.

### Runtime folders

- Logs are written to `~/.config/mote/logs/mote.log` (server and client verbose mode).
- History is stored under `~/.config/mote/history/` by default.
- In multi-client mode, history is partitioned by client runtime session key:
  - `~/.config/mote/history/<session-key>/*.md`

## Usage

### Keybindings

Default keys (configurable via `keybindings.toml`):

| Action | Default key | Config key |
|--------|-------------|------------|
| Send message | `enter` | `send_message` |
| Newline | `alt+enter` | `insert_newline` |
| Quit | `ctrl+c` | `quit` |
| Cursor left/right | `left`/`right` | `cursor_left`/`cursor_right` |
| Cursor home/end | `home`, `ctrl+a` / `end`, `ctrl+e` | `cursor_home`/`cursor_end` |
| Delete before/after | `backspace` / `delete`, `ctrl+d` | `delete_before`/`delete_after` |
| Clear line | `ctrl+k` | `kill_line` |
| History up/down | `up`/`down` | `history_up`/`history_down` |
| Scroll up | `pageup`, `ctrl+up` | `scroll_up` |
| Scroll down | `pagedown`, `ctrl+down` | `scroll_down` |
| Scroll to bottom | `ctrl+end` | `scroll_to_bottom` |
| Slash command | `ctrl+p` | `agent_command` |
| Cycle agent / complete | `tab` | `complete` |
| Switch agent view | `F5` | `switch_view` |
| Cancel agent | `Esc` (during streaming) | `cancel_agent` |

Customize via `~/.config/mote/keybindings.toml`:

```toml
send_message = "enter"
quit = ["ctrl+c", "ctrl+q"]
agent_command = "ctrl+space"
```

### Slash commands

| Command | Description |
|---------|-------------|
| `/help` | Show help |
| `/agent` | List / switch agents |
| `/model` | Open model picker popup (↑/↓, Enter, Esc) |
| `/compact` | Compact older conversation context into a persisted summary |
| `/tokens` | Show token usage |
| `/new` | Start a fresh chat session |
| `/sessions` | Open session picker (↑/↓, Enter, Esc) |
| `/login` | Show login provider choices |
| `/login <provider> <key>` | Save provider API key (`deepseek`, `glm`, `kimi`, `minimax`) |
| `/subagents` | List active subagents |
| `/rollback last` | Roll back latest tracked file changes |
| `/<custom>` | Run a user-defined custom prompt command |
| `! <command>` | Run a local shell command in the current workspace |

Notes:
- `/sessions` is available only when the app is idle (not during an active streaming turn).
- `/new` clears the current transcript and active resumed session id, while keeping your selected agent/model/workspace.
- `/model <provider/model>` still works, but `/model` opens the picker for easier provider/model selection.
- `/compact` summarizes older conversation turns using the current effective agent model. The visible transcript remains, but already-compacted turns are replaced by the summary when future requests are sent to the LLM.
- When the local conversation context gets large, mote asks before auto-compacting. If you decline, mote continues and warns that the model may lose older context or hit token limits.
- Compaction state is saved with sessions and restored when a session is resumed.
- `!` commands are local TUI commands; their output is shown in the transcript but is not sent to the LLM as conversation history.

### Custom slash commands

Like opencode, mote can load custom slash commands from Markdown files. Put command files in:

- Global: `~/.config/mote/commands/`

The Markdown file name becomes the slash command name. For example, `~/.config/mote/commands/test.md` creates `/test`:

```markdown
---
description: Run tests with coverage
agent: build
model: deepseek/deepseek-v4-flash
---

Run the full test suite with coverage. Focus on failures and suggest fixes.
```

Nested folders become slash subcommands. For example:

```text
~/.config/mote/commands/review/staged.md
```

becomes:

```text
/review/staged
```

Supported prompt placeholders mirror opencode's common command behavior:

- `$ARGUMENTS` expands to everything after the command, e.g. `/component Button` → `Button`.
- `$1`, `$2`, ... expand to positional arguments; quoted strings stay grouped.
- `` !`command` `` runs a shell command from the workspace root and inserts stdout/stderr.
- `@path/to/file` inserts file contents for files inside the workspace.

If a custom command has the same name as a built-in command, the custom command wins.

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
glob = "allow"
grep = "allow"
write = "ask"
edit = "ask"
delete = "ask"
bash = "deny"
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

Global permissions live in `config.toml` and default to `ask` when omitted. Each agent can override per-tool permissions in its agent TOML:

| Level | Behavior |
|-------|----------|
| `allow` | Tool runs automatically |
| `ask` | TUI prompts for Y/N approval |
| `deny` | Tool is blocked and hidden from the model |

Tools: `read`, `glob`, `grep`, `write`, `edit`, `delete`, `bash`, `subagent`. `use_skill` is always allowed.

Recommended baseline: allow read-only tools (`read`, `glob`, `grep`), ask for file mutations (`write`, `edit`, `delete`), and keep `bash` as `ask` or `deny` unless you trust the workspace.

Permission prompts are shown as a popup with three choices:
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

Rollback is conflict-safe: if files changed since the original mutation, rollback is blocked with an explanatory message and the rollback entry is preserved so you can retry after resolving the conflict.

Rollback scope is session-local in multi-client mode: each client can only roll back its own tracked change journal.

### Markdown rendering

Assistant messages are rendered as markdown with extra spacing between blocks so paragraphs, headings, lists, tables, and code blocks are easier to scan. Soft line breaks are treated like spaces; fenced code blocks keep their formatting.

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
RUST_LOG=debug ./target/debug/mote-server

# Client: use -v or RUST_LOG=debug
./target/debug/mote-tui -v
```

Verbose logs are saved to `~/.config/mote/logs/mote.log`.

You can override log directory in `config.toml`:

```toml
[logging]
dir = "logs/"
```

## Docker sandbox

Mote can run entirely inside a Docker container for workspace isolation.
Built-in tools (read, write, bash, grep, glob) cannot escape the mounted workspace directory.

```bash
# Build the image
docker build -f docker/Dockerfile -t mote:latest .

# Run with current directory as the sandboxed workspace
./docker/run.sh

# Run with a specific project directory
./docker/run.sh /path/to/your/project
```

Your `~/.config/mote/` is mounted automatically, so config, auth keys, and session history carry over.

See [docker/README.md](docker/README.md) for the full guide.

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
│       ├── deepseek.rs     # DeepSeek + OpenAI-compatible remote providers
│       └── ollama.rs       # Ollama (local)
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
| `POST` | `/compact` | Summarize older conversation context for persisted compacted sessions |
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

### Prompt assembly

Assembled system layers:

1. **Environment** — model name, working directory, platform, date
2. **Shared system prompt** — `prompts/system/mote.md` by default
3. **User AGENTS.md** — `~/.config/mote/AGENTS.md` (optional)
4. **Workspace AGENTS.md** — repo policy sent by the client (optional)
5. **Agent instructions** — from agent TOML's `instructions` field
6. **Skills** — `~/.config/mote/skills/` names + descriptions (optional)

Per turn, the agent loop also injects a dynamic system reminder with time,
progress, tool results, and guidance.

## Tests

```bash
cargo test                     # all crates
cargo test -p mote-server
cargo test -p mote-tui
```

Target: **zero warnings** across the workspace.
