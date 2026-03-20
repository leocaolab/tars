"""EN actor — reuses interview_app.actor with lang='en'."""

from interview_app.actor import InterviewActor as _Base
from ube_core.llm import LLMClient


class InterviewActor(_Base):
    def __init__(self, client: LLMClient, persona: str = None):
        super().__init__(client, persona=persona, lang="en")
