"""业务桥接层 — merge_patch, DirectiveExtractor (考点锁+深度熔断+多维评分), TerminationChecker。"""

from ube_core import Blackboard, Patch
from ube_core.types import DirectiveExtractor, TerminationChecker

# 状态紧急度
_STATUS_PRIORITY = {
    "FATAL_FLAW": 4,
    "NEEDS_PROBING": 3,
    "GATHERING_SIGNALS": 2,
    "INIT": 1,
    "SATISFIED": 0,
}

# 深度熔断阈值
MAX_DEPTH_TURNS = 2

# 终态
_TERMINAL = {"SATISFIED", "FATAL_FLAW"}

# 阶段划分（按轮数）
_PHASE_THRESHOLDS = {"early": (0, 3), "mid": (3, 7), "late": (7, 999), "any": (0, 999)}


def _resolve_node_id(key: str, valid_ids: set[str]) -> str | None:
    if key in valid_ids:
        return key
    key_lower = key.lower().replace("_", ".").replace("-", ".")
    for nid in valid_ids:
        tail = nid.split(".")[-1]
        if tail in key_lower or key_lower in nid:
            return nid
    return None


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
# 考点锁 — 存在 board.context 中
# ==========================================

def _get_focus(board: Blackboard) -> tuple[str | None, int]:
    return (
        board.context.get("_focused_node_id"),
        board.context.get("_focused_turn_count", 0),
    )


def _set_focus(board: Blackboard, node_id: str | None, turns: int = 0) -> None:
    board.context["_focused_node_id"] = node_id
    board.context["_focused_turn_count"] = turns


def _get_turn_number(board: Blackboard) -> int:
    """当前轮数 = 对话中 user 消息的数量"""
    return sum(1 for m in board.history if m.get("role") == "user")


def _get_current_phase(board: Blackboard) -> str:
    turn = _get_turn_number(board)
    for phase, (lo, hi) in _PHASE_THRESHOLDS.items():
        if phase == "any":
            continue
        if lo <= turn < hi:
            return phase
    return "late"


def _build_rubric_index(board: Blackboard) -> dict:
    """从 context.rubric 构建 node_id → {priority, phase, prerequisites} 索引"""
    index = {}
    for dim in board.context.get("rubric", []):
        nid = dim.get("node_id", "")
        index[nid] = {
            "priority": dim.get("priority", 50),
            "phase": dim.get("phase", "any"),
            "prerequisites": dim.get("prerequisites", []),
        }
    return index


def _prerequisites_met(node_id: str, rubric_idx: dict, state_tree: dict) -> bool:
    """检查前置考点是否已离开 INIT（至少被触及过）"""
    prereqs = rubric_idx.get(node_id, {}).get("prerequisites", [])
    for prereq_id in prereqs:
        prereq = state_tree.get(prereq_id)
        if not prereq:
            continue
        status = prereq.get("status", "INIT")
        if status == "INIT":
            return False  # 前置考点完全未触及，锁死当前考点
    return True


def _calculate_score(
    node_id: str,
    node: dict,
    rubric_idx: dict,
    current_phase: str,
) -> float:
    """多维动态权重计算。分数越高 = 越应该问。"""
    meta = rubric_idx.get(node_id, {"priority": 50, "phase": "any"})
    score = 0.0

    # 维度 1：静态优先级（priority 越低越重要，反转为分数）
    score += (100 - meta["priority"])

    # 维度 2：状态紧急度
    status = node.get("status", "INIT")
    score += _STATUS_PRIORITY.get(status, 0) * 20

    # 维度 3：阶段匹配度
    node_phase = meta["phase"]
    if node_phase == "any" or node_phase == current_phase:
        score += 200
    elif (  # 相邻阶段也给部分加分
        (current_phase == "mid" and node_phase == "early")
        or (current_phase == "late" and node_phase == "mid")
    ):
        score += 50

    return score


def _pick_next_node(board: Blackboard) -> str | None:
    """多维评分选择下一个追问考点。"""
    rubric_idx = _build_rubric_index(board)
    current_phase = _get_current_phase(board)

    candidates = []
    for node_id, node in board.state_tree.items():
        status = node.get("status", "INIT")
        if status in _TERMINAL:
            continue
        if not node.get("probe_suggestion"):
            continue
        # 依赖图谱：前置未触及则锁死
        if not _prerequisites_met(node_id, rubric_idx, board.state_tree):
            continue

        score = _calculate_score(node_id, node, rubric_idx, current_phase)
        candidates.append((score, node_id))

    candidates.sort(reverse=True)
    return candidates[0][1] if candidates else None


class InterviewDirectiveExtractor(DirectiveExtractor):
    """多维动态权重指令提取器。

    机制：
    1. 考点锁 (Topic Lock): 锁定一个考点连续追问
    2. 深度熔断 (Depth Timeout): 同一考点最多 MAX_DEPTH_TURNS 轮
    3. 多维评分: 静态优先级 + 状态紧急度 + 阶段匹配度
    4. 依赖图谱: 前置考点未触及则锁死后续考点
    """

    def extract(self, board: Blackboard) -> str:
        # 全部终态 → 收尾
        if all(n.get("status") in _TERMINAL for n in board.state_tree.values()):
            _set_focus(board, None)
            return "所有考核维度已评估完毕。用一句话给候选人总结，结束面试。"

        focused_id, turn_count = _get_focus(board)

        # === 锁定中 ===
        if focused_id and focused_id in board.state_tree:
            node = board.state_tree[focused_id]
            status = node.get("status", "INIT")

            # 自然解锁（终态）
            if status in _TERMINAL:
                _set_focus(board, None)

            # 深度熔断
            elif turn_count >= MAX_DEPTH_TURNS:
                _set_focus(board, None)
                return (
                    "在当前话题上的深挖已经足够了。"
                    "请用一句话优雅地收束（如'好的，这块我大概了解了'），"
                    "然后立即转移到系统设计的下一个关键问题。"
                )

            # 继续深挖
            else:
                probe = node.get("probe_suggestion")
                if probe:
                    node["probe_suggestion"] = None
                    _set_focus(board, focused_id, turn_count + 1)
                    return probe
                else:
                    _set_focus(board, None)

        # === 选择下一个考点 ===
        next_id = _pick_next_node(board)
        if next_id:
            probe = board.state_tree[next_id].get("probe_suggestion")
            board.state_tree[next_id]["probe_suggestion"] = None
            _set_focus(board, next_id, 1)
            return probe or "继续倾听。如果候选人停顿或跑偏，用一句话把他拉回正轨。"

        return "继续倾听。如果候选人停顿或跑偏，用一句话把他拉回正轨。"


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
