"""Application configuration via environment variables."""

from pydantic_settings import BaseSettings


class Settings(BaseSettings):
    """Central configuration loaded from environment variables."""

    # LLM
    llm_provider: str = "openai"  # anthropic | openai | gemini | mock
    llm_api_key: str = ""
    llm_model: str = "gpt-4o"

    model_config = {"env_prefix": "UBE_", "env_file": ".env"}


settings = Settings()
