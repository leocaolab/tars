"""Google Gemini LLM client."""

from google import genai
from google.genai import types

from .base import LLMClient

THINKING_BUDGET = 8192


class GeminiClient(LLMClient):

    def __init__(self, api_key: str, model: str):
        super().__init__()
        self._client = genai.Client(api_key=api_key)
        self._model = model

    def chat(self, system: str, user: str, max_tokens: int = 16_384) -> str:
        return self.chat_multi(system, [{"role": "user", "content": user}], max_tokens)

    def chat_multi(self, system: str, messages: list[dict[str, str]], max_tokens: int = 16_384) -> str:
        config = types.GenerateContentConfig(
            system_instruction=system,
            max_output_tokens=max_tokens,
            thinking_config=types.ThinkingConfig(thinking_budget=THINKING_BUDGET),
        )

        # Convert messages to Gemini content format
        contents = []
        for msg in messages:
            role = "model" if msg["role"] == "assistant" else "user"
            contents.append(types.Content(
                role=role,
                parts=[types.Part(text=msg["content"])],
            ))

        response = self._client.models.generate_content(
            model=self._model,
            contents=contents,
            config=config,
        )
        self.call_count += 1

        # Track token usage
        usage = getattr(response, "usage_metadata", None)
        if usage:
            self.tokens_in += getattr(usage, "prompt_token_count", 0) or 0
            self.tokens_out += getattr(usage, "candidates_token_count", 0) or 0
            self.tokens_cached += getattr(usage, "cached_content_token_count", 0) or 0

        # Extract text parts, skipping thinking parts
        parts = []
        for candidate in response.candidates:
            if candidate.content and candidate.content.parts:
                for part in candidate.content.parts:
                    if part.text and not getattr(part, "thought", False):
                        parts.append(part.text)
        return "\n".join(parts) if parts else ""
