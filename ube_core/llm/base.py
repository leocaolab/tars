"""Abstract LLM client interface with usage tracking."""

from abc import ABC, abstractmethod


class LLMClient(ABC):
    """Common interface for all LLM providers."""

    def __init__(self):
        self.tokens_in: int = 0
        self.tokens_out: int = 0
        self.tokens_cached: int = 0
        self.call_count: int = 0

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
        """Send a multi-turn chat and return the response text."""

    @property
    def usage_summary(self) -> dict:
        total = self.tokens_in + self.tokens_out
        cache_ratio = (self.tokens_cached / self.tokens_in * 100) if self.tokens_in > 0 else 0.0
        return {
            "calls": self.call_count,
            "tokens_in": self.tokens_in,
            "tokens_out": self.tokens_out,
            "tokens_total": total,
            "tokens_cached": self.tokens_cached,
            "cache_hit_ratio_pct": round(cache_ratio, 1),
        }
