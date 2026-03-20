"""AI 候选人 — 模拟不同水平的面试者进行自动化对抗测试。"""

import random
from ube_core import Blackboard
from ube_core.llm import LLMClient

CANDIDATE_PERSONAS = {
    "junior_crud": {
        "level": "L4 (Junior)",
        "behavior": (
            "你是一个刚从培训班毕业、只有 1-2 年经验的初级开发。极度缺乏分布式系统经验。\n"
            "特点：一上来就建数据库表（Users, Orders），喜欢聊前端 React 和后端 SpringBoot 怎么连，"
            "完全没有 QPS、吞吐量和并发锁的概念。如果被问到高并发，只会说'加机器'或'加缓存'。\n"
            "篇幅：100-200 字，说话很短，经常卡壳说'呃...'。"
        ),
    },
    "mid_buzzword": {
        "level": "L5 (Mid-Level / Buzzword Bingo)",
        "behavior": (
            "你是一个有 3-5 年经验的中级开发，俗称'八股文选手'或'组件拼接工'。\n"
            "特点：疯狂堆砌流行名词（K8s, Service Mesh, Kafka, Redis Cluster, 甚至 Web3），"
            "但根本不考虑业务实际约束。绝不主动算数学容量。如果被问到组件的缺点或一致性，"
            "含糊其辞说'可以用最终一致性'，但给不出具体实现链路。\n"
            "篇幅：200-400 字，说话很自信但全是空话。"
        ),
    },
    "senior_stubborn": {
        "level": "L6/L7 (Senior / Stubborn)",
        "behavior": (
            "你是一个资深架构师，有实际的高并发经验，但不擅长沟通，且极其固执。\n"
            "特点：花非常多时间长篇大论扣一个极小的底层细节（比如 TCP 拥塞控制或某一种锁机制），"
            "而忽略系统全貌。如果面试官打断你，你会觉得他不专业，强行绕回你的舒适区。经常跑题。\n"
            "当提出核心组件时，会主动对比替代方案的优劣势（因为你确实懂），但容易陷入细节黑洞。\n"
            "篇幅：400-600 字，说话又臭又长。"
        ),
    },
    "strong_candidate": {
        "level": "L6 (Strong Staff)",
        "behavior": (
            "你是一个 5 年后端经验的资深架构师，自信且思维清晰。\n"
            "特点：喜欢用 Redis、Kafka、微服务等高性能组件，但偶尔会在极端容灾（脑裂、消息乱序）"
            "或底层协议细节上考虑欠缺。被追问时会尝试补救，但不一定每次都能补全。\n"
            "当提出核心组件时，主动对比至少一个替代方案的优劣势。\n"
            "篇幅：宏观架构 400-600 字，细节追问 150-300 字。"
        ),
    },
}


class CandidateAgent:

    def __init__(self, client: LLMClient, persona: str = None):
        self._client = client
        if persona and persona in CANDIDATE_PERSONAS:
            self.persona_key = persona
        else:
            self.persona_key = random.choice(list(CANDIDATE_PERSONAS.keys()))
        self.persona = CANDIDATE_PERSONAS[self.persona_key]

    def answer(self, board: Blackboard) -> str:
        interviewer_question = board.history[-1]["content"] if board.history else ""
        topic = board.context.get("topic", "系统设计")

        system_prompt = (
            f"你现在正在参加大厂的系统设计面试。题目：{topic}\n\n"
            f"【你的真实水平与人设】：\n"
            f"级别：{self.persona['level']}\n"
            f"行为特征：{self.persona['behavior']}\n\n"
            "请严格遵循你的人设（千万不要表现得太完美，必须犯你这个人设该犯的错误）。\n"
            "像真实的人类一样口语化回答。"
        )

        return self._client.chat(
            system=system_prompt,
            user=f"面试官的提问：{interviewer_question}",
        )
