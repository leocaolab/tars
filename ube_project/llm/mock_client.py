"""Mock LLM client for testing."""

from .base import LLMClient


class MockClient(LLMClient):
    """Records the prompt it receives and returns a canned response."""

    def __init__(self, api_key: str = "", model: str = "mock"):
        self.api_key = api_key
        self.model = model
        self.last_system: str = ""
        self.last_user: str = ""
        self.last_messages: list[dict[str, str]] = []
        self.call_count: int = 0
        self.response: str = "LGTM — no issues found."

    def chat(self, system: str, user: str, max_tokens: int = 16_384) -> str:
        self.last_system = system
        self.last_user = user
        self.call_count += 1
        return self.response

    def chat_multi(self, system: str, messages: list[dict[str, str]], max_tokens: int = 16_384) -> str:
        self.last_system = system
        self.last_messages = messages
        self.last_user = messages[-1]["content"] if messages else ""
        self.call_count += 1
        return self.response
