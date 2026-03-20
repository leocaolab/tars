"""EN evaluator — reuses interview_app.evaluator with lang='en'."""

from typing import List
from interview_app.evaluator import InterviewEvaluator as _Base
from ube_core.llm import LLMClient
from interview_app.models import RubricDimension


class InterviewEvaluator(_Base):
    def __init__(self, client: LLMClient, target_dimensions: List[RubricDimension]):
        super().__init__(client, target_dimensions=target_dimensions, lang="en")
