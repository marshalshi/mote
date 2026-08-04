from __future__ import annotations

from types import SimpleNamespace

import pytest

from dingtalk_pm_bot.config import Settings
from dingtalk_pm_bot.message_handler import DingTalkPmMessageHandler
from dingtalk_pm_bot.models import HistoryMessage, PmBridgeChatResponse
from dingtalk_pm_bot.mote_client import MotePmContextWindowExceededError
from dingtalk_pm_bot.session_store import SessionStore


class FakeMoteClient:
    def __init__(self, responses):
        self._responses = list(responses)
        self.calls: list[list[HistoryMessage]] = []
        self.session_keys: list[str] = []

    async def chat_pm(self, *, session_key: str, message: str, history: list[HistoryMessage]):
        self.session_keys.append(session_key)
        self.calls.append(history)
        response = self._responses.pop(0)
        if isinstance(response, Exception):
            raise response
        return response


def make_settings(tmp_path) -> Settings:
    return Settings(
        dingtalk_client_id="id",
        dingtalk_client_secret="secret",
        mote_pm_base_url="http://127.0.0.1:9847",
        mote_pm_api_token=None,
        private_chat_only=True,
        allowed_user_ids=(),
        mote_session_prefix="dingtalk",
        log_level="INFO",
        mote_request_timeout_seconds=60.0,
        mote_max_history_messages=12,
        mote_max_history_chars=100,
        mote_session_inactivity_hours=8,
        dingtalk_max_reply_chars=4000,
        sessions_dir=tmp_path / "sessions",
        bot_dir=tmp_path,
    )


def make_handler(tmp_path, mote_client) -> DingTalkPmMessageHandler:
    return DingTalkPmMessageHandler(
        settings=make_settings(tmp_path),
        mote_client=mote_client,
        session_store=SessionStore(tmp_path / "sessions", 12),
    )


def test_fit_history_keeps_complete_recent_turns(tmp_path) -> None:
    handler = make_handler(tmp_path, FakeMoteClient([]))
    history = [
        HistoryMessage(role="user", content="u1"),
        HistoryMessage(role="assistant", content="a1"),
        HistoryMessage(role="user", content="u2"),
        HistoryMessage(role="assistant", content="a2"),
    ]
    kept = handler._fit_history(history, 4, 4)
    assert [(m.role, m.content) for m in kept] == [
        ("user", "u2"),
        ("assistant", "a2"),
    ]


@pytest.mark.anyio
async def test_chat_with_history_fallback_retries_on_context_error(tmp_path) -> None:
    client = FakeMoteClient(
        [
            MotePmContextWindowExceededError("too large"),
            PmBridgeChatResponse(
                reply="ok",
                tokens_input=1,
                tokens_output=1,
                status="done",
            ),
        ]
    )
    handler = make_handler(tmp_path, client)
    history = [
        HistoryMessage(role="user", content="u1" * 50),
        HistoryMessage(role="assistant", content="a1" * 50),
        HistoryMessage(role="user", content="u2"),
        HistoryMessage(role="assistant", content="a2"),
    ]
    response = await handler._chat_with_history_fallback(
        session_key="dingtalk:private:user-1",
        message="hello",
        history=history,
    )
    assert response.reply == "ok"
    assert len(client.calls) == 2
    assert len(client.calls[1]) <= len(client.calls[0])
