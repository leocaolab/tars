"""面试报告生成器 — 提取黑板数据，生成大厂标准的 Hire Packet。"""

import json
from ube_core import Blackboard
from ube_core.llm import LLMClient


def _get_val(node, key, default):
    """兼容 dict 和 Pydantic 对象的安全取值"""
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
                "状态": _get_val(node, "status", "INIT"),
                "正面证据": _get_val(node, "positive_signals", []),
                "负面盲点": _get_val(node, "negative_signals", []),
            }

        topic = board.context.get("topic", "")
        level = board.context.get("interview_level", "")

        prompt = (
            f"你现在是顶级科技公司（如 Meta/Google）的资深架构面委（Bar Raiser）。\n"
            f"你刚刚亲自面完了一场针对【{topic}】的 {level} 级别面试。\n\n"
            f"【这是你的后台助手帮你收集的全量打分数据（JSON）】：\n"
            f"{json.dumps(signals_summary, ensure_ascii=False, indent=2)}\n\n"
            "请根据这些数据，写一份极其专业但**极具人味（口语化、第一人称）**的面试反馈报告（Hire Packet）。\n\n"
            "【⚠️ 极其重要的行文警告 ⚠️】：\n"
            "1. 绝对不要像机器翻译 JSON！禁止使用"候选人构建了..."、"补齐了..."、"反映出其具备..."这种僵化的 AI 八股文词汇。\n"
            "2. 必须写出【互动画面感】。使用"我一直追问..."、"他一开始没绕明白，后来才反应过来..."、"问到细节时他开始避重就轻"这样的动态交流描述。\n"
            "3. 把专业词汇揉碎在口语里，不要像念说明书一样过度堆砌名词。\n\n"
            "报告必须严格遵循以下结构：\n\n"
            "# 面试评估报告 (Hire Packet)\n\n"
            "## 1. Executive Summary\n"
            "用 2-3 句话极其精炼地概括他最牛的亮点和最要命的硬伤。一针见血，切中要害。\n\n"
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
            "对每个维度的评价，必须结合 JSON 里的 evidence，用人话陈述为什么给他打这个分。\n\n"
            "## 4. Interview Notes\n"
            "用主考官写面评的口吻（第一人称"我"），按技术模块还原咱们聊的过程。重点写：\n"
            "- 他一上来主动抛出了什么好的思路或昏招？\n"
            "- 我往深了挖（比如极限容灾、一致性底线）时，他是怎么挣扎或应对的？\n"
            "- 哪些坑是他自己爬出来的，哪些是我喂到嘴边都没吃进去的？\n"
        )

        return self._client.chat(
            system="你是一个冷酷、客观但说话极具人味、极其讨厌官腔八股文的大厂面试官。",
            user=prompt,
        )
