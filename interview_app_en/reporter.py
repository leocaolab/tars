"""EN reporter — reuses interview_app.reporter with lang='en'."""

from interview_app.reporter import InterviewReporter as _Base
from ube_core.llm import LLMClient


class InterviewReporter(_Base):
    def __init__(self, client: LLMClient):
        super().__init__(client, lang="en")
