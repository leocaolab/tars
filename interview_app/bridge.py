"""Business bridge — scorer, merge, termination. Uses ube_core generic merge."""

from ube_core import Blackboard, Patch, NodeScorer, TerminationChecker
from ube_core.merge import merge_evaluator_patch

_STATUS_WEIGHT = {
    "FATAL_FLAW": 4, "NEEDS_PROBING": 3,
    "GATHERING_SIGNALS": 2, "INIT": 1, "SATISFIED": 0,
}
_PHASES = {"early": (0, 3), "mid": (3, 7), "late": (7, 999), "any": (0, 999)}


def merge_patch(board: Blackboard, patch: Patch) -> None:
    """Delegate to framework's generic merge."""
    merge_evaluator_patch(board, patch)


def _get_turn(board: Blackboard) -> int:
    return sum(1 for m in board.history if m.get("role") == "user")


def _current_phase(board: Blackboard) -> str:
    turn = _get_turn(board)
    for phase, (lo, hi) in _PHASES.items():
        if phase != "any" and lo <= turn < hi:
            return phase
    return "late"


def _dim_meta(node_id: str, board: Blackboard) -> dict:
    for d in board.context.get("rubric", []):
        if d.get("node_id") == node_id:
            return d
    return {}


class InterviewScorer(NodeScorer):
    """score = (100-priority) + (status*20) + phase_bonus + (urgency*15)"""

    def score(self, node_id: str, node: dict, board: Blackboard) -> float:
        meta = _dim_meta(node_id, board)
        s = 100.0 - meta.get("priority", 50)
        s += _STATUS_WEIGHT.get(node.get("status", "INIT"), 0) * 20
        s += node.get("probe_urgency", 3) * 15
        cur = _current_phase(board)
        nph = meta.get("phase", "any")
        if nph == "any" or nph == cur:
            s += 200
        elif (cur == "mid" and nph == "early") or (cur == "late" and nph == "mid"):
            s += 50
        return s

    def get_prerequisites(self, node_id: str, board: Blackboard) -> list[str]:
        return _dim_meta(node_id, board).get("prerequisites", [])


class InterviewTerminationChecker(TerminationChecker):

    def should_terminate(self, board: Blackboard) -> bool:
        nodes = list(board.state_tree.values())
        if not nodes:
            return False
        if sum(1 for n in nodes if n.get("status") == "FATAL_FLAW") >= 2:
            return True
        return all(n.get("status") not in ("INIT", "GATHERING_SIGNALS", "NEEDS_PROBING") for n in nodes)
