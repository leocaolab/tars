"""会话工厂：支持三层 Rubric 加载（Universal + Domain + Inline Custom）。"""

import json
import uuid
from pathlib import Path
from typing import Dict, Any

from ube_core import Blackboard
from .models import ProblemBlueprint, RubricDimension, RubricManifest, RubricNode


# ==========================================
# Legacy loaders (backward compatible)
# ==========================================

def load_rubric(path: str = None) -> RubricManifest:
    if path is None:
        path = str(Path(__file__).parent / "rubric.json")
    with open(path, "r", encoding="utf-8") as f:
        return RubricManifest.model_validate(json.load(f))


def load_blueprint(path: str) -> ProblemBlueprint:
    with open(path, "r", encoding="utf-8") as f:
        return ProblemBlueprint.model_validate(json.load(f))


# ==========================================
# V2: Three-layer Rubric Factory
# ==========================================

def _load_rubric_layer(path: str) -> dict[str, dict]:
    """Load a rubric layer file, return {node_id: dimension_dict}."""
    with open(path, "r", encoding="utf-8") as f:
        data = json.load(f)
    index = {}
    for item in data.get("rubrics", []):
        index[item["node_id"]] = item
    return index


class SessionFactoryV2:
    """三层 Rubric 会话工厂。

    Layer 1 (Universal): 跨领域通用素质
    Layer 2 (Domain):    领域专属考点（后端/前端/算法）
    Layer 3 (Inline):    蓝图内联的题目专属考点

    蓝图通过 rubric_layers 字段声明需要加载的 Layer 1 + Layer 2 文件路径，
    再通过 rubric_nodes 订阅具体考点（引用 or 内联）。
    """

    def __init__(self, base_dir: str = None):
        self._base_dir = Path(base_dir) if base_dir else Path(__file__).parent

    def create(self, blueprint_path: str) -> Blackboard:
        with open(blueprint_path, "r", encoding="utf-8") as f:
            bp_raw = json.load(f)

        # 1. 加载 Rubric 字典库（Universal + Domain 层）
        rubric_dict: dict[str, dict] = {}
        for layer_path in bp_raw.get("rubric_layers", []):
            full_path = str(self._base_dir / layer_path)
            rubric_dict.update(_load_rubric_layer(full_path))

        # 2. 遍历蓝图中的 rubric_nodes，组装 state_tree 和 rubric 元数据
        state_tree: dict[str, Any] = {}
        rubric_dims: list[dict] = []

        for node_req in bp_raw.get("rubric_nodes", []):
            node_id = node_req["id"]

            # Layer 3: 内联自定义节点（蓝图自带 eval_rule）
            if "eval_rule" in node_req:
                dim_meta = {
                    "node_id": node_id,
                    "category": node_req.get("category", "Problem Specific"),
                    "priority": node_req.get("priority", 50),
                    "phase": node_req.get("phase", "any"),
                    "prerequisites": node_req.get("prerequisites", []),
                    "eval_rule": node_req["eval_rule"],
                }
            # Layer 1+2: 从字典库查询
            elif node_id in rubric_dict:
                dim_meta = rubric_dict[node_id].copy()
            else:
                print(f"[SessionFactory] WARNING: node '{node_id}' not found in rubric layers, skipped.")
                continue

            rubric_dims.append(dim_meta)

            # 初始化运行时状态
            node = RubricNode(probe_suggestion=node_req.get("initial_probe"))
            state_tree[node_id] = node.model_dump()

        # 3. 组装 context
        context = {
            "topic": bp_raw["title"],
            "interview_level": bp_raw.get("interview_level", "Unknown"),
            "global_constants": bp_raw.get("global_constants", {}),
            "rubric": rubric_dims,
        }

        system_prompt = (
            f"你是一位极其严苛的 {bp_raw.get('interview_level', '')} 级架构面试官。"
            f"今天你要考察候选人设计：{bp_raw['title']}。"
        )
        history = [{"role": "system", "content": system_prompt}]

        session_id = f"intv_{uuid.uuid4().hex[:8]}"
        return Blackboard(
            session_id=session_id,
            context=context,
            state_tree=state_tree,
            history=history,
        )


# ==========================================
# Legacy SessionFactory (backward compatible)
# ==========================================

class SessionFactory:

    def __init__(self, rubric: RubricManifest):
        self.rubric = rubric
        self._dim_index: Dict[str, RubricDimension] = {
            d.node_id: d for d in rubric.dimensions
        }

    def create(self, blueprint: ProblemBlueprint) -> Blackboard:
        referenced_dims = []
        for bp_node in blueprint.rubric_nodes:
            dim = self._dim_index.get(bp_node.id)
            if dim is None:
                raise ValueError(
                    f"Blueprint references unknown dimension: {bp_node.id}\n"
                    f"Available: {list(self._dim_index.keys())}"
                )
            referenced_dims.append(dim)

        state_tree = {}
        for bp_node in blueprint.rubric_nodes:
            node = RubricNode(probe_suggestion=bp_node.initial_probe)
            state_tree[bp_node.id] = node.model_dump()

        context = {
            "topic": blueprint.title,
            "interview_level": blueprint.interview_level,
            "global_constants": blueprint.global_constants,
            "rubric": [d.model_dump() for d in referenced_dims],
        }

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
