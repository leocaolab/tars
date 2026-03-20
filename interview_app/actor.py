"""Interview actor — zero-knowledge speaker, loads personas from JSON."""

import random
from ube_core import IActor
from ube_core.llm import LLMClient
from .prompts import load_prompt


class InterviewActor(IActor):

    def __init__(self, client: LLMClient, persona: str = None, lang: str = "zh"):
        self._client = client
        self._lang = lang
        data = load_prompt("actor", lang)
        self._core_rules = data["core_rules"]
        self._meta_template = data["meta_prompt_template"]
        personas = data["personas"]

        if persona and persona in personas:
            self.persona_key = persona
        else:
            self.persona_key = random.choice(list(personas.keys()))
        self.persona = personas[self.persona_key]

    @property
    def meta_prompt(self) -> str:
        return self._meta_template.format(
            persona_name=self.persona["name"],
            persona_style=self.persona["style"],
            core_rules=self._core_rules,
        )

    def act(self, directive: str) -> str:
        return self._client.chat(system=self.meta_prompt, user=f"[导演指令] {directive}")

    def act_with_history(self, directive: str, history: list[dict[str, str]]) -> str:
        recent = history[-4:] if len(history) > 4 else history
        messages = recent + [{"role": "user", "content": f"[导演指令] {directive}"}]
        return self._client.chat_multi(system=self.meta_prompt, messages=messages)
