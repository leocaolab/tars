"""AI candidate — loads personas from JSON."""

import random
from ube_core import Blackboard
from ube_core.llm import LLMClient
from .prompts import load_prompt


class CandidateAgent:

    def __init__(self, client: LLMClient, persona: str = None, lang: str = "zh"):
        self._client = client
        data = load_prompt("candidate", lang)
        self._system_template = data["system_prompt_template"]
        personas = data["personas"]

        if persona and persona in personas:
            self.persona_key = persona
        else:
            self.persona_key = random.choice(list(personas.keys()))
        self.persona = personas[self.persona_key]

    def answer(self, board: Blackboard) -> str:
        interviewer_question = board.history[-1]["content"] if board.history else ""
        topic = board.context.get("topic", "系统设计")

        system_prompt = self._system_template.format(
            topic=topic,
            level=self.persona["level"],
            behavior=self.persona["behavior"],
        )

        return self._client.chat(
            system=system_prompt,
            user=f"面试官的提问：{interviewer_question}",
        )
