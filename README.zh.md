# Mote

一个使用 Rust 构建的 AI 编程助手，包含本地服务器和终端 UI。

## 亮点

- 只需构建一次客户端/服务器，之后直接运行编译好的二进制文件即可。
- 支持本地 Ollama 模型，也可对接 DeepSeek、GLM/Z.ai、Kimi、MiniMax 等远程提供商。
- 支持智能体（Agent）、子智能体（Subagent）、技能（Skill）、会话历史记录与回滚。
- 每个智能体在当前应用会话中拥有稳定且易于区分的终端颜色。
- 保持终端工作流轻量化，以键盘操作为核心。

## 快速开始

### 系统要求
- Rust 1.75+
- [ripgrep](https://github.com/BurntSushi/ripgrep)（`rg`）— 内置 `grep` 工具优先使用此工具；如不可用则回退到系统 `grep`
- [Ollama](https://ollama.com) 及模型，或 DeepSeek、GLM/Z.ai、Kimi、MiniMax 的 API 密钥

### 首次运行

### 构建

构建调试版二进制文件：

```bash
cargo build --workspace
```

构建优化后的发布版二进制文件：

```bash
cargo build --workspace --release
```

编译后的二进制文件：

- TUI：`./target/debug/mote-tui` 或 `./target/release/mote-tui`
- 服务器：`./target/debug/mote-server` 或 `./target/release/mote-server`

配置文件位于 `~/.config/mote/`：

```bash
mkdir -p ~/.config/mote
cp config.toml.example ~/.config/mote/config.toml
cp keybindings.toml.example ~/.config/mote/keybindings.toml
```

#### 认证（API 密钥、令牌）

密钥独立存放在 `~/.config/mote/auth.json` 中：

```bash
cp auth.json.example ~/.config/mote/auth.json
```

或者使用 `--login` CLI 参数交互式设置：

```bash
# 交互式选择提供商（调试版）
./target/debug/mote-tui --login

# 交互式选择提供商（发布版）
./target/release/mote-tui --login

# 或直接指定提供商
./target/debug/mote-tui --login deepseek
./target/debug/mote-tui --login glm
./target/debug/mote-tui --login kimi
./target/debug/mote-tui --login minimax

./target/release/mote-tui --login deepseek
./target/release/mote-tui --login glm
./target/release/mote-tui --login kimi
./target/release/mote-tui --login minimax
```

### 运行

使用编译好的客户端二进制文件启动 Mote：

```bash
# 在后台启动本地服务器，然后显示 TUI（调试版）
./target/debug/mote-tui

# 在后台启动本地服务器，然后显示 TUI（发布版）
./target/release/mote-tui

# 独立启动服务器（调试版）
./target/debug/mote-server

# 独立启动服务器（发布版）
./target/release/mote-server

# 纯 TUI 模式，连接至已在运行的服务器（调试版）
./target/debug/mote-tui --tui --server-url http://127.0.0.1:9847

# 纯 TUI 模式，连接至已在运行的服务器（发布版）
./target/release/mote-tui --tui --server-url http://127.0.0.1:9847

# 可选：强制指定会话密钥命名空间（调试版）
./target/debug/mote-tui --session-key team-a

# 可选：强制指定会话密钥命名空间（发布版）
./target/release/mote-tui --session-key team-a
```

`mote-tui` 默认会在一个空闲的本地端口上启动服务器并显示 TUI 前端。当你希望单独运行服务器时使用 `mote-server`，然后使用 `mote-tui --tui --server-url http://127.0.0.1:<port>` 将 TUI 连接到已在运行的服务器。

### 运行时目录

- 日志写入 `~/.config/mote/logs/mote.log`（服务器和客户端详细模式）。
- 历史记录默认存储在 `~/.config/mote/history/` 目录下。
- 在多客户端模式下，历史记录按客户端运行时会话密钥分区：
  - `~/.config/mote/history/<会话密钥>/*.md`

## 使用说明

### 快捷键

默认快捷键（可通过 `keybindings.toml` 自定义）：

| 操作 | 默认按键 | 配置键名 |
|------|----------|----------|
| 发送消息 | `enter` | `send_message` |
| 换行 | `alt+enter` | `insert_newline` |
| 退出 | `ctrl+c` | `quit` |
| 光标左/右移 | `left`/`right` | `cursor_left`/`cursor_right` |
| 光标到行首/行尾 | `home`, `ctrl+a` / `end`, `ctrl+e` | `cursor_home`/`cursor_end` |
| 删除光标前/后 | `backspace` / `delete`, `ctrl+d` | `delete_before`/`delete_after` |
| 清空当前行 | `ctrl+k` | `kill_line` |
| 历史记录上/下翻 | `up`/`down` | `history_up`/`history_down` |
| 向上滚动 | `pageup`, `ctrl+up` | `scroll_up` |
| 向下滚动 | `pagedown`, `ctrl+down` | `scroll_down` |
| 滚动到底部 | `ctrl+end` | `scroll_to_bottom` |
| 斜杠命令 | `ctrl+p` | `agent_command` |
| 切换智能体 / 自动补全 | `tab` | `complete` |
| 切换智能体视图 | `F5` | `switch_view` |
| 取消智能体 | `Esc`（流式输出期间） | `cancel_agent` |

通过 `~/.config/mote/keybindings.toml` 进行自定义：

```toml
send_message = "enter"
quit = ["ctrl+c", "ctrl+q"]
agent_command = "ctrl+space"
```

### 斜杠命令

| 命令 | 说明 |
|------|------|
| `/help` | 显示帮助信息 |
| `/agent` | 列出/切换智能体 |
| `/model` | 打开模型选择弹窗（↑/↓ 选择，Enter 确认，Esc 取消） |
| `/compact` | 将较早的对话上下文压缩为持久化的摘要 |
| `/tokens` | 显示 Token 用量 |
| `/new` | 开始一个新的聊天会话 |
| `/sessions` | 打开会话选择器（↑/↓ 选择，Enter 确认，Esc 取消） |
| `/login` | 显示登录提供商选择 |
| `/login <提供商> <密钥>` | 保存提供商 API 密钥（`deepseek`、`glm`、`kimi`、`minimax`） |
| `/subagents` | 列出活动的子智能体 |
| `/rollback last` | 回滚最近跟踪的文件更改 |
| `/<自定义>` | 运行用户定义的自定义提示命令 |
| `! <命令>` | 在当前工作区中运行本地 shell 命令 |

注意：
- `/sessions` 仅在应用空闲时可用（不在流式对话过程中）。
- `/new` 会清除当前对话记录和活动的已恢复会话 ID，但保留已选择的智能体/模型/工作区。
- `/model <提供商/模型>` 仍然可用，但 `/model` 会打开选择器以便更轻松地选择提供商和模型。
- `/compact` 使用当前有效的智能体模型总结较早的对话轮次。可见的对话记录仍然保留，但后续向 LLM 发送请求时，已被压缩的轮次会被摘要替代。
- 当本地对话上下文过大时，Mote 会在自动压缩前询问。如果你拒绝，Mote 会继续运行并警告模型可能丢失较早的上下文或达到 Token 限制。
- 压缩状态与会话一并保存，并在恢复会话时恢复。
- `!` 命令是本地 TUI 命令；其输出会显示在对话记录中，但不会作为对话历史发送给 LLM。

### 自定义斜杠命令

类似于 opencode，Mote 可以从 Markdown 文件中加载自定义斜杠命令。将命令文件放入以下目录：

- 全局：`~/.config/mote/commands/`

Markdown 文件名即成为斜杠命令名。例如，`~/.config/mote/commands/test.md` 会创建 `/test` 命令：

```markdown
---
description: 运行带覆盖率的测试
agent: build
model: deepseek/deepseek-v4-flash
---

运行完整的测试套件并启用覆盖率分析。重点关注失败用例并提出修复建议。
```

嵌套文件夹将成为斜杠子命令。例如：

```text
~/.config/mote/commands/review/staged.md
```

会变成：

```text
/review/staged
```

支持的提示占位符与 opencode 的常见命令行为保持一致：

- `$ARGUMENTS` 展开为命令后面的所有内容，例如 `/component Button` → `Button`。
- `$1`、`$2`、…… 展开为位置参数；引号内的字符串保持分组。
- `` !`command` `` 在工作区根目录执行 shell 命令并插入 stdout/stderr。
- `@path/to/file` 插入工作区内指定文件的内容。

如果自定义命令与内置命令同名，自定义命令优先。

### 智能体

在 `~/.config/mote/agents/` 目录下将智能体定义为 TOML 文件：

```toml
# ~/.config/mote/agents/plan.toml
model = "deepseek/deepseek-v4-flash"
mode = "primary"
temperature = 0.1
instructions = "你是一个规划智能体。"

[permissions]
read = "allow"
glob = "allow"
grep = "allow"
write = "ask"
edit = "ask"
delete = "ask"
bash = "deny"
```

智能体模式控制可见性：

| 模式 | `/agent` 列表中显示 | 可作为子智能体目标 |
|------|---------------------|--------------------|
| `primary` | ✅ 是 | ❌ 否 |
| `subagent` | ❌ 否 | ✅ 是 |
| `all` | ✅ 是 | ✅ 是 |

默认模式为 `primary`。

### 技能

将技能文件夹放入 `~/.config/mote/skills/`。每个技能是一个文件夹，内含一个包含 YAML 头部信息的 `SKILL.md` 文件：

```markdown
---
name: python-conventions
description: 在处理 Python 文件、pytest、uv、.venv 或 Python 项目约定时使用。
---

- 遵循 PEP 8 规范，命名清晰。
- 非简单代码优先使用类型提示。
- 使用 `pytest` 编写测试。
```

技能会被注入系统提示中。使用 `use_skill` 工具来激活它们。

### 权限系统

全局权限位于 `config.toml` 中，省略时默认为 `ask`。每个智能体可以在其 TOML 文件中覆盖每个工具的权限：

| 级别 | 行为 |
|------|------|
| `allow` | 工具自动运行 |
| `ask` | TUI 弹窗询问 Y/N 确认 |
| `deny` | 工具被阻止，且对模型隐藏 |

工具：`read`、`glob`、`grep`、`write`、`edit`、`delete`、`bash`、`subagent`。`use_skill` 始终允许。

推荐基线：允许只读工具（`read`、`glob`、`grep`），文件修改操作（`write`、`edit`、`delete`）设为询问，并将 `bash` 设为 `ask` 或 `deny`，除非你信任当前工作区。

权限提示以弹窗形式显示，提供三个选项：
- `[Y] 允许一次`
- `[A] 始终允许`（会话级别，需二次确认）
- `[N] 拒绝`

### 文件变更差异显示与回滚

对于成功的文件修改型工具调用（`write`、`edit`、`delete`），Mote 会显示类似 git diff 的摘要：
- 修改的文件：`-` 表示删除的行，`+` 表示添加的行
- 新文件：提示行（`! 新文件已添加：……`）
- 已删除文件：提示行（`! 文件已删除：……`）

使用以下命令回滚最近的变更集：

```bash
/rollback last
```

回滚具有冲突安全性：如果文件在原始修改之后发生了变动，回滚会被阻止并显示解释说明，同时保留回滚记录，以便你在解决冲突后重试。

在多客户端模式下，回滚范围是会话本地的：每个客户端只能回滚自己跟踪的变更日志。

### Markdown 渲染

助手消息会以 Markdown 格式渲染，并在各个区块之间增加额外间距，使段落、标题、列表、表格和代码块更易于浏览。软换行被视为空格；围栏代码块保持其原有格式。

### 子智能体

智能体可以通过 `subagent` 工具委托任务给其他智能体：

```
subagent(agent="review", task="检查这段代码是否有 Bug")
```

每个子智能体拥有自己独立的背景"屏幕"。按 **F5** 可在各视图间切换：
- 主智能体 → 子智能体 1 → 子智能体 2 → …… → 主智能体

状态栏会显示当前正在查看的子智能体（`Sub: review (1/2) running`）。

输入 `/subagents` 可列出所有活动的子智能体及其状态，并查看当前选中了哪一个。

当子智能体完成任务后，其输出会自动作为助手消息添加到主对话中，确保你不丢失上下文。你仍然可以通过 F5 查看子智能体的完整屏幕（含工具调用和推理过程）。

递归限制为 3 层。

### 调试日志

```bash
# 服务器：设置 RUST_LOG=debug 以输出详细日志
RUST_LOG=debug ./target/debug/mote-server

# 客户端：使用 -v 或 RUST_LOG=debug
./target/debug/mote-tui -v
```

详细日志会保存到 `~/.config/mote/logs/mote.log`。

你可以在 `config.toml` 中覆盖日志目录：

```toml
[logging]
dir = "logs/"
```

## Docker 沙箱

Mote 可以在 Docker 容器内完全运行，实现工作区隔离。
内置工具（read、write、bash、grep、glob）无法逃脱挂载的工作区目录。

```bash
# 构建镜像
docker build -f docker/Dockerfile -t mote:latest .

# 使用当前目录作为沙箱工作区运行
./docker/run.sh

# 使用指定项目目录运行
./docker/run.sh /path/to/your/project
```

你的 `~/.config/mote/` 会自动挂载，因此配置、认证密钥和会话历史记录都会自动带入。

完整指南请参见 [docker/README.md](docker/README.md)。

## 架构

Mote 分为**服务器**（LLM 提供商、智能体循环、工具执行）和**客户端**（TUI、快捷键、渲染），两者通过 WebSocket + HTTP 通信。

```
mote/
├── Cargo.toml              # 工作区：[protocol, server, client]
├── protocol/               # 共享类型（ServerEvent, ChatRequest）
├── server/                 # axum HTTP + WS 守护进程
│   ├── main.rs             # 路由、WS 处理器、取消守卫
│   ├── auth.rs             # auth.json 加载器
│   ├── config.rs           # config.toml 加载
│   ├── prompt.rs           # 提示组装（6 层）
│   ├── agent.rs            # 智能体运行循环
│   ├── tools.rs            # 内置工具
│   ├── session.rs          # 会话持久化
│   ├── history.rs          # Markdown 历史记录写入器
│   └── llm/                # 提供商实现
│       ├── mod.rs          # 提供商工厂
│       ├── deepseek.rs     # DeepSeek + 兼容 OpenAI 的远程提供商
│       └── ollama.rs       # Ollama（本地）
└── client/                 # TUI + CLI
    ├── main.rs             # 入口点
    ├── client.rs           # WebSocket 客户端 + 聊天流
    ├── config.rs           # 快捷键加载
    ├── llm.rs              # Role 枚举
    └── tui/
        ├── mod.rs          # 事件循环
        ├── state.rs        # 应用状态机
        ├── render.rs       # ratatui 渲染
        └── keybinding.rs   # 按键动作映射
```

### 服务器端点

| 方法 | 路径 | 说明 |
|------|------|------|
| `GET` | `/health` | 健康检查 |
| `GET` | `/config` | UI 设置（强调色、智能体名称、模型信息） |
| `GET` | `/models` | 所有提供商提供的可用模型 |
| `GET` | `/sessions` | 已保存的会话列表 |
| `POST` | `/compact` | 总结较早的对话上下文（用于持久化压缩的会话） |
| `POST` | `/rollback/last` | 回滚最近跟踪的文件更改 |
| `WS` | `/chat` | 流式聊天 |

### 数据流

```
终端 2（客户端）               终端 1（服务器）
┌─────────────────────────┐      ┌──────────────────────────┐
│ TUI 事件循环            │      │                          │
│   → 用户输入 + Enter    │  WS  │   axum /chat             │
│   → client.chat_stream()│─────►│   → agent::run_loop()    │
│   → 接收 ServerEvent    │◄─────│   → 发送 ServerEvent     │
│   → 更新 App 状态       │ JSON │     通过 WebSocket       │
│   → 渲染帧              │      │                          │
└─────────────────────────┘      └──────────────────────────┘
```

客户端从不直接调用 LLM 提供商——所有智能体逻辑都在服务器端运行。

### 提示组装

组装的系统层：

1. **环境** — 模型名称、工作目录、平台、日期
2. **共享系统提示** — 默认为 `prompts/system/mote.md`
3. **用户 AGENTS.md** — `~/.config/mote/AGENTS.md`（可选）
4. **工作区 AGENTS.md** — 由客户端发送的仓库策略（可选）
5. **智能体指令** — 来自智能体 TOML 的 `instructions` 字段
6. **技能** — `~/.config/mote/skills/` 中的名称与描述（可选）

每轮对话中，智能体循环还会注入一个动态系统提醒，包含时间、进度、工具结果和指引。

## 测试

```bash
cargo test                     # 所有 crate
cargo test -p mote-server
cargo test -p mote-tui
```

目标：**整个工作区零警告**。
