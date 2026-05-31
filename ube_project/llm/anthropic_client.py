"""Anthropic (Claude) LLM client."""

import anthropic

from .base import LLMClient
from .errors import LLMClientError


class AnthropicClient(LLMClient):

    def __init__(self, api_key: str, model: str):
        self._client = anthropic.Anthropic(api_key=api_key)
        self._model = model

    @staticmethod
    def _first_text(response) -> str:
        if not response.content:
            raise LLMClientError("Anthropic response contained no content blocks")
        return response.content[0].text

    def chat(self, system: str, user: str, max_tokens: int = 16_384) -> str:
        try:
            response = self._client.messages.create(
                model=self._model,
                max_tokens=max_tokens,
                system=system,
                messages=[{"role": "user", "content": user}],
            )
        except anthropic.AnthropicError as e:
            raise LLMClientError(f"Anthropic chat call failed: {e}") from e
        return self._first_text(response)

    def chat_multi(self, system: str, messages: list[dict[str, str]], max_tokens: int = 16_384) -> str:
        try:
            response = self._client.messages.create(
                model=self._model,
                max_tokens=max_tokens,
                system=system,
                messages=messages,
            )
        except anthropic.AnthropicError as e:
            raise LLMClientError(f"Anthropic chat_multi call failed: {e}") from e
        return self._first_text(response)
