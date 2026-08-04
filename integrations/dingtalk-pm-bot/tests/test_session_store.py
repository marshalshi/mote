from pathlib import Path
from datetime import UTC, datetime, timedelta

from dingtalk_pm_bot.session_store import SessionStore


def test_session_store_append_and_load(tmp_path: Path) -> None:
    store = SessionStore(tmp_path)
    store.append_turn("dingtalk:private:user-1", "hi", "hello")
    history = store.load_history("dingtalk:private:user-1")
    assert len(history) == 2
    assert history[0].role == "user"
    assert history[1].content == "hello"


def test_session_store_preserves_full_history_on_disk(tmp_path: Path) -> None:
    store = SessionStore(tmp_path, max_history_messages=2)
    key = "dingtalk:private:user-1"
    store.append_turn(key, "u1", "a1")
    store.append_turn(key, "u2", "a2")

    history = store.load_history(key)
    assert [(item.role, item.content) for item in history] == [
        ("user", "u1"),
        ("assistant", "a1"),
        ("user", "u2"),
        ("assistant", "a2"),
    ]


def test_session_store_clear(tmp_path: Path) -> None:
    store = SessionStore(tmp_path)
    key = "dingtalk:private:user-2"
    store.append_turn(key, "hi", "hello")
    store.clear(key)
    assert store.load_history(key) == []


def test_active_session_key_reuses_within_timeout(tmp_path: Path) -> None:
    store = SessionStore(tmp_path)
    base_key = "dingtalk:private:user-1"
    first = datetime(2026, 8, 2, 10, 0, tzinfo=UTC)
    second = first + timedelta(hours=7, minutes=59)

    key1 = store.active_session_key(
        base_key,
        now=first,
        inactivity_timeout=timedelta(hours=8),
    )
    store.touch_active_session(base_key, key1, now=first)
    key2 = store.active_session_key(
        base_key,
        now=second,
        inactivity_timeout=timedelta(hours=8),
    )

    assert key2 == key1


def test_active_session_key_rolls_after_timeout(tmp_path: Path) -> None:
    store = SessionStore(tmp_path)
    base_key = "dingtalk:private:user-1"
    first = datetime(2026, 8, 2, 10, 0, tzinfo=UTC)
    second = first + timedelta(hours=8, seconds=1)

    key1 = store.active_session_key(
        base_key,
        now=first,
        inactivity_timeout=timedelta(hours=8),
    )
    store.touch_active_session(base_key, key1, now=first)
    key2 = store.active_session_key(
        base_key,
        now=second,
        inactivity_timeout=timedelta(hours=8),
    )

    assert key2 != key1


def test_clear_active_session_rotates_next_key(tmp_path: Path) -> None:
    store = SessionStore(tmp_path)
    base_key = "dingtalk:private:user-1"
    now = datetime(2026, 8, 2, 10, 0, tzinfo=UTC)
    key1 = store.active_session_key(
        base_key,
        now=now,
        inactivity_timeout=timedelta(hours=8),
    )
    store.append_turn(key1, "hi", "hello")

    store.clear_active_session(base_key)
    key2 = store.active_session_key(
        base_key,
        now=now + timedelta(seconds=1),
        inactivity_timeout=timedelta(hours=8),
    )

    assert key2 != key1
    assert store.load_history(key1) == []
