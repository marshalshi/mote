# Mote Docker Sandbox

Run Mote in a containerised environment. The agent's tool execution stays inside the mounted workspace — tools like `read`, `write`, `bash`, and `grep` cannot escape the sandbox.

## Quick start

```bash
# 1. Build the image (from repo root)
docker build -f docker/Dockerfile -t mote:latest .

# 2. Run with current directory as the workspace
./docker/run.sh

# Or specify a project directory
./docker/run.sh /path/to/your/project
```

## How it works

The image bundles the `mote-client` TUI and `mote-server` in a single container.  
When you run the container:

1. Your chosen host directory is mounted at `/workspace` inside the container.
2. Your `~/.config/mote/` directory is mounted at `/root/.config/mote/` (config, auth, history, agents, skills).
3. `mote-client` starts in the foreground with the TUI, spawning `mote-server` automatically — just like the non-Docker flow.

## Building the image

```bash
# From the repository root:
docker build -f docker/Dockerfile -t mote:latest .

# Use a custom tag:
docker build -f docker/Dockerfile -t my-registry/mote:v1 .
```

The Dockerfile uses multi-stage builds and dependency-layer caching to keep rebuilds fast.

## Selecting a workspace

The sandboxed workspace is the directory the agent can read, write, and execute commands in.  
All file tools (`read`, `write`, `edit`, `glob`, `grep`, `bash`) are restricted to this root.

**Via the wrapper** (recommended):

```bash
./docker/run.sh                    # current directory
./docker/run.sh ~/projects/my-app  # explicit path
```

**Via docker directly**:

```bash
docker run -it --rm \
    -v "$PWD:/workspace" \
    -w /workspace \
    -v "$HOME/.config/mote:/root/.config/mote" \
    mote:latest
```

## Configuration

Your local `~/.config/mote/` is mounted into the container so all settings, auth keys, agents, skills, and session history are preserved across runs.

If you haven't set up the config yet:

```bash
mkdir -p ~/.config/mote
cp config.toml.example ~/.config/mote/config.toml
cp keybindings.toml.example ~/.config/mote/keybindings.toml
cp auth.json.example ~/.config/mote/auth.json
cp -r prompts/ ~/.config/mote/prompts/
```

Then edit `~/.config/mote/config.toml` with your preferred provider and model.

## Auth (API keys)

Secrets are stored in `~/.config/mote/auth.json` on the host. The container reads them from the mounted config directory. To set up auth inside the container:

```bash
./docker/run.sh
# Then in the TUI: /login deepseek <your-api-key>
# Or: /login github
```

Credentials saved this way persist in `~/.config/mote/auth.json` on the host.

## History and logs

- Session history is stored under `~/.config/mote/history/` on the host.
- Logs are written to `~/.config/mote/logs/` on the host.
- Both paths survive container restarts because the config directory is mounted.

## Running server-only

If you want only the server inside the container and connect from a host TUI:

```bash
docker run -it --rm \
    -p 9847:9847 \
    -v "$PWD:/workspace" \
    -w /workspace \
    -v "$HOME/.config/mote:/root/.config/mote" \
    mote:latest \
    mote-server
```

Then on the host:

```bash
cargo run -- --tui --server-url http://127.0.0.1:9847
```

> **Note**: When running this way, the host client sends your host's working directory as the workspace root, but the server sees it as `/workspace` inside the container. This will not work unless you adjust the client's workspace path or set `MOTE_SERVER_HOST=0.0.0.0` and rely on path translation. For production sandboxing, we recommend the all-in-container flow.

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `MOTE_IMAGE` | `mote:latest` | Override the Docker image tag in `run.sh` |
| `MOTE_SERVER_PORT` | `9847` | Server listen port (set inside the container) |

## Tips

- Press **Ctrl+P** for the command palette, **Ctrl+C** to quit.
- Use `/help` in the TUI for built-in commands.
- Workspace must be a directory on the host; the wrapper resolves it to an absolute path.
- To mount a different config directory, edit `run.sh` or pass custom `-v` flags.

## Limitations

- The container has no GPU access — Ollama GPU inference is not available sandboxed.
- Network access works normally (needed for remote LLM APIs like DeepSeek, GitHub Models).
- The `bash` tool runs inside the container, so commands like `curl`, `git`, `cargo` are available if installed at build time.
- File tool operations cannot escape the mounted workspace root.
