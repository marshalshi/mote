from __future__ import annotations

import hashlib
import json
import re
from datetime import UTC, datetime, timedelta
from pathlib import Path

from .models import HistoryMessage


class SessionStore:
    def __init__(self, root: Path, max_history_messages: int = 20) -> None:
        self.root = root
        self.max_history_messages = max(
            2, max_history_messages - (max_history_messages % 2)
        )
        self.root.mkdir(parents=True, exist_ok=True)

    def active_session_key(
        self,
        base_session_key: str,
        *,
        now: datetime | None = None,
        inactivity_timeout: timedelta,
    ) -> str:
        now = _normalize_time(now)
        state = self._load_state(base_session_key)
        if state is not None:
            active_key = state.get("active_session_key")
            last_activity = _parse_time(state.get("last_activity"))
            if active_key and last_activity and now - last_activity < inactivity_timeout:
                return active_key

        active_key = f"{base_session_key}:session:{now.strftime('%Y%m%d%H%M%S%f')}"
        self._write_state(base_session_key, active_key, now)
        return active_key

    def touch_active_session(
        self,
        base_session_key: str,
        active_session_key: str,
        *,
        now: datetime | None = None,
    ) -> None:
        self._write_state(base_session_key, active_session_key, _normalize_time(now))

    def load_history(self, session_key: str) -> list[HistoryMessage]:
        path = self._path_for(session_key)
        if not path.exists():
            return []
        data = json.loads(path.read_text(encoding="utf-8"))
        return [HistoryMessage.model_validate(item) for item in data.get("history", [])]

    def append_turn(
        self,
        session_key: str,
        user_message: str,
        assistant_message: str,
    ) -> list[HistoryMessage]:
        history = self.load_history(session_key)
        history.extend(
            [
                HistoryMessage(role="user", content=user_message),
                HistoryMessage(role="assistant", content=assistant_message),
            ]
        )
        self._path_for(session_key).write_text(
            json.dumps(
                {
                    "session_key": session_key,
                    "history": [item.model_dump() for item in history],
                },
                ensure_ascii=False,
                indent=2,
            ),
            encoding="utf-8",
        )
        return history

    def clear(self, session_key: str) -> None:
        path = self._path_for(session_key)
        if path.exists():
            path.unlink()

    def clear_active_session(self, base_session_key: str) -> None:
        state = self._load_state(base_session_key)
        if state is not None:
            active_key = state.get("active_session_key")
            if isinstance(active_key, str):
                self.clear(active_key)
        state_path = self._state_path_for(base_session_key)
        if state_path.exists():
            state_path.unlink()

    def _path_for(self, session_key: str) -> Path:
        safe = re.sub(r"[^a-zA-Z0-9._-]", "_", session_key)
        digest = hashlib.sha1(session_key.encode("utf-8")).hexdigest()[:12]
        return self.root / f"{safe}-{digest}.json"

    def _state_path_for(self, base_session_key: str) -> Path:
        safe = re.sub(r"[^a-zA-Z0-9._-]", "_", base_session_key)
        digest = hashlib.sha1(base_session_key.encode("utf-8")).hexdigest()[:12]
        return self.root / f"{safe}-{digest}.state.json"

    def _load_state(self, base_session_key: str) -> dict | None:
        path = self._state_path_for(base_session_key)
        if not path.exists():
            return None
        return json.loads(path.read_text(encoding="utf-8"))

    def _write_state(
        self,
        base_session_key: str,
        active_session_key: str,
        now: datetime,
    ) -> None:
        self._state_path_for(base_session_key).write_text(
            json.dumps(
                {
                    "base_session_key": base_session_key,
                    "active_session_key": active_session_key,
                    "last_activity": now.isoformat(),
                },
                ensure_ascii=False,
                indent=2,
            ),
            encoding="utf-8",
        )


def _normalize_time(value: datetime | None) -> datetime:
    if value is None:
        return datetime.now(UTC)
    if value.tzinfo is None:
        return value.replace(tzinfo=UTC)
    return value.astimezone(UTC)


def _parse_time(value: object) -> datetime | None:
    if not isinstance(value, str):
        return None
    try:
        return _normalize_time(datetime.fromisoformat(value))
    except ValueError:
        return None
