"""Application configuration via environment variables."""

from typing import Literal

from pydantic import SecretStr
from pydantic_settings import BaseSettings


class Settings(BaseSettings):
    """Central configuration loaded from environment variables."""

    # LLM
    llm_provider: Literal["anthropic", "openai", "gemini", "mock"] = "openai"
    llm_api_key: SecretStr = SecretStr("")
    llm_model: str = "gpt-4o"

    model_config = {"env_prefix": "UBE_", "env_file": ".env"}


settings = Settings()
