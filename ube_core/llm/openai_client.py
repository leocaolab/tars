"""OpenAI (GPT) LLM client."""

from openai import OpenAI

from .base import LLMClient


class OpenAIClient(LLMClient):

    def __init__(self, api_key: str, model: str):
        super().__init__()
        self._client = OpenAI(api_key=api_key)
        self._model = model

    def _track(self, response):
        self.call_count += 1
        usage = getattr(response, "usage", None)
        if usage:
            self.tokens_in += getattr(usage, "prompt_tokens", 0) or 0
            self.tokens_out += getattr(usage, "completion_tokens", 0) or 0
            cached = getattr(usage, "prompt_tokens_details", None)
            if cached:
                self.tokens_cached += getattr(cached, "cached_tokens", 0) or 0

    def chat(self, system: str, user: str, max_tokens: int = 16_384) -> str:
        response = self._client.chat.completions.create(
            model=self._model,
            max_tokens=max_tokens,
            messages=[
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        )
        self._track(response)
        return response.choices[0].message.content or ""

    def chat_multi(self, system: str, messages: list[dict[str, str]], max_tokens: int = 16_384) -> str:
        all_messages = [{"role": "system", "content": system}, *messages]
        response = self._client.chat.completions.create(
            model=self._model,
            max_tokens=max_tokens,
            messages=all_messages,
        )
        self._track(response)
        return response.choices[0].message.content or ""
