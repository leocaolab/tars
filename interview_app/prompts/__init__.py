"""Prompt loader — loads prompts from JSON files for i18n and cross-language reuse."""

import json
from pathlib import Path
from functools import lru_cache

PROMPTS_DIR = Path(__file__).parent


@lru_cache(maxsize=32)
def load_prompt(name: str, lang: str = "zh") -> dict:
    """Load a prompt JSON file by name and language.

    Args:
        name: prompt file name without extension (actor, candidate, evaluator, reporter, session)
        lang: language code (zh, en)

    Returns:
        Parsed JSON dict.
    """
    path = PROMPTS_DIR / lang / f"{name}.json"
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)
