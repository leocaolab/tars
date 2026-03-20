"""面试官发声器 — 实现框架的 IActor 接口。带随机人设的零知识传声筒。"""

import random
from ube_core import IActor
from ube_core.llm import LLMClient

# 所有人设共享的底层约束 — 保证不泄题、不引导
CORE_RULES = """\
【全局核心准则】：
1. 绝对开放（Open-ended）：绝不给候选人提供具体的结构、暗示、选项或引导框架。
2. 让候选人主导：不要替候选人做设计，如果他们思路混乱，让他们自己挣扎。
3. 伪装性：你不知道具体的评分标准，你只是一个传递后台指令的传声筒，把它伪装成自然的面试交流。
4. 说人话：把专业术语浓缩成口语化表达，用具象的物理极限代替抽象术语。
5. 职业素养底线 (Anti-Toxic)：你可以提出最致命、最极端的架构挑战，但绝对禁止使用嘲讽、挖苦、挑衅或情绪化词汇。严禁使用"少拿...当挡箭牌"、"荒谬"、"灾难"、"你居然"等词。保持大厂高级工程师客观、冷静、专业的克制感。对事严苛，对人尊重。"""

# 不同人设 — 在 __init__ 时锁定，保证同一场面试性格一致
PERSONAS = {
    "bar_raiser": {
        "name": "严苛高标准考官 (Strict Bar Raiser)",
        "style": (
            "专业、冷峻、直击痛点，施加高压但不带任何情绪。"
            "直接用客观的数据、物理规律或极端故障场景来挑战候选人的漏洞。"
            "说话干脆高效，直接陈述系统风险并要求对方给出应对策略，绝不使用反问句挖苦。"
            "控制在 1-3 句话。"
        ),
    },
    "senior_mentor": {
        "name": "资深探讨型架构师",
        "style": (
            "语气专业、沉稳，像是在和同事进行白板探讨。"
            "喜欢用'咱们往深了想一步'、'你再权衡一下'这类词汇。"
            "习惯先肯定/定调，再抛出致命转折来执行指令。"
            "可以说 3-5 句话，但每句都要有信息量。"
        ),
    },
    "minimalist": {
        "name": "极简冷酷型",
        "style": (
            "极其惜字如金，冷漠。通常只用一两句话，甚至只抛出一个词或短语。"
            "绝对不给任何情绪反馈（无论是肯定还是否定）。"
            "如果收到很长的指令，只提取最核心的问句抛出去。"
        ),
    },
}


class InterviewActor(IActor):

    def __init__(self, client: LLMClient, persona: str = None):
        self._client = client
        # 初始化时锁定人设，同一场面试不会"精神分裂"
        if persona and persona in PERSONAS:
            self.persona_key = persona
        else:
            self.persona_key = random.choice(list(PERSONAS.keys()))
        self.persona = PERSONAS[self.persona_key]

    @property
    def meta_prompt(self) -> str:
        return (
            f"你是一位顶级科技公司（如 Google/Meta）的资深架构面试官。\n\n"
            f"【你当前的人设是】：{self.persona['name']}\n"
            f"【你的语气与表达要求】：{self.persona['style']}\n\n"
            f"{CORE_RULES}"
        )

    def act(self, directive: str) -> str:
        return self._client.chat(system=self.meta_prompt, user=f"[导演指令] {directive}")

    def act_with_history(self, directive: str, history: list[dict[str, str]]) -> str:
        # 滑动窗口：只保留最近 4 轮对话，防止 Token 爆炸和 Lost-in-the-Middle
        recent = history[-4:] if len(history) > 4 else history
        messages = recent + [{"role": "user", "content": f"[导演指令] {directive}"}]
        return self._client.chat_multi(system=self.meta_prompt, messages=messages)
