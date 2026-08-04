from __future__ import annotations

import logging
from datetime import timedelta
from typing import Any

import dingtalk_stream
from dingtalk_stream import AckMessage

from .config import Settings
from .models import HistoryMessage, PmBridgeChatResponse
from .mote_client import MotePmClient, MotePmContextWindowExceededError
from .session_keys import private_runtime_session_key
from .session_store import SessionStore


class DingTalkPmMessageHandler(dingtalk_stream.ChatbotHandler):
    def __init__(
        self,
        *,
        settings: Settings,
        mote_client: MotePmClient,
        session_store: SessionStore,
        logger: logging.Logger | None = None,
    ) -> None:
        super().__init__()
        self.settings = settings
        self.mote_client = mote_client
        self.session_store = session_store
        self.logger = logger or logging.getLogger(__name__)

    async def process(self, callback: dingtalk_stream.CallbackMessage) -> tuple:
        message: Any | None = None
        try:
            message = dingtalk_stream.ChatbotMessage.from_dict(callback.data)
            text = self._extract_text(message)
            if not text:
                self.reply_text(
                    "I can only process text messages right now.", message
                )
                return AckMessage.STATUS_OK, "OK"

            if self.settings.private_chat_only and self._is_known_group_chat(message):
                self.reply_text(
                    "This bot currently supports only 1:1 private chat.",
                    message,
                )
                return AckMessage.STATUS_OK, "OK"

            user_id = self._resolve_user_id(message)
            if not user_id:
                self.reply_text(
                    "I could not identify your DingTalk user account.",
                    message,
                )
                return AckMessage.STATUS_OK, "OK"

            if self.settings.allowed_user_ids and user_id not in self.settings.allowed_user_ids:
                self.reply_text("You are not allowed to use this bot.", message)
                return AckMessage.STATUS_OK, "OK"

            cleaned_text = " ".join(text.split())
            base_session_key = private_runtime_session_key(
                self.settings.mote_session_prefix,
                user_id,
            )

            if cleaned_text.lower() in {"clear session", "/clear", "reset session"}:
                self.session_store.clear_active_session(base_session_key)
                self.reply_text("Session cleared.", message)
                return AckMessage.STATUS_OK, "OK"

            session_key = self.session_store.active_session_key(
                base_session_key,
                inactivity_timeout=timedelta(
                    hours=self.settings.mote_session_inactivity_hours
                ),
            )

            history = self.session_store.load_history(session_key)
            bridge_response = await self._chat_with_history_fallback(
                session_key=session_key,
                message=cleaned_text,
                history=history,
            )
            reply = self._truncate_reply(bridge_response.reply)
            self.session_store.append_turn(session_key, cleaned_text, reply)
            self.session_store.touch_active_session(base_session_key, session_key)
            self.reply_text(reply, message)
            return AckMessage.STATUS_OK, "OK"
        except Exception as exc:
            self.logger.error("Failed to process DingTalk PM message: %s", exc, exc_info=True)
            if message is not None:
                try:
                    self.reply_text(
                        "Sorry, I hit an internal error while talking to mote PM mode.",
                        message,
                    )
                except Exception:
                    self.logger.error("Failed to send DingTalk error reply", exc_info=True)
            return AckMessage.STATUS_OK, "ERROR"

    def _extract_text(self, message: Any) -> str:
        if getattr(message, "text", None) and getattr(message.text, "content", None):
            return message.text.content
        if getattr(message, "markdown", None) and getattr(message.markdown, "text", None):
            return message.markdown.text
        return ""

    def _resolve_user_id(self, message: Any) -> str | None:
        return (
            getattr(message, "sender_staff_id", None)
            or getattr(message, "sender_id", None)
            or getattr(getattr(message, "hosting_context", None), "user_id", None)
        )

    def _is_known_group_chat(self, message: Any) -> bool:
        conversation_type = (
            getattr(message, "conversation_type", None)
            or getattr(message, "conversationType", None)
            or getattr(getattr(message, "conversation", None), "type", None)
        )
        if conversation_type is None:
            return False
        normalized = str(conversation_type).strip().lower()
        return normalized in {"group", "group_chat", "2"}

    def _truncate_reply(self, reply: str) -> str:
        max_chars = self.settings.dingtalk_max_reply_chars
        if len(reply) <= max_chars:
            return reply
        return f"{reply[:max_chars].rstrip()}\n\n[truncated]"

    async def _chat_with_history_fallback(
        self,
        *,
        session_key: str,
        message: str,
        history: list[HistoryMessage],
    ) -> PmBridgeChatResponse:
        candidates = self._history_candidates(history)
        last_error: Exception | None = None
        for index, candidate in enumerate(candidates):
            try:
                if index > 0:
                    self.logger.warning(
                        "Retrying PM bridge with reduced history: %s messages",
                        len(candidate),
                    )
                return await self.mote_client.chat_pm(
                    session_key=session_key,
                    message=message,
                    history=candidate,
                )
            except MotePmContextWindowExceededError as exc:
                last_error = exc
                continue
        if last_error is not None:
            raise last_error
        raise RuntimeError("No PM bridge history candidates were available")

    def _history_candidates(
        self,
        history: list[HistoryMessage],
    ) -> list[list[HistoryMessage]]:
        full = self._complete_turn_history(history)
        configured = self._fit_history(
            history,
            self.settings.mote_max_history_messages,
            self.settings.mote_max_history_chars,
        )
        short = self._fit_history(
            history,
            min(4, self.settings.mote_max_history_messages),
            min(1200, self.settings.mote_max_history_chars),
        )
        candidates: list[list[HistoryMessage]] = []
        for candidate in (full, configured, short, []):
            if not any(self._same_history(candidate, existing) for existing in candidates):
                candidates.append(candidate)
        return candidates

    def _complete_turn_history(
        self,
        history: list[HistoryMessage],
    ) -> list[HistoryMessage]:
        if len(history) % 2 == 1:
            history = history[1:]
        result: list[HistoryMessage] = []
        for index in range(0, len(history) - 1, 2):
            user_message = history[index]
            assistant_message = history[index + 1]
            if user_message.role == "user" and assistant_message.role == "assistant":
                result.extend([user_message, assistant_message])
        return result

    def _fit_history(
        self,
        history: list[HistoryMessage],
        max_messages: int,
        max_chars: int,
    ) -> list[HistoryMessage]:
        if not history or max_messages <= 0 or max_chars <= 0:
            return []
        trimmed = history[-max_messages:]
        if len(trimmed) % 2 == 1:
            trimmed = trimmed[1:]

        turns: list[tuple[HistoryMessage, HistoryMessage]] = []
        for index in range(0, len(trimmed) - 1, 2):
            user_message = trimmed[index]
            assistant_message = trimmed[index + 1]
            if user_message.role != "user" or assistant_message.role != "assistant":
                continue
            turns.append((user_message, assistant_message))

        kept_turns: list[tuple[HistoryMessage, HistoryMessage]] = []
        total_chars = 0
        for turn in reversed(turns):
            turn_chars = len(turn[0].content) + len(turn[1].content)
            if kept_turns and total_chars + turn_chars > max_chars:
                break
            if not kept_turns and turn_chars > max_chars:
                continue
            kept_turns.append(turn)
            total_chars += turn_chars
        kept_turns.reverse()

        result: list[HistoryMessage] = []
        for user_message, assistant_message in kept_turns:
            result.extend([user_message, assistant_message])
        return result

    def _same_history(
        self,
        left: list[HistoryMessage],
        right: list[HistoryMessage],
    ) -> bool:
        return [(item.role, item.content) for item in left] == [
            (item.role, item.content) for item in right
        ]
