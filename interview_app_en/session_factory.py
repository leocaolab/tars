"""Session factory: blueprint + rubric -> initial Blackboard."""

import json
import uuid
from pathlib import Path
from typing import Dict

from ube_core import Blackboard
from .models import ProblemBlueprint, RubricDimension, RubricManifest, RubricNode


def load_rubric(path: str = None) -> RubricManifest:
    if path is None:
        path = str(Path(__file__).parent / "rubric.json")
    with open(path, "r", encoding="utf-8") as f:
        return RubricManifest.model_validate(json.load(f))


def load_blueprint(path: str) -> ProblemBlueprint:
    with open(path, "r", encoding="utf-8") as f:
        return ProblemBlueprint.model_validate(json.load(f))


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
                raise ValueError(
                    f"Blueprint references unknown rubric dimension: {bp_node.id}\n"
                    f"Available: {list(self._dim_index.keys())}"
                )
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
            f"You are an extremely rigorous {blueprint.interview_level}-level architecture interviewer. "
            f"Today you are evaluating a candidate on: {blueprint.title}."
        )
        history = [{"role": "system", "content": system_prompt}]

        session_id = f"intv_{uuid.uuid4().hex[:8]}"
        return Blackboard(
            session_id=session_id,
            context=context,
            state_tree=state_tree,
            history=history,
        )
