"""Anthropic (Claude) LLM client."""

import anthropic

from .base import LLMClient


class AnthropicClient(LLMClient):

    def __init__(self, api_key: str, model: str):
        super().__init__()
        self._client = anthropic.Anthropic(api_key=api_key)
        self._model = model

    def _track(self, response):
        self.call_count += 1
        usage = getattr(response, "usage", None)
        if usage:
            self.tokens_in += getattr(usage, "input_tokens", 0) or 0
            self.tokens_out += getattr(usage, "output_tokens", 0) or 0
            self.tokens_cached += getattr(usage, "cache_read_input_tokens", 0) or 0

    def chat(self, system: str, user: str, max_tokens: int = 16_384) -> str:
        response = self._client.messages.create(
            model=self._model,
            max_tokens=max_tokens,
            system=system,
            messages=[{"role": "user", "content": user}],
        )
        self._track(response)
        return response.content[0].text

    def chat_multi(self, system: str, messages: list[dict[str, str]], max_tokens: int = 16_384) -> str:
        response = self._client.messages.create(
            model=self._model,
            max_tokens=max_tokens,
            system=system,
            messages=messages,
        )
        self._track(response)
        return response.content[0].text
