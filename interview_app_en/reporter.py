"""Interview report generator — high-determinism version with signal anchoring."""

import json
from ube_core import Blackboard
from ube_core.llm import LLMClient


def _get_val(node, key, default):
    if isinstance(node, dict):
        return node.get(key, default)
    return getattr(node, key, default)


class InterviewReporter:

    def __init__(self, client: LLMClient):
        self._client = client

    def generate_report(self, board: Blackboard) -> str:
        signals_summary = {}
        for node_id, node in board.state_tree.items():
            signals_summary[node_id] = {
                "status": _get_val(node, "status", "INIT"),
                "positive_signals": _get_val(node, "positive_signals", []),
                "negative_signals": _get_val(node, "negative_signals", []),
            }

        topic = board.context.get("topic", "")
        level = board.context.get("interview_level", "")

        prompt = (
            f"You are a hiring committee chair (Bar Raiser) at a top tech company.\n"
            f"Generate an objective, rigorous interview assessment based on the structured signals below.\n\n"
            f"[Topic]: {topic}\n"
            f"[Level]: {level}\n\n"
            f"[Signal JSON]:\n"
            f"{json.dumps(signals_summary, ensure_ascii=False, indent=2)}\n\n"
            "[STRICT OUTPUT CONSTRAINTS]:\n"
            "1. FAITHFUL TO DATA: Every claim must be backed by positive_signals or negative_signals from the JSON. Do not invent statements the candidate never made.\n"
            '2. PROFESSIONAL WITH WARMTH: No sarcasm or exaggeration, but also not a cold audit report. Write like a senior engineer giving feedback to a colleague — direct, professional, with occasional personal observations.\n'
            "3. NO OMISSIONS: Cover ALL dimensions in the JSON. If a dimension lacks sufficient signals, mark as 'Insufficient Data'.\n"
            '4. HUMAN VOICE: Write Interview Notes in first person ("I pressed him on...", "he initially struggled but then...") as if briefing the hiring committee in person.\n\n'
            "Use this exact Markdown template:\n\n"
            "# Interview Assessment (Hire Packet)\n\n"
            "## 1. Executive Summary\n"
            "[2-3 sentences based on JSON signals. Core strength and critical flaw. Objective.]\n\n"
            "## 2. Hiring Decision\n"
            "[Bold one: Strong Hire / Hire / Leaning Hire / Leaning No Hire / No Hire / Strong No Hire]\n\n"
            "## 3. Dimensional Evaluation\n"
            "[For EACH dimension in the JSON, use this format:]\n"
            "* **[dimension_id]**: [Outstanding / Solid / Marginal / Lacking / Insufficient Data]\n"
            "  * **Positive Evidence**: [Quote from positive_signals, or 'None']\n"
            "  * **Negative Evidence**: [Quote from negative_signals, or 'None']\n\n"
            "## 4. Key Architectural Flaws\n"
            "[Extract the 2-3 most critical technical errors from negative_signals. Explain why each is wrong from an engineering standpoint.]\n\n"
            "## 5. Unexplored Areas\n"
            "[List dimensions still at INIT or NEEDS_PROBING with insufficient signals. State what should be assessed if time allowed.]\n"
        )

        return self._client.chat(
            system="You are a professional, evidence-based hiring committee chair. Direct and honest, but not cold — like a senior engineer giving feedback to a peer.",
            user=prompt,
        )
