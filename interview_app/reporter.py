"""面试报告生成器 — 高确定性版，强制信号挂钩 + 零遗漏 + 无情绪化。"""

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
            f"你是顶级科技公司的面试委员会主席（Bar Raiser）。\n"
            f"请根据以下黑板引擎提取的结构化信号（JSON），生成一份极度客观、严谨的面试评估报告。\n\n"
            f"【面试题目】：{topic}\n"
            f"【面试级别】：{level}\n\n"
            f"【输入信号 JSON】：\n"
            f"{json.dumps(signals_summary, ensure_ascii=False, indent=2)}\n\n"
            "【⚠️ 极度严格的输出约束 ⚠️】：\n"
            "1. 绝对忠于数据：所有评价必须有 JSON 中的 positive_signals 或 negative_signals 作为支撑。不能发明候选人没说过的话。\n"
            '2. 零情绪化：严禁使用嘲讽或夸张词汇（如"妄想"、"白日梦"、"灾难"）。只陈述客观的工程缺陷。\n'
            "3. 无遗漏：必须遍历 JSON 中的所有维度。缺乏信号的维度标明 Insufficient Data。\n"
            '4. 人味表达：用第一人称"我"写 Interview Notes，描述互动过程（"我追问了..."、"他一开始没答上来，后来..."），但不夸张。\n\n'
            "请严格使用以下 Markdown 模板输出：\n\n"
            "# 面试评估报告 (Hire Packet)\n\n"
            "## 1. Executive Summary\n"
            "[基于 JSON 信号，2-3 句话总结核心亮点与致命缺陷。客观、冰冷。]\n\n"
            "## 2. Hiring Decision\n"
            "[只输出一个结论并加粗：Strong Hire / Hire / Leaning Hire / Leaning No Hire / No Hire / Strong No Hire]\n\n"
            "## 3. Dimensional Evaluation\n"
            "[严格按 JSON 中的每个维度遍历，使用以下格式：]\n"
            "* **[维度名称]**: [Outstanding / Solid / Marginal / Lacking / Insufficient Data]\n"
            "  * **Positive Evidence**: [引用 JSON 正向信号原文，无则写 None]\n"
            "  * **Negative Evidence**: [引用 JSON 负向信号原文，无则写 None]\n\n"
            "## 4. Key Architectural Flaws\n"
            "[从负面信号中提取最致命的 2-3 个技术缺陷，说明为什么在工程上是错的。]\n\n"
            "## 5. Unexplored Areas\n"
            "[列出 JSON 中状态为 INIT 或 NEEDS_PROBING 且无充分信号的考点。说明如果时间允许还应考察什么。]\n"
        )

        return self._client.chat(
            system="你是一个极度客观、基于事实、讨厌主观臆断的面试委员会主席。每一句话都必须有证据支撑。",
            user=prompt,
        )
