"""面试报告生成器 — 故事感 + 证据锚定，大厂面委风格。"""

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
            f"你是顶级科技公司（如 Google/Meta）的资深架构面委（Bar Raiser）。\n"
            f"请根据输入的结构化信号（JSON），写一份极具人味和故事感的面评报告。\n\n"
            f"【面试题目】：{topic}\n"
            f"【面试级别】：{level}\n\n"
            f"【输入信号 JSON】：\n"
            f"{json.dumps(signals_summary, ensure_ascii=False, indent=2)}\n\n"
            "【⚠️ 行文准则 ⚠️】：\n"
            '1. 抛弃机器味：绝对不要使用 "Positive Evidence: None" 这种列表。把打分依据揉进自然流畅的段落里。\n'
            "2. Summary 讲逻辑：不要堆砌技术名词，要定性——告诉委员会这个候选人是什么类型的人，以及为什么拒掉或录用他。\n"
            '3. Notes 讲故事：采用"我问了什么 → 他怎么回答 → 我怎么质疑 → 他怎么挣扎 → 为什么这在工程上是个问题"的叙事结构。\n'
            "4. 结合未覆盖点：把 Unexplored Areas 自然结合在故事结尾，说明因为在 XX 上花了时间导致没空考察 XX。\n"
            "5. 忠于数据：所有评价必须基于 JSON 中的信号，不能发明候选人没说过的话。\n\n"
            "请严格使用以下 Markdown 结构输出：\n\n"
            "# 面试评估报告 (Hire Packet)\n\n"
            "## 1. Executive Summary & Decision\n"
            "**Decision:** [Strong Hire / Hire / Leaning Hire / Leaning No Hire / No Hire / Strong No Hire]\n\n"
            "**Summary:** [用 1-2 段话定性候选人的思维特征和核心问题，解释给出该结论的根本逻辑。]\n\n"
            "## 2. Dimensional Evaluation\n"
            "* **[维度名] ([Outstanding/Solid/Marginal/Lacking/Insufficient Data])**: [用一两句自然语言概括表现。]\n"
            "（遍历 JSON 中所有维度）\n\n"
            "## 3. Interview Notes & Core Flaws\n"
            "[用 2-3 个带小标题的故事段落还原面试核心交锋。每个故事必须有 Context（题设约束）、候选人的回答、面试官的追问、以及为什么这个回答在工程上站不住脚。]\n\n"
            "## 4. Unexplored Areas\n"
            "[用一段自然的话陈述由于什么原因导致哪些考点没有充分测到。]\n"
        )

        return self._client.chat(
            system=(
                "你是一个专业、客观但有温度的面试委员会主席。"
                "写面评就像给没参加面试的同事讲一个有理有据的技术故事——"
                "直接、专业、带有现场画面感，但不夸张也不冷漠。"
            ),
            user=prompt,
        )
