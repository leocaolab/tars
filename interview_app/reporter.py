"""面试报告生成器 — 提取黑板数据，生成大厂标准的 Hire Packet。"""

import json
from ube_core import Blackboard
from ube_core.llm import LLMClient


class InterviewReporter:

    def __init__(self, client: LLMClient):
        self._client = client

    def generate_report(self, board: Blackboard) -> str:
        signals_summary = {}
        for node_id, node in board.state_tree.items():
            signals_summary[node_id] = {
                "状态": node.get("status", "INIT"),
                "正面证据": node.get("positive_signals", []),
                "负面盲点": node.get("negative_signals", []),
            }

        topic = board.context.get("topic", "")
        level = board.context.get("interview_level", "")

        prompt = (
            f"你现在是顶级科技公司（如 Meta/Google）的架构面试委员会主席（Bar Raiser）。\n"
            f"刚才结束了一场针对【{topic}】的 {level} 级别面试。\n\n"
            f"【后台引擎收集到的全量打分数据（JSON）】：\n"
            f"{json.dumps(signals_summary, ensure_ascii=False, indent=2)}\n\n"
            "请根据上述后台数据，输出一份极其专业、干练的面试评估报告。\n"
            "绝对不要在报告里暴露 JSON 格式，必须使用自然的工程师语言。\n\n"
            "报告必须严格遵循以下结构：\n\n"
            "# 面试评估报告 (Hire Packet)\n\n"
            "## 1. Executive Summary\n"
            "用 2-3 句话精炼概括候选人的核心亮点与最致命缺陷，以及最终定调。\n\n"
            "## 2. Hiring Decision\n"
            "只允许从以下选项中选择一个并加粗：\n"
            "[Strong Hire (SH) / Hire (H) / Leaning Hire (LH) / "
            "Leaning No Hire (LNH) / No Hire (NH) / Strong No Hire (SNH)]\n\n"
            "## 3. Dimensional Evaluation\n"
            "将每个维度映射到以下层次之一：\n"
            "- Outstanding (超出预期)\n"
            "- Solid (符合预期)\n"
            "- Marginal (勉强达标)\n"
            "- Lacking (致命缺陷)\n\n"
            "每个维度必须：给出评分层次，并引用后台数据中的具体证据（用人话陈述）。\n\n"
            "## 4. Interview Notes\n"
            "按技术模块还原候选人的核心设计思路，指出哪些是主动想到的，哪些是被追问后才补的，"
            "哪些始终没答上来。\n"
        )

        return self._client.chat(
            system="你是一个冷酷、客观、极其挑剔的大厂面试委员会主席。",
            user=prompt,
        )
