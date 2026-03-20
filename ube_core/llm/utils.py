"""LLM response utilities — JSON extraction, markdown cleanup, retry."""

from typing import Type, TypeVar, Optional
from pydantic import BaseModel
from .base import LLMClient

T = TypeVar("T", bound=BaseModel)


def clean_json_response(raw: str) -> str:
    """Strip markdown code fences and extract the JSON object."""
    text = raw.strip()
    if text.startswith("```"):
        text = text.split("\n", 1)[1] if "\n" in text else text[3:]
        if text.endswith("```"):
            text = text[:-3]
        text = text.strip()
    start = text.find("{")
    end = text.rfind("}")
    if start != -1 and end != -1:
        text = text[start:end + 1]
    return text


def json_retry_parse(
    client: LLMClient,
    system: str,
    user: str,
    model_class: Type[T],
    max_retries: int = 3,
) -> Optional[T]:
    """Call LLM up to max_retries times, parse response as Pydantic model.

    Returns parsed model on success, None on all failures.
    """
    last_err = None
    for _ in range(max_retries):
        raw = client.chat(system=system, user=user)
        text = clean_json_response(raw)
        try:
            return model_class.model_validate_json(text)
        except Exception as e:
            last_err = e
    return None
