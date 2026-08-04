from dingtalk_pm_bot.session_keys import (
    group_runtime_session_key,
    private_runtime_session_key,
)


def test_private_runtime_session_key() -> None:
    assert (
        private_runtime_session_key("dingtalk", "user-1")
        == "dingtalk:private:user-1"
    )


def test_group_runtime_session_key() -> None:
    assert (
        group_runtime_session_key("dingtalk", "conv-1")
        == "dingtalk:group:conv-1"
    )
