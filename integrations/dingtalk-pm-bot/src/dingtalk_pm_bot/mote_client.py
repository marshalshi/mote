from __future__ import annotations

import httpx

from .models import HistoryMessage, PmBridgeChatRequest, PmBridgeChatResponse


class MotePmClientError(Exception):
    """Base error for PM bridge failures."""


class MotePmContextWindowExceededError(MotePmClientError):
    """The underlying model rejected the request due to context size."""


class MotePmHttpError(MotePmClientError):
    """The PM bridge returned a non-success HTTP response."""


class MotePmClient:
    def __init__(
        self,
        *,
        base_url: str,
        timeout_seconds: float,
        api_token: str | None = None,
    ) -> None:
        self._base_url = base_url.rstrip("/")
        self._timeout_seconds = timeout_seconds
        self._api_token = api_token

    async def chat_pm(
        self,
        *,
        session_key: str,
        message: str,
        history: list[HistoryMessage],
    ) -> PmBridgeChatResponse:
        headers: dict[str, str] = {}
        if self._api_token:
            headers["Authorization"] = f"Bearer {self._api_token}"
        payload = PmBridgeChatRequest(
            message=message,
            history=history,
            runtime_session_key=session_key,
        )
        async with httpx.AsyncClient(timeout=self._timeout_seconds) as client:
            response = await client.post(
                f"{self._base_url}/integrations/pm/chat",
                json=payload.model_dump(),
                headers=headers,
            )
            if response.is_error:
                body = response.text
                lowered = body.lower()
                if "context window exceeds limit" in lowered:
                    raise MotePmContextWindowExceededError(body)
                raise MotePmHttpError(
                    f"PM bridge HTTP {response.status_code}: {body}"
                )
        return PmBridgeChatResponse.model_validate(response.json())
