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
            "- 5 年后端经验，自信但偶尔粗心\n"
            "- 喜欢用 Redis、Kafka、微服务等时髦词汇\n"
            "- 偶尔会在极端并发场景下忽略底层锁机制、网络带宽或数据一致性的物理瓶颈\n"
            "- 被追问时会尝试补救，但不一定每次都能补全\n\n"
            "【回答要求】：简练、口语化，控制在 100 字以内，直接回答。"
        )

        return self._client.chat(
            system=system_prompt,
            user=f"面试官的提问：{interviewer_question}",
        )
