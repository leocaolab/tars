"""UBE Core Directive Router — generic focus management + priority routing.

Framework component implementing the DirectiveExtractor interface.
Business layer only provides a NodeScorer (scoring formula);
routing, locking, and timeout are handled by the framework.
"""

from .types import Blackboard, DirectiveExtractor, NodeScorer


class DirectiveRouter(DirectiveExtractor):
    """Generic directive router with focus lock and depth timeout.

    Mechanisms:
    1. Focus Lock: locks onto one node for consecutive follow-ups
    2. Depth Timeout: force-switch after max_depth_turns on same node
    3. Priority Routing: picks highest-scored node via NodeScorer
    4. Prerequisite Blocking: via NodeScorer.get_prerequisites()
    """

    def __init__(
        self,
        scorer: NodeScorer,
        max_depth_turns: int = 2,
        all_terminal_message: str = "All dimensions have been evaluated. Wrap up and conclude.",
        timeout_message: str = (
            "Enough depth on this topic. "
            "Gracefully close it and move on to the next key question."
        ),
        fallback_message: str = "Keep listening. If they stall or go off track, nudge them back.",
    ):
        self._scorer = scorer
        self._max_depth = max_depth_turns
        self._all_terminal_msg = all_terminal_message
        self._timeout_msg = timeout_message
        self._fallback_msg = fallback_message

    # --- Focus state (stored in board.context) ---

    def _get_focus(self, board: Blackboard) -> tuple[str | None, int]:
        return (
            board.context.get("_focused_node_id"),
            board.context.get("_focused_turn_count", 0),
        )

    def _set_focus(self, board: Blackboard, node_id: str | None, turns: int = 0):
        board.context["_focused_node_id"] = node_id
        board.context["_focused_turn_count"] = turns

    # --- Prerequisite check ---

    def _prerequisites_met(self, node_id: str, board: Blackboard) -> bool:
        prereqs = self._scorer.get_prerequisites(node_id, board)
        for pid in prereqs:
            pnode = board.state_tree.get(pid)
            if pnode and pnode.get("status", "INIT") == "INIT":
                return False
        return True

    # --- Node selection ---

    def _pick_next(self, board: Blackboard) -> str | None:
        candidates = []
        for node_id, node in board.state_tree.items():
            if self._scorer.is_terminal(node):
                continue
            if not self._scorer.get_probe_text(node):
                continue
            if not self._prerequisites_met(node_id, board):
                continue
            score = self._scorer.score(node_id, node, board)
            candidates.append((score, node_id))

        candidates.sort(reverse=True)
        return candidates[0][1] if candidates else None

    # --- Core extract ---

    def extract(self, board: Blackboard) -> str:
        # All terminal
        if all(self._scorer.is_terminal(n) for n in board.state_tree.values()):
            self._set_focus(board, None)
            return self._all_terminal_msg

        focused_id, turn_count = self._get_focus(board)

        # === Currently locked ===
        if focused_id and focused_id in board.state_tree:
            node = board.state_tree[focused_id]

            if self._scorer.is_terminal(node):
                self._set_focus(board, None)

            elif turn_count >= self._max_depth:
                self._set_focus(board, None)
                return self._timeout_msg

            else:
                probe = self._scorer.get_probe_text(node)
                if probe:
                    if isinstance(node, dict):
                        node["probe_suggestion"] = None
                    self._set_focus(board, focused_id, turn_count + 1)
                    return probe
                else:
                    self._set_focus(board, None)

        # === Pick next ===
        next_id = self._pick_next(board)
        if next_id:
            node = board.state_tree[next_id]
            probe = self._scorer.get_probe_text(node)
            if isinstance(node, dict):
                node["probe_suggestion"] = None
            self._set_focus(board, next_id, 1)
            return probe or self._fallback_msg

        return self._fallback_msg
