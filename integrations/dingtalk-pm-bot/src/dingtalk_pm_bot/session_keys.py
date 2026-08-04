from __future__ import annotations


def private_runtime_session_key(prefix: str, user_id: str) -> str:
    return f"{prefix}:private:{user_id}"


def group_runtime_session_key(prefix: str, conversation_id: str) -> str:
    return f"{prefix}:group:{conversation_id}"
