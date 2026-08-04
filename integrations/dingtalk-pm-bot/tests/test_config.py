from pathlib import Path

from dingtalk_pm_bot.config import Settings


def test_settings_load(tmp_path: Path) -> None:
    env_path = tmp_path / ".env"
    env_path.write_text(
        "\n".join(
            [
                "DINGTALK_CLIENT_ID=client-id",
                "DINGTALK_CLIENT_SECRET=client-secret",
                "MOTE_PM_BASE_URL=http://127.0.0.1:9847",
                "PRIVATE_CHAT_ONLY=true",
                "ALLOWED_USER_IDS=user-1,user-2",
            ]
        ),
        encoding="utf-8",
    )
    settings = Settings.load(env_path)
    assert settings.dingtalk_client_id == "client-id"
    assert settings.private_chat_only is True
    assert settings.allowed_user_ids == ("user-1", "user-2")
