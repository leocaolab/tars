"""会话工厂：从蓝图 + 考纲 → 生成初始黑板（使用框架的通用 Blackboard）。"""

import json
import uuid
from pathlib import Path
from typing import Dict

from ube_core import Blackboard
from .models import ProblemBlueprint, RubricDimension, RubricManifest, RubricNode


def load_rubric(path: str = None) -> RubricManifest:
    if path is None:
        path = str(Path(__file__).parent / "rubric.json")
    with open(path, "r", encoding="utf-8") as f:
        return RubricManifest.model_validate(json.load(f))


def load_blueprint(path: str) -> ProblemBlueprint:
    with open(path, "r", encoding="utf-8") as f:
        return ProblemBlueprint.model_validate(json.load(f))


class SessionFactory:

    def __init__(self, rubric: RubricManifest):
        self.rubric = rubric
        self._dim_index: Dict[str, RubricDimension] = {
            d.node_id: d for d in rubric.dimensions
        }

    def create(self, blueprint: ProblemBlueprint) -> Blackboard:
        # 交叉验证
        referenced_dims = []
        for bp_node in blueprint.rubric_nodes:
            dim = self._dim_index.get(bp_node.id)
            if dim is None:
                raise ValueError(
                    f"蓝图引用了不存在的考纲维度: {bp_node.id}\n"
                    f"可用维度: {list(self._dim_index.keys())}"
                )
            referenced_dims.append(dim)

        # 组装 state_tree — 用 RubricNode 序列化为 dict 塞进泛型 state_tree
        state_tree = {}
        for bp_node in blueprint.rubric_nodes:
            node = RubricNode(probe_suggestion=bp_node.initial_probe)
            state_tree[bp_node.id] = node.model_dump()

        # 业务属性全部塞进 context
        context = {
            "topic": blueprint.title,
            "interview_level": blueprint.interview_level,
            "global_constants": blueprint.global_constants,
            "rubric": [d.model_dump() for d in referenced_dims],
        }

        # 系统 prompt
        system_prompt = (
            f"你是一位极其严苛的 {blueprint.interview_level} 级架构面试官。"
            f"今天你要考察候选人设计：{blueprint.title}。"
        )
        history = [{"role": "system", "content": system_prompt}]

        session_id = f"intv_{uuid.uuid4().hex[:8]}"
        return Blackboard(
            session_id=session_id,
            context=context,
            state_tree=state_tree,
            history=history,
        )
