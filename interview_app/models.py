"""面试业务的领域模型 — 所有面试专属概念都在这里。"""

from pydantic import BaseModel, Field
from typing import Dict, List, Literal, Optional

NodeStatus = Literal["INIT", "GATHERING_SIGNALS", "SATISFIED", "NEEDS_PROBING", "FATAL_FLAW"]


class RubricDimension(BaseModel):
    """静态考纲维度"""
    node_id: str
    category: str
    eval_rule: str


class RubricManifest(BaseModel):
    """静态考纲"""
    interview_level: str = "Staff/Principal"
    dimensions: List[RubricDimension] = Field(default_factory=list)


class RubricNode(BaseModel):
    """运行时考点状态"""
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


class EvaluatorPatch(BaseModel):
    """Evaluator 输出 — 面试专属的补丁格式"""
    internal_thought: str = ""
    updates: Dict[str, NodeStatus] = Field(default_factory=dict)
    new_positive_signals: Dict[str, str] = Field(default_factory=dict)
    new_negative_signals: Dict[str, str] = Field(default_factory=dict)
    probe_suggestions: Dict[str, str] = Field(default_factory=dict)
