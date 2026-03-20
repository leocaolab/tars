"""业务桥接层 — merge_patch, DirectiveExtractor (含考点锁+深度熔断), TerminationChecker。"""

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

# 考点静态优先级 — P0 最高，决定深挖顺序
_NODE_PRIORITY = {
    "discovery.cujs_and_metrics": 0,       # P0: 需求不清绝不往后走
    "design.logical_consistency": 1,       # P1: 架构逻辑自洽
    "design.math_and_probabilistic": 2,    # P2: 容量估算
    "design.tradeoffs_and_rationale": 3,   # P3: 方案权衡
    "design.ilities_and_tradeoffs": 4,     # P4: 非功能性
    "design.component_deep_dive": 5,       # P5: 底层深挖（前置稳了才深挖）
    "knowledge.factual_correctness": 6,    # P6: 事实核查
    "execution.stress_testing": 7,         # P7: 极端压力
    "communication.clarity": 99,           # 最低: 全盘评估，不需要专门发问
}

# 深度熔断阈值：同一考点最多连续追问 N 轮
MAX_DEPTH_TURNS = 2

# 终态集合
_TERMINAL = {"SATISFIED", "FATAL_FLAW"}


def _resolve_node_id(key: str, valid_ids: set[str]) -> str | None:
    """尝试将 LLM 返回的 key 匹配到合法的 node_id。"""
    if key in valid_ids:
        return key
    key_lower = key.lower().replace("_", ".").replace("-", ".")
    for nid in valid_ids:
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


def _get_focus(board: Blackboard) -> tuple[str | None, int]:
    """从 context 读取当前考点锁状态。"""
    return (
        board.context.get("_focused_node_id"),
        board.context.get("_focused_turn_count", 0),
    )


def _set_focus(board: Blackboard, node_id: str | None, turns: int = 0) -> None:
    """写入考点锁状态到 context。"""
    board.context["_focused_node_id"] = node_id
    board.context["_focused_turn_count"] = turns


def _pick_next_node(board: Blackboard) -> str | None:
    """按优先级挑选下一个需要追问的考点。"""
    candidates = []
    for node_id, node in board.state_tree.items():
        status = node.get("status", "INIT")
        if status in _TERMINAL:
            continue
        probe = node.get("probe_suggestion")
        if not probe:
            continue
        # 组合排序：先按状态紧急度降序，再按考点优先级升序
        candidates.append((
            -_STATUS_PRIORITY.get(status, 0),
            _NODE_PRIORITY.get(node_id, 50),
            node_id,
        ))
    candidates.sort()
    return candidates[0][2] if candidates else None


class InterviewDirectiveExtractor(DirectiveExtractor):
    """从面试黑板中提取导演指令。

    核心机制：
    1. 考点锁 (Topic Lock): 锁定一个考点连续追问，防止跳来跳去
    2. 深度熔断 (Depth Timeout): 同一考点最多追问 MAX_DEPTH_TURNS 轮后强制切题
    3. 优先级队列: 按 _NODE_PRIORITY 选择下一个考点
    """

    def extract(self, board: Blackboard) -> str:
        # 全部终态 → 收尾
        if all(n.get("status") in _TERMINAL for n in board.state_tree.values()):
            _set_focus(board, None)
            return "所有考核维度已评估完毕。用一句话给候选人总结，结束面试。"

        focused_id, turn_count = _get_focus(board)

        # === 如果当前有锁定的考点 ===
        if focused_id and focused_id in board.state_tree:
            node = board.state_tree[focused_id]
            status = node.get("status", "INIT")

            # 考点已到终态 → 自然解锁
            if status in _TERMINAL:
                _set_focus(board, None)
                # 继续往下选新考点

            # 深度熔断 → 强制切题
            elif turn_count >= MAX_DEPTH_TURNS:
                _set_focus(board, None)
                return (
                    f"在当前底层话题上的深挖已经足够了。"
                    f"请用一句话优雅地收束当前话题（如'好的，这块我大概了解你的深度了'），"
                    f"然后立即将话题转移到系统设计的下一个关键问题。"
                )

            # 正常深挖 → 继续追问，轮数 +1
            else:
                probe = node.get("probe_suggestion")
                if probe:
                    node["probe_suggestion"] = None  # 消费
                    _set_focus(board, focused_id, turn_count + 1)
                    return probe
                else:
                    # 没有新的 probe → 解锁，选下一个
                    _set_focus(board, None)

        # === 选择下一个考点锁定 ===
        next_id = _pick_next_node(board)
        if next_id:
            probe = board.state_tree[next_id].get("probe_suggestion")
            board.state_tree[next_id]["probe_suggestion"] = None  # 消费
            _set_focus(board, next_id, 1)
            return probe or "继续倾听。如果候选人停顿或跑偏，用一句话把他拉回正轨。"

        return "继续倾听。如果候选人停顿或跑偏，用一句话把他拉回正轨。"


class InterviewTerminationChecker(TerminationChecker):
    """面试终态检测器。"""

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
        if pending == 0:
            return True

        return False
