"""EN session factory — reuses framework LayeredSessionFactory with English overrides."""

from pathlib import Path
from ube_core.session import LayeredSessionFactory

# Reuse legacy loaders from ZH app
from interview_app.session_factory import load_rubric, load_blueprint, SessionFactory  # noqa: F401


class SessionFactoryV2(LayeredSessionFactory):
    """English-specific overrides."""

    def __init__(self, base_dir: str = None):
        super().__init__(base_dir or str(Path(__file__).parent))

    def build_context(self, blueprint: dict, rubric_dims: list[dict]) -> dict:
        return {
            "topic": blueprint.get("title", ""),
            "interview_level": blueprint.get("interview_level", "Unknown"),
            "global_constants": blueprint.get("global_constants", {}),
            "rubric": rubric_dims,
        }

    def build_system_prompt(self, blueprint: dict) -> str | None:
        level = blueprint.get("interview_level", "")
        title = blueprint.get("title", "")
        return (
            f"You are an extremely rigorous {level}-level architecture interviewer. "
            f"Today you are evaluating a candidate on: {title}."
        )
