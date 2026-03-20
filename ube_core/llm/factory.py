"""Factory: create the right LLM client from config."""

from .base import LLMClient

_REGISTRY = {
    "anthropic": ("ube_core.llm.anthropic_client", "AnthropicClient"),
    "openai": ("ube_core.llm.openai_client", "OpenAIClient"),
    "gemini": ("ube_core.llm.gemini_client", "GeminiClient"),
    "mock": ("ube_core.llm.mock_client", "MockClient"),
}


def create_client(provider: str, api_key: str, model: str) -> LLMClient:
    if provider not in _REGISTRY:
        supported = ", ".join(sorted(_REGISTRY))
        raise ValueError(f"Unknown provider '{provider}'. Supported: {supported}")

    module_path, class_name = _REGISTRY[provider]

    import importlib
    module = importlib.import_module(module_path)
    cls = getattr(module, class_name)
    return cls(api_key=api_key, model=model)
