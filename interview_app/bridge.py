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


def merge_patch(board: Blackboard, patch: Patch) -> None:
    """将 Evaluator 的通用 Patch 解包为面试专属的黑板更新。"""
    ev_patch = patch.updates.get("_evaluator_patch")
    if not ev_patch:
        return

    # 状态更新
    for node_id, new_status in ev_patch.get("updates", {}).items():
        if node_id in board.state_tree:
            board.state_tree[node_id]["status"] = new_status

    # 正面信号
    for node_id, signal in ev_patch.get("new_positive_signals", {}).items():
        if node_id in board.state_tree:
            board.state_tree[node_id].setdefault("positive_signals", []).append(signal)

    # 负面信号
    for node_id, signal in ev_patch.get("new_negative_signals", {}).items():
        if node_id in board.state_tree:
            board.state_tree[node_id].setdefault("negative_signals", []).append(signal)

    # 追问建议
    for node_id, suggestion in ev_patch.get("probe_suggestions", {}).items():
        if node_id in board.state_tree:
            board.state_tree[node_id]["probe_suggestion"] = suggestion


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
