from __future__ import annotations

from pydantic import BaseModel, Field


class HistoryMessage(BaseModel):
    role: str
    content: str


class PmBridgeChatRequest(BaseModel):
    message: str
    history: list[HistoryMessage] = Field(default_factory=list)
    runtime_session_key: str


class PmBridgeChatResponse(BaseModel):
    reply: str
    tokens_input: int
    tokens_output: int
    status: str
