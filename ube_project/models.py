from pydantic import BaseModel, Field
from typing import List, Dict, Literal, Optional

# 运行时状态机的五个绝对状态
NodeStatus = Literal["INIT", "GATHERING_SIGNALS", "SATISFIED", "NEEDS_PROBING", "FATAL_FLAW"]


class RubricDimension(BaseModel):
    """静态考纲维度：不含任何具体技术名词，只有评估目标和信号捕获规则"""
    node_id: str
    category: str = Field(description="能力类别，如 Engineering Rigor")
    eval_rule: str = Field(description="给考官看的评判规则，纯能力导向")


class RubricManifest(BaseModel):
    """静态配置：Rubric Manifest（能力维度打分卡）"""
    interview_level: str = "Staff/Principal"
    dimensions: List[RubricDimension] = Field(default_factory=list)


class RubricNode(BaseModel):
    """运行时考点状态：五态状态机 + 正负信号分离 + 追问建议"""
    status: NodeStatus = "INIT"
    positive_signals: List[str] = Field(default_factory=list, description="正面证据")
    negative_signals: List[str] = Field(default_factory=list, description="负面证据/盲点")
    probe_suggestion: Optional[str] = Field(default=None, description="考官给前台的追问建议")


class Blackboard(BaseModel):
    """全局黑板：Single Source of Truth"""
    session_id: str
    topic: str
    interview_level: str = "Staff/Principal"
    global_constants: Dict[str, str] = Field(description="不可篡改的物理约束")
    rubric: List[RubricDimension] = Field(description="静态考纲维度定义")
    state_tree: Dict[str, RubricNode] = Field(description="运行时累积记分牌")
    history: List[Dict[str, str]] = Field(default_factory=list, description="对话记录")


class BlueprintNode(BaseModel):
    """蓝图中的考点预设：引用 rubric 维度 ID + 可选的初始引导词"""
    id: str
    initial_probe: Optional[str] = Field(default=None, description="Time=0 时注入的首轮追问建议")


class ProblemBlueprint(BaseModel):
    """静态题目蓝图：由资深面试官预先定义，存在数据库中"""
    problem_id: str
    title: str
    interview_level: str = "Staff/Principal"
    global_constants: Dict[str, str] = Field(description="不可篡改的物理约束")
    rubric_nodes: List[BlueprintNode] = Field(description="引用的考纲维度 ID 列表 + 初始探针")


class EvaluatorPatch(BaseModel):
    """考官输出的增量补丁 — LLM 强制返回此结构"""
    internal_thought: str = Field(description="推理过程 (CoT)")
    updates: Dict[str, NodeStatus] = Field(
        default_factory=dict, description="对考点的状态覆写，Key 为 node_id"
    )
    new_positive_signals: Dict[str, str] = Field(
        default_factory=dict, description="为对应考点追加的正面证据，Key 为 node_id"
    )
    new_negative_signals: Dict[str, str] = Field(
        default_factory=dict, description="为对应考点追加的负面证据/盲点，Key 为 node_id"
    )
    probe_suggestions: Dict[str, str] = Field(
        default_factory=dict, description="为 NEEDS_PROBING 的考点提供追问建议，Key 为 node_id"
    )
