# Remaining Work Plan: Marshaling Codebase Polish

> **Status**: Incremental feature delivery completed for file diff display + delete tool + rollback v1.  
> **Last updated**: 2026-06-01  
> **Baseline**: 182 tests passing, zero compile errors.

### 2026-06-01 execution notes (feature slice)

- ✅ Added structured file change metadata in protocol (`FileChange`, `DiffLine`, kinds).
- ✅ Extended tool-complete server events to carry `changes`.
- ✅ Added built-in `delete` tool (file-only), wired into built-in tool registry.
- ✅ Added server-side rollback journal and `POST /rollback/last`.
- ✅ Added `/rollback last` slash command in client.
- ✅ Added git-diff-like rendering for modified lines, plus alerts for add/remove.
- ✅ Added/updated tests for tool behavior (`delete`, structured outputs).
- ✅ Updated `README.md` and `config.toml.example` for new capabilities.

### 2026-06-13 execution notes (rename slice)

- ✅ Renamed workspace crate packages to `mote-*` (`mote-server`, `mote-client`, `mote-protocol`).
- ✅ Updated client/server runtime strings, log targets, and startup messages from Marshaling → Mote.
- ✅ Migrated config/auth/skills/AGENTS paths from `~/.config/marshaling/` → `~/.config/mote/`.
- ✅ Updated prompts and docs (`README.md`, `AGENTS.md`, examples) to use Mote naming.
- ✅ Renamed `MarshalingClient` type to `MoteClient` for consistency.
- ✅ Verified with tests: `cargo test -p mote-server` and `cargo test -p mote-client` (all passing).

### 2026-06-14 execution notes (sessions popup + continuation slice)

- ✅ Replaced legacy `/session ...` command intent in client with popup-first `/sessions` flow.
- ✅ Added/finished session picker UI handling: open from `/sessions`, navigate with `↑/↓`, `Enter` to load, `Esc` to close.
- ✅ Wired loaded session messages into chat state and tracked `active_session_id` for continuation.
- ✅ Updated outgoing chat requests to include selected `session_id` so follow-up turns continue the chosen session.
- ✅ Server now validates optional `session_id` and reuses it when persisting session history on `Done`.
- ✅ Session summary generation now follows short word-based style (5–10 word target) instead of 80-char slicing.
- ✅ Removed now-unused client `delete_session` method to keep warning-free build.
- ✅ Updated README slash-command table to document `/sessions` picker UX.
- ✅ Verified with tests:
  - `cargo test -p mote-client` (77 passed)
  - `cargo test -p mote-server` (116 passed)
  - `cargo test -p mote-protocol` (8 passed)

### 2026-06-14 execution notes (sessions review fixes slice)

- ✅ Fixed race/regression risk: `/sessions` open/load is now blocked while a chat stream is active.
- ✅ Added explicit command feedback when users try to open/load sessions during running state.
- ✅ Added `App::reset_for_loaded_session()` and applied it during session load to clear transient per-turn UI/runtime state:
  - stream/reasoning buffers
  - tool/subagent views
  - pending permission + cancel state
  - queued input/suggestions/input cursor
  - scroll + loading progress + picker state
- ✅ Hardened server session loading role mapping:
  - unsupported roles are now skipped (no fallback coercion to `user`).
- ✅ Improved session picker UX for long lists with windowed rendering around current selection plus `... N more` indicator.
- ✅ Added tests for new behavior:
  - `/sessions` blocked while running
  - loaded-session state reset helper
  - chat request includes `active_session_id`
  - server role mapping helper filters non-conversation roles
  - picker windowing helper bounds/centering
- ✅ Updated README notes to clarify `/sessions` is idle-only.
- ✅ Verified with full workspace test run:
  - `cargo test --workspace` (all passing)

### 2026-06-18 execution notes (post-review hardening slice)

- 🚧 Implementing non-auth review fixes while explicitly deferring localhost/API authentication work per user request.
- ✅ Changed omitted global tool permission default from `allow` to `ask`.
- ✅ Hid denied tools from primary/subagent model tool advertisements at `run_loop` time.
- ✅ Preserved final no-tool assistant responses in saved session history.
- ✅ Skipped internal assistant tool-call placeholders and tool result messages when persisting user-visible sessions.
- ✅ Made rollback journal entries pop only after rollback succeeds, preserving entries on conflict/failure.
- ✅ Added CRLF-aware SSE event separator parsing for DeepSeek and GitHub model streams.
- ✅ Reduced verbose LLM request/response logs to metadata summaries to avoid logging prompt/body content.
- ✅ Updated README/config example to document safer permissions and rollback retry behavior.
- ✅ Added targeted tests for final-answer persistence, denied tool advertisement, session placeholder filtering, rollback conflict preservation, safer default permissions, and SSE CRLF separators.
- ✅ Verified:
  - `cargo fmt -p mote-server --check`
  - `cargo test -p mote-server` (125 passed)
  - `cargo test --workspace` (215 passed)
- ⚠️ `cargo fmt --check` for the whole workspace still reports pre-existing client formatting diffs unrelated to this slice; only the server crate was formatted to avoid unrelated churn.
- ✅ Follow-up review result: no blocking regressions found in the non-auth changes.
- ⚠️ Follow-up caveats: rollback is still not fully transactional on mid-apply failure; provider error bodies can still include upstream response text; additional future tests could cover successful rollback pop/apply-failure preservation/subagent integration/SSE split chunks.

### 2026-06-19 plan: TUI command and popup UX improvements

- Goal: add local `! <command>` shell input, improve thinking/answer spacing, replace typed model selection with a popup picker, and restore permission approval as an elegant popup.
- Planned approach:
  - Treat `!` input as a local command action, not LLM conversation history.
  - Fetch `/models` into a model picker popup with Default + provider/model rows and ↑/↓/Enter/Esc handling.
  - Render permission requests as centered overlays with margins and compact args preview.
  - Add extra blank spacing between reasoning/thinking and answer text for stored/live/subagent output.
  - Update tests/docs and verify with client/workspace test runs.
- ✅ Implemented:
  - local `! <command>` execution via `/bin/bash -lc` in the client workspace root;
  - shell command outputs as command messages excluded from LLM history;
  - model picker popup with Default + provider/model choices and ↑/↓/Enter/Esc handling;
  - centered permission popup with margins, args preview, and styled Y/A/N controls;
  - extra spacing between thinking/reasoning and answer output;
  - README/help text updates;
  - tests for shell input, model picker behavior, and command-history exclusion.
- ✅ Verified:
  - `cargo fmt --check`
  - `cargo test -p mote-client` (87 passed)
  - `cargo test --workspace` (220 passed)

### 2026-06-19 execution notes (TUI elegance polish)

- ✅ Applied a visual-only polish pass focused on simple, clear, elegant presentation.
- ✅ Planned changes:
  - calmer welcome screen with concise shortcut hints;
  - consistent popup chrome for sessions/models/permissions;
  - softer loading line and status-bar hints;
  - empty-input placeholder hint;
  - cleaner session picker row metadata.
- ✅ Verified:
  - `cargo fmt --check`
  - `cargo test -p mote-client` (87 passed)
  - `cargo test --workspace` (220 passed)

### 2026-06-19 plan: session/model/server startup polish

- 🚧 Implementing requested updates:
  - `/new` command to start a fresh chat session while keeping agent/model/workspace selections;
  - model picker grouped into provider sections;
  - server startup auto-increments to the next available localhost port if configured port is occupied.
- ✅ Implemented and documented in README.
- ✅ Added/updated tests for new-session reset and provider-sorted model picker behavior.
- ✅ Verified:
  - `cargo fmt --check`
  - `cargo test --workspace` (221 passed)

### 2026-06-19 plan: unified client launcher modes

- 🚧 Implementing final launch UX update for this round:
  - default `mote-client` starts a background local server and displays only the TUI;
  - `--server` runs server-only mode;
  - `--tui` runs frontend-only mode against `--server-url`;
  - old server URL option is renamed to `--server-url` because `--server` is now a mode flag.
- ✅ Implemented and documented in README.
- ✅ Added `MOTE_SERVER_PORT` / `MOTE_PORT` server port override for the launcher path.
- ✅ Verified:
  - `cargo fmt --check`
  - `cargo test -p mote-client` (88 passed)
  - `cargo test --workspace` (221 passed)

---

## Current Baseline

| Metric | Value |
|--------|-------|
| Server tests | 92 passed, 0 failed |
| Client tests | 66 passed, 0 failed |
| Protocol tests | 6 passed, 0 failed |
| **Total tests** | **164 passed** |
| Server clippy warnings | 20 (16 auto-fixable) |
| Client clippy warnings | 24 (23 auto-fixable) |
| Compile errors | 0 |

---

## Remaining Work by Priority Tier

---

## 2026-06-14 plan: decouple runtime workspace from server process cwd

### Goal

Allow:

- the **server** to run from any folder on the machine
- the **client/frontend** to run from any folder on the machine
- each chat request to target a **separate user-selected workspace/repo root**
- repo-local context such as **`AGENTS.md`** to be collected from the target workspace and sent to the backend for prompt assembly

### Key findings from current code

- Server workspace is currently fixed at startup from `std::env::current_dir()` in `server/src/main.rs` and stored as `AppState.workspace`.
- Built-in tools are created once at startup via `llm::builtin_tools(workspace.clone())`, so all file tools are pinned to the server process cwd.
- Prompt environment text and reminder text both use `std::env::current_dir()` (`server/src/prompt.rs`, `server/src/agent.rs`) rather than request-specific workspace context.
- Prompt assembly currently reads only user-global `~/.config/mote/AGENTS.md`; it does **not** ingest repo-local `AGENTS.md` from the target workspace.
- `ChatRequest` has no field for target workspace path or client-collected repo instructions.
- Config discovery still falls back to server cwd (`find_config()`), which is fine for server config, but must stay separate from per-request workspace.

### Recommended architecture

Keep **server runtime/config root** separate from **request workspace root**.

```text
Server process cwd
  └─ only for launching the binary

Server config root (~/.config/mote or explicit config path)
  └─ config.toml, auth.json, skills, global AGENTS.md

Per-request workspace root (sent by client)
  └─ repo files, .git, local AGENTS.md, prompt-local context, tool sandbox
```

### Proposed protocol shape

- Extend `protocol::ChatRequest` with request-scoped workspace data:
  - `workspace_root: Option<String>` — absolute path selected by the client
  - `workspace_context: Option<WorkspaceContext>`
- Add `WorkspaceContext` in `protocol/`:
  - `repo_agents_md: Option<String>`
  - optional future fields: `git_repo: Option<bool>`, `workspace_label: Option<String>`

Recommendation: start with **both** `workspace_root` and `repo_agents_md`.

- `workspace_root` is needed for tool sandboxing and bash `current_dir`
- `repo_agents_md` is needed so frontend can explicitly pass repo-local instructions as you described

### Proposed execution plan

- [ ] **W1** Add request-scoped workspace types to `protocol/src/types.rs` and bump protocol version.
- [ ] **W2** Update client request construction (`client/src/main.rs`, `client/src/tui/mod.rs`) to send:
  - selected/current workspace absolute path
  - repo-local `AGENTS.md` contents if present in that workspace
- [ ] **W3** Introduce a client-side workspace resolver module:
  - resolve target workspace root from launch cwd (initial version)
  - read `<workspace>/AGENTS.md` safely
  - keep failure non-fatal when file is absent
- [ ] **W4** Refactor server request handling so tools are built per request using `request.workspace_root`, not `AppState.workspace`.
- [ ] **W5** Replace prompt/agent cwd lookups with explicit request workspace context:
  - env block working directory
  - git repo detection
  - dynamic reminder working directory
- [ ] **W6** Extend prompt assembly to accept repo-local prompt layers from the request:
  - prepend/append repo-local `AGENTS.md` at a defined layer
  - keep global `~/.config/mote/AGENTS.md` behavior unchanged
- [ ] **W7** Decide and enforce trust boundary for client-sent paths:
  - require absolute path
  - canonicalize on server
  - reject nonexistent workspace roots
  - ensure all file tools remain constrained under that canonical root
- [ ] **W8** Update subagent tool construction so nested agents inherit the same request workspace.
- [ ] **W9** Add tests:
  - protocol serde round-trip for new fields
  - prompt assembly includes repo-local AGENTS content
  - tools reject paths outside request workspace
  - bash runs inside request workspace
  - client gracefully handles missing local AGENTS.md
- [ ] **W10** Update docs/README with the new mental model: server config root vs per-chat workspace root.

### Review / result for this planning slice

- No product code changed yet.
- Architectural change is required; current structure cannot support arbitrary frontend/server folders plus a separate target repo workspace correctly.
- The safest design is **request-scoped workspace context**, not changing server global cwd.

### Tier 1: High Priority (highest impact per effort)

These items offer substantial correctness or architecture improvements with moderate effort.

---

#### H1-full: Decompose `run_loop()` in `server/src/agent.rs`

**Current state**: `run_loop()` is 268 lines (lines 78–346) in a single `for` loop. No decomposition has been done. Helper functions `extract_last_turn_results()` (line 349) and `extract_last_user_message()` (line 394) already extracted.

**Value**: Improves testability, readability, and enables future composition.  
**Effort**: High (~2–3 hours refactoring + testing).  
**Risk**: Medium — must preserve exact async state machine semantics.

- [ ] **H1-full.1** Extract `send_llm_request()` (~lines 131–215): build messages, spawn `chat_stream()`, process stream events, accumulate tokens/tool calls, return accumulated `(content, reasoning, tool_calls, tokens)`.
- [ ] **H1-full.2** Extract `execute_tool_calls()` (~lines 226–342): for each tool call — cancel check, skill check, permission check, execute, record results, push to history.
- [ ] **H1-full.3** Refactor `run_loop()` to become a thin orchestrator (~30–40 lines): setup → loop { send_llm_request() → execute_tool_calls() → emit TurnDone }.
- [ ] **H1-full.4** Verify all 92 server tests still pass. Run agent with multi-turn conversation to confirm behavior unchanged.
- [ ] **H1-full.5** Run `cargo clippy -p marshaling-server` and fix any new warnings introduced.

---

#### L1: Type-safe `Role` enum in protocol

**Current state**: `protocol/src/types.rs:11` — `HistoryMessage.role: String` with comment `// "user" or "assistant"`. String matching in `server/src/main.rs:110-114, 400-401` and `client/src/main.rs:100-103`, `client/src/tui/mod.rs:218-220`.

**Value**: Eliminates string-matching bugs, enables exhaustive pattern matching.  
**Effort**: Low (~30–45 min).  
**Risk**: Low — purely additive, serde-compatible.

- [ ] **L1.1** In `protocol/src/types.rs`, define `enum Role { User, Assistant, System, Tool }` with serde `rename_all = "lowercase"`. Replace `HistoryMessage.role: String` with `role: Role`.
- [ ] **L1.2** Add `impl From<Role> for llm::Role` conversion (or vice versa) in server crate for interop.
- [ ] **L1.3** Update `server/src/main.rs` — replace `"user" => ...`, `"assistant" => ...` string matches with `Role::User`, `Role::Assistant`.
- [ ] **L1.4** Update `client/src/main.rs:100-103` — use `Role` enum instead of string match.
- [ ] **L1.5** Update `client/src/tui/mod.rs:218-220` — use `Role` enum.
- [ ] **L1.6** Update `client/src/tui/state.rs` — `DisplayMessage` role field if it uses `String`.
- [ ] **L1.7** Verify 164 tests pass, zero new clippy warnings.

---

### Tier 2: Medium Priority (good ROI, moderate risk)

---

#### L5: Split `App`'s 31 fields into sub-structs

**Current state**: `client/src/tui/state.rs:45-116` — `App` has 31 flat fields. Three small sub-structs already exist (`SubagentView`, `PendingPermission`, `SlashAction`). All fields accessed directly (`app.field_name`) in `state.rs`, `render.rs`, `mod.rs`.

**Value**: Improves code organization, reduces cognitive load, enables focused unit testing of sub-states.  
**Effort**: Medium (~1.5–2 hours — heavy refactoring of field access across 3 files).  
**Risk**: Medium — every field access in 3 files changes. Must be mechanically correct.

- [ ] **L5.1** Define sub-structs in `state.rs`:
  - **`InputState`**: `input`, `input_cursor`, `input_history`, `input_history_idx`, `suggestions`, `suggestion_index`, `handled_slash_command`, `input_queue`
  - **`StreamState`**: `stream_buffer`, `reasoning_buffer`
  - **`ToolState`**: `tool_calls`, `pending_permission`, `pending_permission_response`
  - **`ConfigState`**: `current_agent`, `agent_names`, `model_override`, `provider_override`, `models_cache`, `input_accent`, `user_accent`, `assistant_accent`
  - **`SubagentState`**: `subagent_view`, `show_subagent`, `current_skill`
  - **`ConnectionState`**: `server_health`, `pending_slash`, `loading_progress`, `pending_cancel`
- [ ] **L5.2** Replace 31 flat fields in `App` with 6 sub-struct fields: `pub input: InputState`, `pub stream: StreamState`, `pub tools: ToolState`, `pub config: ConfigState`, `pub subagent: SubagentState`, `pub conn: ConnectionState`.
- [ ] **L5.3** Update all field accesses in `state.rs` — `app.input` → `app.input.input`, `app.stream_buffer` → `app.stream.buffer`, etc.
- [ ] **L5.4** Update all field accesses in `render.rs` (~20+ accesses).
- [ ] **L5.5** Update all field accesses in `mod.rs` (event dispatcher, ~30+ accesses).
- [ ] **L5.6** Update `App::new()` and `App::from_session()` to initialize sub-structs.
- [ ] **L5.7** Verify 66 client tests pass, zero new clippy warnings.

---

#### L7: Provider registry pattern in `server/src/llm/mod.rs`

**Current state**: `build_provider_for()` (line 234) uses a 2-arm match statement on string literals (`"deepseek"`, `"ollama"`). No registry pattern exists.

**Value**: Decouples provider instantiation from core logic, enables future provider plugins.  
**Effort**: Medium (~1 hour — type system design + migration).  
**Risk**: Low — pure refactor of instantiation path.

- [ ] **L7.1** Define `type ProviderFactory = fn(&crate::config::Config) -> Result<Box<dyn LlmProvider>>;` and `struct ProviderRegistry { providers: HashMap<String, ProviderFactory> }`.
- [ ] **L7.2** Implement `ProviderRegistry::new()` — registers `"deepseek"` and `"ollama"` factories.
- [ ] **L7.3** Add `registry.get(provider_name).ok_or_else(|| anyhow!("Unknown provider"))?(&config)`.
- [ ] **L7.4** Replace `build_provider_for()` body with registry lookup. Keep function signature for backward compat.
- [ ] **L7.5** Verify 92 server tests pass. Add a test for unknown provider error.

---

#### L8: File locking for sessions using `fs2`

**Current state**: `server/src/history.rs:106-114` — `save_session()` uses `std::fs::write()` directly. `load_session()` uses `std::fs::read_to_string()`. No locking. `fs2` not in `server/Cargo.toml`.

**Value**: Prevents session corruption when multiple processes or rapid save/load cycles collide.  
**Effort**: Medium (~1 hour — fs2 integration + retry logic).  
**Risk**: Low — advisory locks, fall back gracefully.

- [ ] **L8.1** Add `fs2 = "0.4"` to `server/Cargo.toml` dependencies.
- [ ] **L8.2** In `save_session()`: open file with `OpenOptions::create().write()`, acquire exclusive lock via `fs2::FileExt::lock_exclusive()`, write, drop lock on `Drop`.
- [ ] **L8.3** In `load_session()`: open file with `OpenOptions::read()`, acquire shared lock via `lock_shared()`.
- [ ] **L8.4** Add retry with exponential backoff (up to 3 attempts, 100ms → 200ms → 400ms). Return `LockContention` error on exhaustion.
- [ ] **L8.5** Add test: two concurrent saves to same session file — second should wait or fail cleanly.
- [ ] **L8.6** Verify all existing session tests pass.

---

#### M11: WebSocket TLS support (`wss://`)

**Current state**: `client/src/client.rs:112-117` — always uses `ws://` scheme. URL parsing strips `http://`/`https://` and prepends `ws://`. No TLS connector. `WsWriter` type alias references `MaybeTlsStream` but no TLS is configured. No `--tls` CLI flag.

**Value**: Secure communication for remote deployments.  
**Effort**: Medium (~1 hour — TLS connector config + CLI flag).  
**Risk**: Medium — need to test with actual TLS cert (or skip verification for dev).

- [ ] **M11.1** Add `tokio-native-tls` (or `rustls` + `webpki-roots`) feature to `client/Cargo.toml`. Prefer `rustls` for pure-Rust builds.
- [ ] **M11.2** In `MarshalingClient::connect()`, parse URL scheme: `https://` or `wss://` → use TLS; `http://` or `ws://` → plain TCP.
- [ ] **M11.3** For TLS, use `tokio_tungstenite::connect_async_tls_with_config()` with a `rustls::ClientConfig` that uses webpki roots (or `dangerous_configuration` for self-signed).
- [ ] **M11.4** Add `--tls` CLI flag (or `--insecure` for self-signed) and `MARSHALING_TLS` env var.
- [ ] **M11.5** Update error messages — `"Failed to connect"` → distinguish TLS handshake vs WebSocket upgrade failures.
- [ ] **M11.6** Test: connect to `wss://localhost:9847` with self-signed cert (dev mode).

---

### Tier 3: Low Priority (polish, cosmetic, nice-to-have)

---

#### H3: Eliminate `history.clone()` — use `Arc`

**Current state**: PARTIALLY DONE. Line 131 uses `history.iter().cloned()` to avoid intermediate `Vec` allocation. But `AgentEvent::Done` (line 64) still uses `Vec<ChatMessage>` (not `Arc`), so full deep copy still occurs on every `Done` emission (lines 126, 175, 200, 212, 229, 304, 345).

**Value**: Eliminates O(n) clone at every agent turn termination.  
**Effort**: Low (~30 min — change 8 lines).  
**Risk**: Low — `Arc` is purely additive, no behavior change.

- [ ] **H3.1** Change `AgentEvent::Done { history: Vec<ChatMessage> }` → `Arc<Vec<ChatMessage>>`. Update type definition in `agent.rs:64-71`.
- [ ] **H3.2** Wrap history in `Arc::new()` at the start of `run_loop()` (after initial push).
- [ ] **H3.3** Replace all 7 `Done` emission sites: `history` → `Arc::clone(&history)`.
- [ ] **H3.4** Update `server/src/main.rs:439` consumer to deref `Arc` when building session.
- [ ] **H3.5** Verify 92 server tests pass, zero clippy warnings added.

---

#### M4: Audit GlobTool description accuracy

**Current state**: Description is `"Search for files matching a glob pattern."` — NO false gitignore claim. Implementation uses `glob` crate (no gitignore support). The original M4 plan noted a false claim, but the current description is already accurate.

**Decision**: Option (b) — update description to remove false claim. **Already done** (description is already truthful). This item may be moot or reduces to:

- [ ] **M4.1** Verify GlobTool description accurately reflects capability (✓ already accurate).
- [ ] **M4.2** (Optional) If gitignore support is desired in future, add `ignore` crate and update description. Track as separate feature request.
- [ ] **M4.3** (If keep as-is) Add a comment in `tools.rs` noting gitignore is intentionally not respected (simplicity over feature creep).

---

#### M9: Rename `UiConfig` → `UiStyle`

**Current state**: 18 references across 7 files. Two structs named `UiConfig` exist:
- `protocol/src/types.rs:109` — public type with 6 fields (accent colors + agent info)
- `server/src/config.rs:115` — internal config struct (3 fields, accent colors only)

**Value**: Purely cosmetic — `UiStyle` better describes the purpose (visual styling vs configuration).  
**Effort**: Very low (~10 min — find-and-replace).  
**Risk**: None — mechanical rename, zero semantic change.

- [ ] **M9.1** Rename `UiConfig` → `UiStyle` in `protocol/src/types.rs:109`.
- [ ] **M9.2** Rename `UiConfig` → `UiStyle` in `server/src/config.rs:40,115,127`.
- [ ] **M9.3** Update `server/src/main.rs:58` — rename.
- [ ] **M9.4** Update `client/src/client.rs:3,52` — rename.
- [ ] **M9.5** Update `client/src/main.rs:128` — rename.
- [ ] **M9.6** Update `client/src/tui/state.rs:173,682-683` — rename.
- [ ] **M9.7** Update `README.md` and `AGENTS.md` if they reference the type by name.
- [ ] **M9.8** Run `cargo build --workspace` — verify zero compile errors.
- [ ] **M9.9** Run 164 tests — verify zero failures.

---

#### E-B6: Cache skill index at startup

**Current state**: Skills are parsed per-agent-invocation from system prompt layers (lines 98–115 in `agent.rs`). No indexing, no caching. Skills listed in prompt files but re-parsed every time.

**Value**: Avoids repeated regex/file parsing.  
**Effort**: Low (~30 min — pre-parse on startup, store in `AppState`).  
**Risk**: Low — cache invalidation is simple (skills change only on config reload).

- [ ] **E-B6.1** Add `skills_index: Vec<SkillInfo>` (or `Arc<RwLock<Vec<SkillInfo>>>`) to `AppState` in `server/src/main.rs`.
- [ ] **E-B6.2** Parse all available skills from `prompts/instructions/` at server startup (after config load). Store skill name, description, and source path.
- [ ] **E-B6.3** In `agent.rs:98-115`, replace regex-based parsing with lookup from `AppState.skills_index`.
- [ ] **E-B6.4** Emit `SkillsLoaded` with the cached data (same event format, no breaking change).
- [ ] **E-B6.5** Verify 92 server tests pass. Add test: skills index populated after startup.

---

#### T1-T7: Final testing, documentation, and review

**Current state**: 164 tests pass, 44 clippy warnings, no formatting check run yet.

**Value**: Gates the project for production readiness.  
**Effort**: Medium (~1.5–2 hours across all T items).  
**Risk**: None — verification-only tasks.

- [ ] **T1** Add unit/integration tests for any new safety checks introduced (C1 truncation, H6 sandbox, C3 timeout — if implemented). If these items are skipped, T1 is N/A.
- [ ] **T2** Add perf benchmarks for H3 (clone elimination) if implemented.
- [ ] **T3** Verify all 164 existing tests pass: `cargo test --workspace`.
- [ ] **T4** Fix all 44 clippy warnings: run `cargo clippy --fix --workspace` for auto-fixable ones, manually fix the rest, then `cargo clippy --workspace -- -D warnings` to enforce zero-warning policy.
- [ ] **T5** Run `cargo fmt --all -- --check` — ensure formatting compliance. Run `cargo fmt --all` if needed.
- [ ] **T6** Update `README.md` with any new features: TLS, `--resume`, `--single`, `--tls`, `max_steps` config, `fs2` dependency note.
- [ ] **T7** Final review: audit for any remaining `unwrap()`/`expect()` in production (non-test) code paths. Run `rg "unwrap\(\)" --type rust --glob '!*test*'` and `rg "expect\(.*\)" --type rust --glob '!*test*'`.

---

## Progress Tracker

| Tier | Items | Completed | Remaining |
|------|-------|-----------|-----------|
| Tier 1: High Priority | H1-full, L1 (2) | 0 | 2 |
| Tier 2: Medium Priority | L5, L7, L8, M11 (4) | 0 | 4 |
| Tier 3: Low Priority | H3, M4, M9, E-B6 (4) | 0 | 4 |
| Tier 3: Tests & Docs | T1-T7 (7 sub-items) | 0 | 7 |
| **Total** | **17** | **0** | **17** |

---

## Implementation Order (Recommended)

```
Phase 1: Quick wins (1–2 hours)
  ├── L1  (Role enum)           — 30 min, high value
  ├── M9  (UiConfig→UiStyle)    — 10 min, trivial
  ├── H3  (Arc history)         — 30 min, perf win
  ├── T4  (clippy fix all)      — 20 min, zero-warning policy
  └── T5  (cargo fmt)           — 5 min

Phase 2: Architecture (2–3 hours)
  ├── L7  (Provider registry)   — 1 hour
  ├── L8  (Session locking)     — 1 hour
  └── H1-full (run_loop decomp) — 2–3 hours (largest item)

Phase 3: Feature & Polish (2–3 hours)
  ├── L5  (App sub-structs)     — 1.5–2 hours (riskiest refactor)
  ├── M11 (TLS support)         — 1 hour
  ├── E-B6 (Skill cache)        — 30 min
  └── M4  (GlobTool audit)      — 5 min (moot)

Phase 4: Ship it (1 hour)
  ├── T6  (README update)
  ├── T7  (unwrap audit)
  └── T3  (final test suite run)

Total estimated: ~8–10 hours
```

---

## Notes

- **M4 (GlobTool gitignore)** is effectively complete — the description is already accurate. Mark as verified/non-actionable unless gitignore support is desired as a new feature.
- **M9 (UiConfig→UiStyle)** has two distinct structs in different crates — rename both, but they serve different purposes (protocol transport vs server config). Consider renaming them differently: `UiStyle` for protocol, but the server-internal one could stay `UiConfig` or become `AccentColors`.
- **L5 (App sub-structs)** is the highest-risk refactor due to sheer number of field accesses across 3 files. Consider doing this in a separate branch with mechanical search-replace.
- **T4 (clippy warnings)** — 44 warnings, mostly `collapsible_if` and `unnecessary_map_or`. Auto-fix handles 39 of 44. Manual fixes needed for 5 items (e.g., `too_many_arguments` on `run_loop`, `ptr_arg` in tools).
- All key files: `server/src/{agent,config,tools,history,llm/{mod,deepseek,ollama},main}.rs`, `client/src/{main,client,tui/{state,render,mod}}.rs`, `protocol/src/{lib,types}.rs`.
- Reference `AGENTS.md` at the repo root for code style and architecture guidelines. Zero compiler warnings is a hard requirement.
