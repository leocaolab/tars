"""Session factory — uses framework's LayeredSessionFactory with interview overrides."""

import json
from pathlib import Path
from typing import Dict

from ube_core import Blackboard
from ube_core.session import LayeredSessionFactory
from .models import ProblemBlueprint, RubricDimension, RubricManifest, RubricNode


# ==========================================
# Legacy loaders (backward compatible)
# ==========================================

def load_rubric(path: str = None) -> RubricManifest:
    if path is None:
        path = str(Path(__file__).parent / "rubric.json")
    with open(path, "r", encoding="utf-8") as f:
        return RubricManifest.model_validate(json.load(f))


def load_blueprint(path: str) -> ProblemBlueprint:
    with open(path, "r", encoding="utf-8") as f:
        return ProblemBlueprint.model_validate(json.load(f))


# ==========================================
# V2: Three-layer factory (extends framework)
# ==========================================

class SessionFactoryV2(LayeredSessionFactory):
    """Interview-specific overrides for context and system prompt."""

    def __init__(self, base_dir: str = None, lang: str = "zh"):
        super().__init__(base_dir or str(Path(__file__).parent))
        self._lang = lang

    def build_context(self, blueprint: dict, rubric_dims: list[dict]) -> dict:
        return {
            "topic": blueprint.get("title", ""),
            "interview_level": blueprint.get("interview_level", "Unknown"),
            "global_constants": blueprint.get("global_constants", {}),
            "rubric": rubric_dims,
        }

    def build_system_prompt(self, blueprint: dict) -> str | None:
        from .prompts import load_prompt
        data = load_prompt("session", self._lang)
        return data["system_prompt_template"].format(
            level=blueprint.get("interview_level", ""),
            title=blueprint.get("title", ""),
        )


# ==========================================
# Legacy SessionFactory (backward compatible)
# ==========================================

class SessionFactory:

    def __init__(self, rubric: RubricManifest):
        self.rubric = rubric
        self._dim_index: Dict[str, RubricDimension] = {
            d.node_id: d for d in rubric.dimensions
        }

    def create(self, blueprint: ProblemBlueprint) -> Blackboard:
        referenced_dims = []
        for bp_node in blueprint.rubric_nodes:
            dim = self._dim_index.get(bp_node.id)
            if dim is None:
                raise ValueError(f"Unknown dimension: {bp_node.id}")
            referenced_dims.append(dim)

        state_tree = {}
        for bp_node in blueprint.rubric_nodes:
            node = RubricNode(probe_suggestion=bp_node.initial_probe)
            state_tree[bp_node.id] = node.model_dump()

        context = {
            "topic": blueprint.title,
            "interview_level": blueprint.interview_level,
            "global_constants": blueprint.global_constants,
            "rubric": [d.model_dump() for d in referenced_dims],
        }

        system_prompt = (
            f"你是一位极其严苛的 {blueprint.interview_level} 级架构面试官。"
            f"今天你要考察候选人设计：{blueprint.title}。"
        )

        import uuid
        return Blackboard(
            session_id=f"intv_{uuid.uuid4().hex[:8]}",
            context=context,
            state_tree=state_tree,
            history=[{"role": "system", "content": system_prompt}],
        )
