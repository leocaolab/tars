"""Interview report generator — loads prompt templates from JSON."""

import json
from ube_core import Blackboard
from ube_core.llm import LLMClient
from .prompts import load_prompt


def _get_val(node, key, default):
    if isinstance(node, dict):
        return node.get(key, default)
    return getattr(node, key, default)


class InterviewReporter:

    def __init__(self, client: LLMClient, lang: str = "zh"):
        self._client = client
        data = load_prompt("reporter", lang)
        self._system_prompt = data["system_prompt"]
        self._user_template = data["user_prompt_template"]

    def generate_report(self, board: Blackboard) -> str:
        signals_summary = {}
        for node_id, node in board.state_tree.items():
            signals_summary[node_id] = {
                "status": _get_val(node, "status", "INIT"),
                "positive_signals": _get_val(node, "positive_signals", []),
                "negative_signals": _get_val(node, "negative_signals", []),
            }

        # Build transcript from conversation history
        transcript_lines = []
        for msg in board.history:
            role = msg.get("role", "")
            content = msg.get("content", "")
            if role == "assistant":
                transcript_lines.append(f"Interviewer: {content}")
            elif role == "user":
                transcript_lines.append(f"Candidate: {content}")
        transcript = "\n\n".join(transcript_lines)

        user_prompt = self._user_template.format(
            topic=board.context.get("topic", ""),
            level=board.context.get("interview_level", ""),
            signals_json=json.dumps(signals_summary, ensure_ascii=False, indent=2),
            transcript=transcript,
        )

        return self._client.chat(system=self._system_prompt, user=user_prompt)
