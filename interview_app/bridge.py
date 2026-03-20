"""业务桥接层 — NodeScorer(评分公式), merge_patch, TerminationChecker。

焦点锁、深度熔断、优先级路由已下沉到 ube_core.router.DirectiveRouter。
业务层只需要实现评分公式和补丁合并。
"""

from ube_core import Blackboard, Patch, NodeScorer, TerminationChecker

# 状态紧急度权重
_STATUS_WEIGHT = {
    "FATAL_FLAW": 4,
    "NEEDS_PROBING": 3,
    "GATHERING_SIGNALS": 2,
    "INIT": 1,
    "SATISFIED": 0,
}

# 阶段划分（按 user 消息轮数）
_PHASES = {"early": (0, 3), "mid": (3, 7), "late": (7, 999), "any": (0, 999)}


def _resolve_node_id(key: str, valid_ids: set[str]) -> str | None:
    if key in valid_ids:
        return key
    key_lower = key.lower().replace("_", ".").replace("-", ".")
    for nid in valid_ids:
        tail = nid.split(".")[-1]
        if tail in key_lower or key_lower in nid:
            return nid
    return None


# ==========================================
# merge_patch — 补丁合并（业务专属）
# ==========================================

def merge_patch(board: Blackboard, patch: Patch) -> None:
    ev_patch = patch.updates.get("_evaluator_patch")
    if not ev_patch:
        return

    valid_ids = set(board.state_tree.keys())

    for key, new_status in ev_patch.get("updates", {}).items():
        nid = _resolve_node_id(key, valid_ids)
        if nid:
            board.state_tree[nid]["status"] = new_status

    for key, signal in ev_patch.get("new_positive_signals", {}).items():
        nid = _resolve_node_id(key, valid_ids)
        if nid:
            signals = signal if isinstance(signal, list) else [signal]
            board.state_tree[nid].setdefault("positive_signals", []).extend(signals)

    for key, signal in ev_patch.get("new_negative_signals", {}).items():
        nid = _resolve_node_id(key, valid_ids)
        if nid:
            signals = signal if isinstance(signal, list) else [signal]
            board.state_tree[nid].setdefault("negative_signals", []).extend(signals)

    for key, suggestion in ev_patch.get("probe_suggestions", {}).items():
        nid = _resolve_node_id(key, valid_ids)
        if nid:
            board.state_tree[nid]["probe_suggestion"] = suggestion


# ==========================================
# InterviewScorer — 多维评分公式（业务专属）
# ==========================================

def _get_turn_number(board: Blackboard) -> int:
    return sum(1 for m in board.history if m.get("role") == "user")


def _get_current_phase(board: Blackboard) -> str:
    turn = _get_turn_number(board)
    for phase, (lo, hi) in _PHASES.items():
        if phase == "any":
            continue
        if lo <= turn < hi:
            return phase
    return "late"


def _get_dim_meta(node_id: str, board: Blackboard) -> dict:
    """从 context.rubric 读取维度元数据（priority, phase, prerequisites）"""
    for dim in board.context.get("rubric", []):
        if dim.get("node_id") == node_id:
            return dim
    return {}


class InterviewScorer(NodeScorer):
    """面试专属评分公式。

    score = (100 - static_priority) + (status_urgency × 20) + phase_match_bonus
    """

    def score(self, node_id: str, node: dict, board: Blackboard) -> float:
        meta = _get_dim_meta(node_id, board)
        s = 0.0

        # 维度 1：静态优先级（priority 越低越重要）
        s += (100 - meta.get("priority", 50))

        # 维度 2：状态紧急度
        status = node.get("status", "INIT")
        s += _STATUS_WEIGHT.get(status, 0) * 20

        # 维度 3：阶段匹配度
        current = _get_current_phase(board)
        node_phase = meta.get("phase", "any")
        if node_phase == "any" or node_phase == current:
            s += 200
        elif (
            (current == "mid" and node_phase == "early")
            or (current == "late" and node_phase == "mid")
        ):
            s += 50

        return s

    def get_prerequisites(self, node_id: str, board: Blackboard) -> list[str]:
        meta = _get_dim_meta(node_id, board)
        return meta.get("prerequisites", [])


# ==========================================
# InterviewTerminationChecker
# ==========================================

class InterviewTerminationChecker(TerminationChecker):

    def should_terminate(self, board: Blackboard) -> bool:
        nodes = list(board.state_tree.values())
        if not nodes:
            return False
        fatal = sum(1 for n in nodes if n.get("status") == "FATAL_FLAW")
        if fatal >= 2:
            return True
        pending = sum(
            1 for n in nodes
            if n.get("status") in ("INIT", "GATHERING_SIGNALS", "NEEDS_PROBING")
        )
        return pending == 0
