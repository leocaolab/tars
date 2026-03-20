"""Interview evaluator — implements IEvaluator."""

import json
import sys
from typing import List

from ube_core import Blackboard, Patch, IEvaluator
from ube_core.llm import LLMClient
from .models import EvaluatorPatch, RubricDimension

_TERMINAL = {"SATISFIED", "FATAL_FLAW"}


class InterviewEvaluator(IEvaluator):

    def __init__(self, client: LLMClient, target_dimensions: List[RubricDimension]):
        self._client = client
        self.target_dimensions = target_dimensions

    @staticmethod
    def _get_status(node) -> str:
        if isinstance(node, dict):
            return node.get("status", "INIT")
        return getattr(node, "status", "INIT")

    @staticmethod
    def _node_to_dict(node) -> dict:
        if hasattr(node, "model_dump"):
            return node.model_dump()
        return node

    def evaluate(self, board: Blackboard, user_input: str) -> Patch:
        target_ids = [d.node_id for d in self.target_dimensions]

        active_ids = [
            tid for tid in target_ids
            if tid in board.state_tree
            and self._get_status(board.state_tree[tid]) not in _TERMINAL
        ]
        if not active_ids:
            return Patch(updates={})

        slice_state = {
            k: self._node_to_dict(v)
            for k, v in board.state_tree.items()
            if k in active_ids
        }

        active_rules = "\n".join(
            f"  - [{d.category}] {d.node_id}: {d.eval_rule}"
            for d in self.target_dimensions
            if d.node_id in active_ids
        )

        ctx = board.context
        schema_hint = json.dumps(
            EvaluatorPatch.model_json_schema(), ensure_ascii=False, indent=2
        )

        system_prompt = (
            "You are a rigorous system design evaluator. Output ONLY valid JSON.\n"
            f"Strictly follow this JSON Schema:\n{schema_hint}"
        )

        last_question = ""
        for msg in reversed(board.history):
            if msg.get("role") == "assistant":
                last_question = msg["content"]
                break

        user_prompt = (
            f"Interview level: {ctx.get('interview_level', '')}\n"
            f"Topic: {ctx.get('topic', '')}\n\n"
            f"[Physical constraints]: {json.dumps(ctx.get('global_constants', {}), ensure_ascii=False)}\n\n"
            f"[Dimensions and evaluation rules]:\n{active_rules}\n\n"
            f"[Current scoreboard]:\n"
            f"{json.dumps(slice_state, ensure_ascii=False, indent=2)}\n\n"
            f"[Interviewer's last question]: \"{last_question}\"\n"
            f"[Candidate's answer]: \"{user_input}\"\n\n"
            "=== State machine rules ===\n"
            "- INIT: dimension not yet touched\n"
            "- GATHERING_SIGNALS: candidate started addressing but insufficient evidence\n"
            "- SATISFIED: dimension fully demonstrated\n"
            "- NEEDS_PROBING: gaps or blind spots, needs follow-up\n"
            "- FATAL_FLAW: critical logical error\n\n"
            "=== Output requirements ===\n"
            "- internal_thought: per-dimension analysis of this round\n"
            "- updates: Dict[node_id -> new_status], keys MUST be exact node_ids listed above\n"
            "- new_positive_signals: Dict[node_id -> evidence string], keys MUST be node_ids\n"
            "- new_negative_signals: Dict[node_id -> evidence string], keys MUST be node_ids\n"
            '- probe_suggestions: Dict[node_id -> {"question": "follow-up text", "urgency": 1-5}], keys MUST be node_ids\n'
            "  urgency scale: 5=critical flaw must interrupt immediately, 3=important but not urgent, 1=minor nitpick\n\n"
            "CRITICAL: all dict keys must use the exact node_ids with dots (e.g. \"design.logical_consistency\").\n"
            "Return pure JSON only, no markdown code blocks."
        )

        last_err = None
        for attempt in range(3):
            raw = self._client.chat(system=system_prompt, user=user_prompt)

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

            try:
                patch_data = EvaluatorPatch.model_validate_json(text)
                return Patch(updates={"_evaluator_patch": patch_data.model_dump()})
            except Exception as e:
                last_err = e

        print(f"[Evaluator] JSON parse failed (3 retries): {last_err}", file=sys.stderr)
        return Patch(updates={})
