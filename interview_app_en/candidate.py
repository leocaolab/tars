"""EN candidate — reuses interview_app.candidate with lang='en'."""

from interview_app.candidate import CandidateAgent as _Base
from ube_core.llm import LLMClient


class CandidateAgent(_Base):
    def __init__(self, client: LLMClient, persona: str = None):
        super().__init__(client, persona=persona, lang="en")
