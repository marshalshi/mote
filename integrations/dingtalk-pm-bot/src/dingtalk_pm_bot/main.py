from __future__ import annotations

import logging
import sys
from pathlib import Path

import dingtalk_stream

from .config import Settings
from .message_handler import DingTalkPmMessageHandler
from .mote_client import MotePmClient
from .session_store import SessionStore


def setup_logging(log_level: str) -> logging.Logger:
    logger = logging.getLogger("dingtalk_pm_bot")
    logger.setLevel(getattr(logging, log_level.upper(), logging.INFO))
    logger.handlers = []
    formatter = logging.Formatter(
        "%(asctime)s - %(name)s - %(levelname)s - %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    )
    handler = logging.StreamHandler(sys.stdout)
    handler.setFormatter(formatter)
    logger.addHandler(handler)
    return logger


def main() -> None:
    bot_dir = Path(__file__).resolve().parents[2]
    env_path = bot_dir / ".env"
    settings = Settings.load(env_path)
    logger = setup_logging(settings.log_level)
    session_store = SessionStore(
        settings.sessions_dir,
        settings.mote_max_history_messages,
    )
    mote_client = MotePmClient(
        base_url=settings.mote_pm_base_url,
        timeout_seconds=settings.mote_request_timeout_seconds,
        api_token=settings.mote_pm_api_token,
    )
    handler = DingTalkPmMessageHandler(
        settings=settings,
        mote_client=mote_client,
        session_store=session_store,
        logger=logger,
    )

    credential = dingtalk_stream.Credential(
        settings.dingtalk_client_id,
        settings.dingtalk_client_secret,
    )
    client = dingtalk_stream.DingTalkStreamClient(credential)
    client.register_callback_handler(
        dingtalk_stream.chatbot.ChatbotMessage.TOPIC,
        handler,
    )
    logger.info("Starting DingTalk PM bot sidecar")
    logger.info("Sessions dir: %s", settings.sessions_dir)
    client.start_forever()


if __name__ == "__main__":
    main()
