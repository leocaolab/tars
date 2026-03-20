import json
import uuid
from pathlib import Path
from typing import Dict

from .models import (
    Blackboard,
    ProblemBlueprint,
    RubricDimension,
    RubricManifest,
    RubricNode,
)


def load_rubric(path: str = None) -> RubricManifest:
    """从 JSON 文件加载静态考纲"""
    if path is None:
        path = str(Path(__file__).parent / "rubric.json")
    with open(path, "r", encoding="utf-8") as f:
        return RubricManifest.model_validate(json.load(f))


def load_blueprint(path: str) -> ProblemBlueprint:
    """从 JSON 文件加载题目蓝图"""
    with open(path, "r", encoding="utf-8") as f:
        return ProblemBlueprint.model_validate(json.load(f))


class SessionFactory:
    """会话工厂：从静态蓝图 + 考纲 → 生成动态初始黑板"""

    def __init__(self, rubric: RubricManifest):
        self.rubric = rubric
        # 建立 node_id → RubricDimension 的索引
        self._dim_index: Dict[str, RubricDimension] = {
            d.node_id: d for d in rubric.dimensions
        }

    def create(self, blueprint: ProblemBlueprint) -> Blackboard:
        """
        创世函数：拉取静态蓝图，交叉引用考纲，生成动态初始黑板。
        蓝图的 rubric_nodes 必须是考纲 dimensions 的子集。
        """
        # 1. 交叉验证：蓝图引用的每个 node_id 必须在考纲中存在
        referenced_dims = []
        for bp_node in blueprint.rubric_nodes:
            dim = self._dim_index.get(bp_node.id)
            if dim is None:
                raise ValueError(
                    f"蓝图引用了不存在的考纲维度: {bp_node.id}\n"
                    f"可用维度: {list(self._dim_index.keys())}"
                )
            referenced_dims.append(dim)

        # 2. 组装运行时状态树（全部 INIT，注入蓝图预设的 initial_probe）
        state_tree: Dict[str, RubricNode] = {}
        for bp_node in blueprint.rubric_nodes:
            state_tree[bp_node.id] = RubricNode(
                probe_suggestion=bp_node.initial_probe,
            )

        # 3. 组装初始对话历史（系统发令枪）
        system_prompt = (
            f"你是一位极其严苛的 {blueprint.interview_level} 级架构面试官。"
            f"今天你要考察候选人设计：{blueprint.title}。"
            f"请查看 state_tree，寻找状态为 INIT 且带有 probe_suggestion 的节点，"
            f"用一句极其简练、专业的话开场，不带任何废话，直接把舞台交给候选人。"
        )
        history = [{"role": "system", "content": system_prompt}]

        # 4. 实例化强类型 Pydantic 黑板
        session_id = f"intv_{uuid.uuid4().hex[:8]}"
        return Blackboard(
            session_id=session_id,
            topic=blueprint.title,
            interview_level=blueprint.interview_level,
            global_constants=blueprint.global_constants,
            rubric=referenced_dims,
            state_tree=state_tree,
            history=history,
        )
