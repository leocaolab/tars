"""AI 候选人 — 用于 Self-Play 对抗。"""

from ube_core import Blackboard
from ube_core.llm import LLMClient


class CandidateAgent:

    def __init__(self, client: LLMClient):
        self._client = client

    def answer(self, board: Blackboard) -> str:
        interviewer_question = board.history[-1]["content"] if board.history else ""
        topic = board.context.get("topic", "系统设计")
        level = board.context.get("interview_level", "Staff")

        system_prompt = (
            f"你现在是一个正在参加大厂 {level} 级别系统设计面试的候选人。\n"
            f"面试题目是：{topic}\n\n"
            "【你的人设】：\n"
            "- 5 年后端经验，自信，喜欢用 Redis、Kafka、微服务等高性能组件\n"
            "- 偶尔会在极端容灾（脑裂、消息乱序）或底层协议细节上考虑欠缺\n"
            "- 被追问时会尝试补救，但不一定每次都能补全\n\n"
            "【表达策略与篇幅控制】：\n"
            "- 拟真口语化：你在白板前边画图边讲，多用'我会'、'考虑到'、'咱们来看看'等口语\n"
            "- 动态篇幅：\n"
            "  · 面试官要求总体架构/宏观分析 → 详细展开技术拓扑、数据流和组件选型，给出权衡(Trade-offs)，400-600 字\n"
            "  · 面试官具体追问/质问某个细节 → 总-分结构直接回答核心逻辑，150-300 字\n"
            "- 当提出核心组件时，主动对比至少一个替代方案的优劣势"
        )

        return self._client.chat(
            system=system_prompt,
            user=f"面试官的提问：{interviewer_question}",
        )
