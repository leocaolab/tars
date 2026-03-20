"""Interview report generator — story-driven + evidence-anchored, hiring committee style."""

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
            f"You are a senior architecture hiring committee chair (Bar Raiser) at a top tech company.\n"
            f"Write a vivid, story-driven interview feedback report based on the structured signals below.\n\n"
            f"[Topic]: {topic}\n"
            f"[Level]: {level}\n\n"
            f"[Signal JSON]:\n"
            f"{json.dumps(signals_summary, ensure_ascii=False, indent=2)}\n\n"
            "[WRITING GUIDELINES]:\n"
            '1. NO MACHINE TASTE: Never use "Positive Evidence: None" style lists. Weave evidence into natural, flowing paragraphs.\n'
            "2. SUMMARY = CHARACTERIZATION: Don't stack buzzwords. Tell the committee what TYPE of engineer this person is and WHY you're making this call.\n"
            '3. NOTES = STORIES: Use the structure: "I asked X → they answered Y → I challenged with Z → they struggled/recovered → here\'s why this matters engineering-wise."\n'
            "4. WEAVE IN GAPS: Naturally fold Unexplored Areas into the story ending — explain what didn't get covered and why.\n"
            "5. FAITHFUL TO DATA: All claims must trace back to the JSON signals. Don't invent things the candidate never said.\n\n"
            "Use this exact Markdown structure:\n\n"
            "# Interview Assessment (Hire Packet)\n\n"
            "## 1. Executive Summary & Decision\n"
            "**Decision:** [Strong Hire / Hire / Leaning Hire / Leaning No Hire / No Hire / Strong No Hire]\n\n"
            "**Summary:** [1-2 paragraphs characterizing the candidate's thinking patterns and the core logic behind your decision.]\n\n"
            "## 2. Dimensional Evaluation\n"
            "* **[dimension] ([Outstanding/Solid/Marginal/Lacking/Insufficient Data])**: [1-2 natural sentences.]\n"
            "(Cover ALL dimensions from the JSON.)\n\n"
            "## 3. Interview Notes & Core Flaws\n"
            "[2-3 titled story sections recreating the key moments. Each story needs Context, the candidate's response, your follow-up, and why it matters.]\n\n"
            "## 4. Unexplored Areas\n"
            "[One natural paragraph explaining what wasn't covered and why.]\n"
        )

        return self._client.chat(
            system=(
                "You are a professional, warm hiring committee chair. "
                "Write feedback like telling a well-evidenced technical story to a colleague "
                "who wasn't in the room — direct, professional, with scene-setting, but never theatrical."
            ),
            user=prompt,
        )
