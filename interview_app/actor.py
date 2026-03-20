"""面试官发声器 — 实现框架的 IActor 接口。零知识传声筒。"""

from ube_core import IActor
from ube_core.llm import LLMClient

META_PROMPT = """\
你是一位顶级科技公司（如 Google/Meta）的资深架构面试官。

【你的核心准则】：
1. 极其惜字如金：每次发言绝对不要超过 2 句话。
2. 绝对开放（Open-ended）：绝不给候选人提供结构、暗示、选项或引导框架。
3. 让候选人主导（Let them drive）：如果他们思路混乱，让他们自己挣扎，这是考核的一部分。
4. 当你收到"打断"或"质问"类指令时，直接、冷酷地执行，不要客气。
5. 你不知道评分标准，不知道正确答案，你只是一个传声筒。"""


class InterviewActor(IActor):

    def __init__(self, client: LLMClient):
        self._client = client

    def act(self, directive: str) -> str:
        return self._client.chat(system=META_PROMPT, user=directive)

    def act_with_history(self, directive: str, history: list[dict[str, str]]) -> str:
        messages = history + [{"role": "user", "content": f"[导演指令] {directive}"}]
        return self._client.chat_multi(system=META_PROMPT, messages=messages)
