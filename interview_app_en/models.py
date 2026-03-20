"""Interview domain models."""

from pydantic import BaseModel, Field
from typing import Dict, List, Literal, Optional, Union

NodeStatus = Literal["INIT", "GATHERING_SIGNALS", "SATISFIED", "NEEDS_PROBING", "FATAL_FLAW"]


class RubricDimension(BaseModel):
    node_id: str
    category: str
    eval_rule: str
    priority: int = 50
    phase: str = "any"
    prerequisites: List[str] = Field(default_factory=list)


class RubricManifest(BaseModel):
    interview_level: str = "Staff/Principal"
    dimensions: List[RubricDimension] = Field(default_factory=list)


class RubricNode(BaseModel):
    status: NodeStatus = "INIT"
    positive_signals: List[str] = Field(default_factory=list)
    negative_signals: List[str] = Field(default_factory=list)
    probe_suggestion: Optional[str] = None


class BlueprintNode(BaseModel):
    id: str
    initial_probe: Optional[str] = None


class ProblemBlueprint(BaseModel):
    problem_id: str
    title: str
    interview_level: str = "Staff/Principal"
    global_constants: Dict[str, str]
    rubric_nodes: List[BlueprintNode]


class ProbeInstruction(BaseModel):
    question: str = Field(description="Follow-up question text")
    urgency: int = Field(default=3, description="Urgency 1-5. 5=critical must interrupt, 1=minor")


class EvaluatorPatch(BaseModel):
    internal_thought: str = ""
    updates: Dict[str, NodeStatus] = Field(default_factory=dict)
    new_positive_signals: Dict[str, Union[str, List[str]]] = Field(default_factory=dict)
    new_negative_signals: Dict[str, Union[str, List[str]]] = Field(default_factory=dict)
    probe_suggestions: Dict[str, Union[str, dict]] = Field(default_factory=dict)
