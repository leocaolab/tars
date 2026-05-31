"""OpenAI (GPT) LLM client."""

from openai import OpenAI, OpenAIError

from .base import LLMClient
from .errors import LLMClientError


class OpenAIClient(LLMClient):

    def __init__(self, api_key: str, model: str):
        self._client = OpenAI(api_key=api_key)
        self._model = model

    @staticmethod
    def _first_content(response) -> str:
        if not response.choices:
            raise LLMClientError("OpenAI response contained no choices")
        return response.choices[0].message.content or ""

    def chat(self, system: str, user: str, max_tokens: int = 16_384) -> str:
        try:
            response = self._client.chat.completions.create(
                model=self._model,
                max_tokens=max_tokens,
                messages=[
                    {"role": "system", "content": system},
                    {"role": "user", "content": user},
                ],
            )
        except OpenAIError as e:
            raise LLMClientError(f"OpenAI chat call failed: {e}") from e
        return self._first_content(response)

    def chat_multi(self, system: str, messages: list[dict[str, str]], max_tokens: int = 16_384) -> str:
        all_messages = [{"role": "system", "content": system}, *messages]
        try:
            response = self._client.chat.completions.create(
                model=self._model,
                max_tokens=max_tokens,
                messages=all_messages,
            )
        except OpenAIError as e:
            raise LLMClientError(f"OpenAI chat_multi call failed: {e}") from e
        return self._first_content(response)
