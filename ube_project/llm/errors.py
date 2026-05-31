"""Shared exception types for LLM clients."""


class LLMClientError(Exception):
    """Raised when an LLM provider call fails or returns an unusable response."""
