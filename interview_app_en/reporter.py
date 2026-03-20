"""Interview report generator — produces a Hire Packet from blackboard data."""

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
                "positive_evidence": _get_val(node, "positive_signals", []),
                "negative_gaps": _get_val(node, "negative_signals", []),
            }

        topic = board.context.get("topic", "")
        level = board.context.get("interview_level", "")

        prompt = (
            f"You are a senior architecture hiring committee chair (Bar Raiser) at a top tech company.\n"
            f"You just personally conducted a {level}-level interview on [{topic}].\n\n"
            f"[Scoring data collected by your backend assistant (JSON)]:\n"
            f"{json.dumps(signals_summary, ensure_ascii=False, indent=2)}\n\n"
            "Write a professional but **deeply human** interview feedback report (Hire Packet).\n\n"
            "[CRITICAL WRITING RULES]:\n"
            '1. DO NOT machine-translate the JSON! Ban stiff phrases like "the candidate demonstrated..." or "reflects capability...".\n'
            '2. Write with INTERACTIVE SCENE descriptions. Use "I kept pressing him on...", '
            '"he stumbled at first, then recovered...", "when I drilled into the details, he started deflecting".\n'
            "3. Weave technical terms naturally into conversational language.\n\n"
            "Report structure:\n\n"
            "# Interview Assessment (Hire Packet)\n\n"
            "## 1. Executive Summary\n"
            "2-3 razor-sharp sentences: strongest highlight and most critical flaw.\n\n"
            "## 2. Hiring Decision\n"
            "Pick exactly one (bold it):\n"
            "[Strong Hire (SH) / Hire (H) / Leaning Hire (LH) / "
            "Leaning No Hire (LNH) / No Hire (NH) / Strong No Hire (SNH)]\n\n"
            "## 3. Dimensional Evaluation\n"
            "Map each dimension to one of:\n"
            "- Outstanding / Solid / Marginal / Lacking\n"
            "Cite specific evidence from the JSON in plain language.\n\n"
            "## 4. Interview Notes\n"
            'Write in first person ("I"). By technical module, describe:\n'
            "- What did they proactively bring up vs. what was only drawn out under pressure?\n"
            "- How did they handle being cornered on consistency, failure modes, capacity?\n"
            "- What gaps did they never close, even after being guided?\n"
        )

        return self._client.chat(
            system="You are a cold, objective, but deeply human hiring committee chair who despises corporate jargon.",
            user=prompt,
        )
