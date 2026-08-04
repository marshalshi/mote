from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

from dotenv import load_dotenv


def _split_csv(value: str | None) -> tuple[str, ...]:
    if not value:
        return ()
    return tuple(part.strip() for part in value.split(",") if part.strip())


@dataclass(frozen=True)
class Settings:
    dingtalk_client_id: str
    dingtalk_client_secret: str
    mote_pm_base_url: str
    mote_pm_api_token: str | None
    private_chat_only: bool
    allowed_user_ids: tuple[str, ...]
    mote_session_prefix: str
    log_level: str
    mote_request_timeout_seconds: float
    mote_max_history_messages: int
    mote_max_history_chars: int
    mote_session_inactivity_hours: float
    dingtalk_max_reply_chars: int
    sessions_dir: Path
    bot_dir: Path

    @classmethod
    def load(cls, env_path: Path | None = None) -> "Settings":
        load_dotenv(env_path, override=False)
        bot_dir = (env_path.parent if env_path else Path.cwd()).resolve()
        sessions_dir = bot_dir / "data" / "sessions"
        return cls(
            dingtalk_client_id=_require_env("DINGTALK_CLIENT_ID"),
            dingtalk_client_secret=_require_env("DINGTALK_CLIENT_SECRET"),
            mote_pm_base_url=_require_env("MOTE_PM_BASE_URL").rstrip("/"),
            mote_pm_api_token=os.getenv("MOTE_PM_API_TOKEN") or None,
            private_chat_only=_parse_bool(
                os.getenv("PRIVATE_CHAT_ONLY", "true")
            ),
            allowed_user_ids=_split_csv(os.getenv("ALLOWED_USER_IDS")),
            mote_session_prefix=os.getenv("MOTE_SESSION_PREFIX", "dingtalk"),
            log_level=os.getenv("LOG_LEVEL", "INFO").upper(),
            mote_request_timeout_seconds=float(
                os.getenv("MOTE_REQUEST_TIMEOUT_SECONDS", "60")
            ),
            mote_max_history_messages=int(
                os.getenv("MOTE_MAX_HISTORY_MESSAGES", "12")
            ),
            mote_max_history_chars=int(
                os.getenv("MOTE_MAX_HISTORY_CHARS", "6000")
            ),
            mote_session_inactivity_hours=float(
                os.getenv("MOTE_SESSION_INACTIVITY_HOURS", "8")
            ),
            dingtalk_max_reply_chars=int(
                os.getenv("DINGTALK_MAX_REPLY_CHARS", "4000")
            ),
            sessions_dir=sessions_dir,
            bot_dir=bot_dir,
        )


def _require_env(name: str) -> str:
    value = os.getenv(name, "").strip()
    if not value:
        raise ValueError(f"Missing required environment variable: {name}")
    return value


def _parse_bool(value: str) -> bool:
    normalized = value.strip().lower()
    if normalized in {"1", "true", "yes", "on"}:
        return True
    if normalized in {"0", "false", "no", "off"}:
        return False
    raise ValueError(f"Invalid boolean value: {value}")
