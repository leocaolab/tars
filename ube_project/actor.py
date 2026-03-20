"""前台面试官 Agent：零知识传声筒，只接收 Meta Prompt + 导演指令，不知道题目/Rubric/约束"""

from .llm import LLMClient

# 固定的 Meta Prompt — Actor 永远只看到这段话，与业务完全解耦
META_PROMPT = """\
你是一位顶级科技公司（如 Google/Meta）的资深架构面试官。

【你的核心准则】：
1. 极其惜字如金：每次发言绝对不要超过 2 句话。
2. 绝对开放（Open-ended）：绝不给候选人提供结构、暗示、选项或引导框架。
3. 让候选人主导（Let them drive）：如果他们思路混乱，让他们自己挣扎，这是考核的一部分。
4. 当你收到"打断"或"质问"类指令时，直接、冷酷地执行，不要客气。
5. 你不知道评分标准，不知道正确答案，你只是一个传声筒。"""


class ActorAgent:

    def __init__(self, client: LLMClient):
        self._client = client

    def act(self, directive: str) -> str:
        """接收一条纯文本导演指令，生成面试官发言。不接触黑板。"""
        return self._client.chat(
            system=META_PROMPT,
            user=directive,
        )

    def act_with_history(self, directive: str, history: list[dict[str, str]]) -> str:
        """带对话历史的多轮版本。"""
        # 将导演指令作为最新的 user message 注入
        messages = history + [{"role": "user", "content": f"[导演指令] {directive}"}]
        return self._client.chat_multi(
            system=META_PROMPT,
            messages=messages,
        )
