"""Interview evaluator — loads prompt templates from JSON."""

import json
import sys
from typing import List

from ube_core import Blackboard, Patch, IEvaluator
from ube_core.llm import LLMClient, json_retry_parse
from .models import EvaluatorPatch, RubricDimension
from .prompts import load_prompt

_TERMINAL = {"SATISFIED", "FATAL_FLAW"}


class InterviewEvaluator(IEvaluator):

    def __init__(self, client: LLMClient, target_dimensions: List[RubricDimension], lang: str = "zh"):
        self._client = client
        self.target_dimensions = target_dimensions
        data = load_prompt("evaluator", lang)
        self._system_template = data["system_prompt_template"]
        self._user_template = data["user_prompt_template"]

    @staticmethod
    def _get_status(node) -> str:
        return node.get("status", "INIT") if isinstance(node, dict) else getattr(node, "status", "INIT")

    @staticmethod
    def _node_to_dict(node) -> dict:
        return node.model_dump() if hasattr(node, "model_dump") else node

    def evaluate(self, board: Blackboard, user_input: str) -> Patch:
        target_ids = [d.node_id for d in self.target_dimensions]

        active_ids = [
            tid for tid in target_ids
            if tid in board.state_tree and self._get_status(board.state_tree[tid]) not in _TERMINAL
        ]
        if not active_ids:
            return Patch(updates={})

        slice_state = {
            k: self._node_to_dict(v) for k, v in board.state_tree.items() if k in active_ids
        }

        active_rules = "\n".join(
            f"  - [{d.category}] {d.node_id}: {d.eval_rule}"
            for d in self.target_dimensions if d.node_id in active_ids
        )

        ctx = board.context
        schema_hint = json.dumps(EvaluatorPatch.model_json_schema(), ensure_ascii=False, indent=2)

        last_question = ""
        for msg in reversed(board.history):
            if msg.get("role") == "assistant":
                last_question = msg["content"]
                break

        system_prompt = self._system_template.format(schema=schema_hint)
        user_prompt = self._user_template.format(
            interview_level=ctx.get("interview_level", ""),
            topic=ctx.get("topic", ""),
            global_constants=json.dumps(ctx.get("global_constants", {}), ensure_ascii=False),
            active_rules=active_rules,
            slice_state=json.dumps(slice_state, ensure_ascii=False, indent=2),
            last_question=last_question,
            user_input=user_input,
        )

        result = json_retry_parse(self._client, system_prompt, user_prompt, EvaluatorPatch)
        if result:
            return Patch(updates={"_evaluator_patch": result.model_dump()})

        print("[Evaluator] JSON parse failed after retries", file=sys.stderr)
        return Patch(updates={})
