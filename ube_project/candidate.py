"""AI 候选人 Agent：故意带破绽的系统设计候选人，用于 Self-Play 对抗"""

from .llm import LLMClient
from .models import Blackboard


class CandidateAgent:

    def __init__(self, client: LLMClient):
        self._client = client

    def answer(self, board: Blackboard) -> str:
        # 拿到面试官刚刚提的问题
        interviewer_question = board.history[-1]["content"] if board.history else ""

        system_prompt = (
            f"你现在是一个正在参加大厂 {board.interview_level} 级别系统设计面试的候选人。\n"
            f"面试题目是：{board.topic}\n\n"
            "【你的人设】：\n"
            "- 5 年后端经验，自信但偶尔粗心\n"
            "- 喜欢用 Redis、Kafka、微服务等时髦词汇\n"
            "- 偶尔会在极端并发场景下忽略底层锁机制、网络带宽或数据一致性的物理瓶颈\n"
            "- 被追问时会尝试补救，但不一定每次都能补全\n\n"
            "【回答要求】：\n"
            "- 简练、口语化，像真实程序员在白板前说话\n"
            "- 控制在 100 字以内\n"
            "- 直接回答，不要说「好的」「这个问题很好」之类的废话"
        )

        return self._client.chat(
            system=system_prompt,
            user=f"面试官的提问：{interviewer_question}",
        )
