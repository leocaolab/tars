"""业务桥接层 — 实现框架要求的 merge_patch 和 DirectiveExtractor。

这是框架与业务之间的"翻译器"：
- merge_patch: 把框架的通用 Patch 翻译成面试专属的黑板更新
- InterviewDirectiveExtractor: 从面试黑板中提取导演指令给 Actor
"""

from ube_core import Blackboard, Patch
from ube_core.types import DirectiveExtractor
from .models import RubricNode

_PRIORITY = {
    "FATAL_FLAW": 4,
    "NEEDS_PROBING": 3,
    "GATHERING_SIGNALS": 2,
    "INIT": 1,
    "SATISFIED": 0,
}


def _resolve_node_id(key: str, valid_ids: set[str]) -> str | None:
    """尝试将 LLM 返回的 key 匹配到合法的 node_id。

    LLM 有时返回自创的描述性 key（如 "atomicity_awareness"）而非
    正确的 node_id（如 "design.logical_consistency"）。
    先精确匹配，再尝试子串包含匹配。
    """
    if key in valid_ids:
        return key
    # 子串匹配：如果 key 包含某个 node_id 的尾部片段
    key_lower = key.lower().replace("_", ".").replace("-", ".")
    for nid in valid_ids:
        # "design.logical_consistency" 包含 "logical" 或 "consistency"
        tail = nid.split(".")[-1]
        if tail in key_lower or key_lower in nid:
            return nid
    return None


def merge_patch(board: Blackboard, patch: Patch) -> None:
    """将 Evaluator 的通用 Patch 解包为面试专属的黑板更新。"""
    ev_patch = patch.updates.get("_evaluator_patch")
    if not ev_patch:
        return

    valid_ids = set(board.state_tree.keys())

    # 状态更新
    for key, new_status in ev_patch.get("updates", {}).items():
        nid = _resolve_node_id(key, valid_ids)
        if nid:
            board.state_tree[nid]["status"] = new_status

    # 正面信号
    for key, signal in ev_patch.get("new_positive_signals", {}).items():
        nid = _resolve_node_id(key, valid_ids)
        if nid:
            board.state_tree[nid].setdefault("positive_signals", []).append(signal)

    # 负面信号
    for key, signal in ev_patch.get("new_negative_signals", {}).items():
        nid = _resolve_node_id(key, valid_ids)
        if nid:
            board.state_tree[nid].setdefault("negative_signals", []).append(signal)

    # 追问建议
    for key, suggestion in ev_patch.get("probe_suggestions", {}).items():
        nid = _resolve_node_id(key, valid_ids)
        if nid:
            board.state_tree[nid]["probe_suggestion"] = suggestion


class InterviewDirectiveExtractor(DirectiveExtractor):
    """从面试黑板中提取最紧急的导演指令。"""

    def extract(self, board: Blackboard) -> str:
        # 全部 SATISFIED → 收尾
        all_satisfied = all(
            node.get("status") == "SATISFIED"
            for node in board.state_tree.values()
        )
        if all_satisfied:
            return "所有考核维度已通过。用一句话给候选人正面总结，结束面试。"

        # 按优先级排序，取最紧急的有 probe_suggestion 的节点
        candidates = []
        for node_id, node_data in board.state_tree.items():
            status = node_data.get("status", "INIT")
            probe = node_data.get("probe_suggestion")
            if status != "SATISFIED" and probe:
                candidates.append((node_id, status, probe))

        candidates.sort(key=lambda x: _PRIORITY.get(x[1], 0), reverse=True)

        if candidates:
            return candidates[0][2]

        return "继续倾听。如果候选人停顿或跑偏，用一句话把他拉回正轨。"
