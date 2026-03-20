"""Abstract LLM client interface."""

from abc import ABC, abstractmethod


class LLMClient(ABC):
    """Common interface for all LLM providers."""

    @abstractmethod
    def chat(self, system: str, user: str, max_tokens: int = 16_384) -> str:
        """Send a single-turn chat and return the response text."""

    @abstractmethod
    def chat_multi(
        self,
        system: str,
        messages: list[dict[str, str]],
        max_tokens: int = 16_384,
    ) -> str:
        """Send a multi-turn chat and return the response text.

        Args:
            system: System prompt.
            messages: List of {"role": "user"|"assistant", "content": "..."}.
            max_tokens: Maximum tokens in the response.

        Returns:
            The model's text response.
        """
