# DingTalk PM Bot Sidecar

This folder is reserved for the **isolated Python sidecar** that connects DingTalk Stream mode to the `pm` agent inside `mote`.

It is intentionally separate from the Rust crates.

## Purpose

The sidecar will:

- connect to DingTalk using **Stream / WebSocket mode**
- receive **1:1 chat messages**
- forward them to local `mote`
- force all requests to the **`pm` agent only**
- send the reply back to DingTalk

## Folder-local environment file

Add the runtime environment file **here**:

```text
integrations/dingtalk-pm-bot/.env
```

Create it by copying the example file in the same folder:

```bash
cp integrations/dingtalk-pm-bot/.env.example integrations/dingtalk-pm-bot/.env
```

Do **not** put DingTalk secrets in the repo root `.env`.
Do **not** put DingTalk secrets in `~/.config/mote/auth.json`.

For this sidecar, the correct place is:

```text
integrations/dingtalk-pm-bot/.env
```

## Environment variables

### Required

| Variable | Purpose |
|---|---|
| `DINGTALK_CLIENT_ID` | DingTalk Stream app/bot client ID (app key) |
| `DINGTALK_CLIENT_SECRET` | DingTalk Stream app/bot client secret |
| `MOTE_PM_BASE_URL` | Local `mote` server base URL the sidecar will call |

### Optional but recommended

| Variable | Default | Purpose |
|---|---:|---|
| `PRIVATE_CHAT_ONLY` | `true` | Restrict the bot to 1:1 chat only |
| `ALLOWED_USER_IDS` | empty | Comma-separated DingTalk user IDs allowed to use the bot |
| `MOTE_SESSION_PREFIX` | `dingtalk` | Prefix used when building mote runtime session keys |
| `LOG_LEVEL` | `INFO` | Sidecar log verbosity |
| `MOTE_REQUEST_TIMEOUT_SECONDS` | `60` | Timeout for local calls into `mote` |
| `MOTE_MAX_HISTORY_MESSAGES` | `12` | Max history messages sent to `mote` per request |
| `MOTE_MAX_HISTORY_CHARS` | `6000` | Soft cap for total history characters sent to `mote` |
| `DINGTALK_MAX_REPLY_CHARS` | `4000` | Soft max reply length returned to DingTalk |
| `MOTE_PM_API_TOKEN` | empty | Reserved for future local bridge auth if needed |

## Example `.env`

```env
DINGTALK_CLIENT_ID=dingxxxxxxxx
DINGTALK_CLIENT_SECRET=xxxxxxxxxxxxxxxx
MOTE_PM_BASE_URL=http://127.0.0.1:9847
PRIVATE_CHAT_ONLY=true
ALLOWED_USER_IDS=manager_user_id_1
MOTE_SESSION_PREFIX=dingtalk
LOG_LEVEL=INFO
MOTE_REQUEST_TIMEOUT_SECONDS=60
MOTE_MAX_HISTORY_MESSAGES=12
MOTE_MAX_HISTORY_CHARS=6000
DINGTALK_MAX_REPLY_CHARS=4000
```

## Planned runtime model

```text
DingTalk private chat
  -> Python sidecar (this folder)
  -> local mote PM bridge
  -> pm agent only
  -> PM SQLite ground truth
  -> reply back to DingTalk
```

## Planned Python environment

This sidecar will use its **own** Python environment with `uv`.

Expected local setup:

```bash
cd integrations/dingtalk-pm-bot
uv venv
uv sync
```

The virtual environment should remain local to this folder.

## Project layout

```text
integrations/dingtalk-pm-bot/
  .env.example
  .python-version
  pyproject.toml
  README.md
  src/
    dingtalk_pm_bot/
      config.py
      main.py
      message_handler.py
      models.py
      mote_client.py
      session_keys.py
      session_store.py
  tests/
```

## Local bridge contract

The sidecar talks to this local `mote` endpoint:

```text
POST /integrations/pm/chat
```

That endpoint is **PM-only** and always routes the request to the `pm` agent.

## Planned run command

After creating `.env` and syncing dependencies:

```bash
cd integrations/dingtalk-pm-bot
uv run dingtalk-pm-bot
```

## Notes

- We are using DingTalk **Stream mode**, so no public callback endpoint is required.
- This sidecar is planned to talk only to the `pm` agent, not `build` or any other agent.
- Google Sheets sync is intentionally deferred for now.
