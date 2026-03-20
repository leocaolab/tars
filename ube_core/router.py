"""UBE Core Directive Router — 通用的焦点管理 + 优先级路由。

框架级组件，实现了 DirectiveExtractor 接口。
业务层只需要提供 NodeScorer（评分公式），路由、锁定、熔断全由框架处理。
"""

from .types import Blackboard, DirectiveExtractor, NodeScorer


class DirectiveRouter(DirectiveExtractor):
    """通用指令路由器 — 带焦点锁定和深度熔断。

    机制：
    1. 焦点锁 (Focus Lock): 锁定一个节点连续追问，防止跳来跳去
    2. 深度熔断 (Depth Timeout): 同一节点最多 max_depth_turns 轮后强制切题
    3. 优先级路由: 通过业务层的 NodeScorer 评分，选最该问的节点
    4. 前置依赖: 通过 NodeScorer.get_prerequisites() 阻止跨级追问
    """

    def __init__(
        self,
        scorer: NodeScorer,
        max_depth_turns: int = 2,
        all_terminal_message: str = "所有维度已评估完毕。请总结并结束。",
        timeout_message: str = (
            "在当前话题上的深挖已经足够了。"
            "请用一句话优雅地收束，然后转移到下一个关键问题。"
        ),
        fallback_message: str = "继续倾听。如果对方停顿或跑偏，用一句话拉回正轨。",
    ):
        self._scorer = scorer
        self._max_depth = max_depth_turns
        self._all_terminal_msg = all_terminal_message
        self._timeout_msg = timeout_message
        self._fallback_msg = fallback_message

    # --- Focus state management (stored in board.context) ---

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
        # All terminal → farewell
        if all(self._scorer.is_terminal(n) for n in board.state_tree.values()):
            self._set_focus(board, None)
            return self._all_terminal_msg

        focused_id, turn_count = self._get_focus(board)

        # === Currently locked ===
        if focused_id and focused_id in board.state_tree:
            node = board.state_tree[focused_id]

            # Natural unlock (terminal)
            if self._scorer.is_terminal(node):
                self._set_focus(board, None)

            # Depth timeout
            elif turn_count >= self._max_depth:
                self._set_focus(board, None)
                return self._timeout_msg

            # Continue probing
            else:
                probe = self._scorer.get_probe_text(node)
                if probe:
                    # Consume probe to prevent repetition
                    if isinstance(node, dict):
                        node["probe_suggestion"] = None
                    self._set_focus(board, focused_id, turn_count + 1)
                    return probe
                else:
                    self._set_focus(board, None)

        # === Pick next node ===
        next_id = self._pick_next(board)
        if next_id:
            node = board.state_tree[next_id]
            probe = self._scorer.get_probe_text(node)
            if isinstance(node, dict):
                node["probe_suggestion"] = None
            self._set_focus(board, next_id, 1)
            return probe or self._fallback_msg

        return self._fallback_msg
