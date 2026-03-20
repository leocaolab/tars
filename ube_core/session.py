"""Generic three-layer session factory for blackboard initialization."""

import json
import uuid
from pathlib import Path
from typing import Any

from .types import Blackboard


def load_rubric_layer(path: str) -> dict[str, dict]:
    """Load a rubric layer JSON file. Returns {node_id: dimension_dict}."""
    with open(path, "r", encoding="utf-8") as f:
        data = json.load(f)
    return {item["node_id"]: item for item in data.get("rubrics", [])}


class LayeredSessionFactory:
    """Three-layer session factory: Universal + Domain + Inline Custom.

    Business apps subclass and override build_context() and build_system_prompt()
    to inject domain-specific context keys and system prompts.
    """

    def __init__(self, base_dir: str | Path = "."):
        self._base_dir = Path(base_dir)

    def create(self, blueprint_path: str) -> Blackboard:
        with open(blueprint_path, "r", encoding="utf-8") as f:
            bp = json.load(f)

        # Layer 1 + 2: load rubric dictionaries
        rubric_dict: dict[str, dict] = {}
        for layer_path in bp.get("rubric_layers", []):
            full_path = str(self._base_dir / layer_path)
            rubric_dict.update(load_rubric_layer(full_path))

        # Resolve nodes: inline custom OR dictionary lookup
        state_tree: dict[str, Any] = {}
        rubric_dims: list[dict] = []

        for node_req in bp.get("rubric_nodes", []):
            node_id = node_req["id"]

            if "eval_rule" in node_req:
                dim_meta = {
                    "node_id": node_id,
                    "category": node_req.get("category", "Custom"),
                    "priority": node_req.get("priority", 50),
                    "phase": node_req.get("phase", "any"),
                    "prerequisites": node_req.get("prerequisites", []),
                    "eval_rule": node_req["eval_rule"],
                }
            elif node_id in rubric_dict:
                dim_meta = rubric_dict[node_id].copy()
            else:
                continue

            rubric_dims.append(dim_meta)
            state_tree[node_id] = {
                "status": "INIT",
                "positive_signals": [],
                "negative_signals": [],
                "probe_suggestion": node_req.get("initial_probe"),
            }

        context = self.build_context(bp, rubric_dims)
        history = []
        sys_prompt = self.build_system_prompt(bp)
        if sys_prompt:
            history.append({"role": "system", "content": sys_prompt})

        return Blackboard(
            session_id=f"sess_{uuid.uuid4().hex[:8]}",
            context=context,
            state_tree=state_tree,
            history=history,
        )

    def build_context(self, blueprint: dict, rubric_dims: list[dict]) -> dict:
        """Override to customize context keys. Default: topic + rubric."""
        return {
            "topic": blueprint.get("title", ""),
            "rubric": rubric_dims,
        }

    def build_system_prompt(self, blueprint: dict) -> str | None:
        """Override to customize system prompt. Default: None."""
        return None
