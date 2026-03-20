"""UBE Core Engine — 冷酷无情的状态机调度器，无 UI，无业务知识。"""

from typing import Callable, List, Optional
from .types import Blackboard, Patch, IEvaluator, IActor, DirectiveExtractor, TerminationChecker

# 事件类型
EVENT_EVALUATING_START = "EVALUATING_START"
EVENT_EVALUATING_DONE = "EVALUATING_DONE"
EVENT_ACTING_START = "ACTING_START"
EVENT_ACTING_DONE = "ACTING_DONE"
EVENT_TURN_COMPLETE = "TURN_COMPLETE"
EVENT_SESSION_END = "SESSION_END"

# 回调签名: (event: str, board: Blackboard, extra: dict) -> None
EventCallback = Callable[[str, Blackboard, dict], None]


class AgentEngine:
    """通用黑板引擎 — 控制反转，事件驱动。

    引擎不知道黑板里有什么业务数据，不知道 Evaluator 在评估什么，
    不知道 Actor 在说什么。它只负责：
    1. 把用户输入追加到 history
    2. 依次调用所有 Evaluator，收集 Patch
    3. 调用业务层的 merge_patch 合并补丁
    4. 调用业务层的 DirectiveExtractor 提取指令
    5. 把指令喂给 Actor 生成回复
    6. 在每个阶段触发事件回调
    """

    def __init__(
        self,
        evaluators: List[IEvaluator],
        actor: IActor,
        directive_extractor: DirectiveExtractor,
        merge_patch: Callable[[Blackboard, Patch], None],
        termination_checker: Optional[TerminationChecker] = None,
        on_event: Optional[EventCallback] = None,
    ):
        self.evaluators = evaluators
        self.actor = actor
        self.directive_extractor = directive_extractor
        self.merge_patch = merge_patch
        self.termination_checker = termination_checker
        self.on_event = on_event or (lambda *_: None)

    def _emit(self, event: str, board: Blackboard, **extra):
        self.on_event(event, board, extra)

    def generate_greeting(self, board: Blackboard) -> str:
        """生成开场白：提取指令 → Actor 发声 → 追加到 history。"""
        directive = self.directive_extractor.extract(board)
        self._emit(EVENT_ACTING_START, board, directive=directive)

        history = [m for m in board.history if m["role"] != "system"]
        if history:
            reply = self.actor.act_with_history(directive, history)
        else:
            reply = self.actor.act(directive)

        board.history.append({"role": "assistant", "content": reply})
        self._emit(EVENT_ACTING_DONE, board, reply=reply)
        return reply

    def push_input(self, board: Blackboard, user_input: str) -> tuple[str, bool]:
        """处理一轮用户输入，返回 (Actor 回复, 是否应终止)。

        完整流程：追加输入 → Evaluator 评估 → 合并补丁 → 终态检测 → 提取指令 → Actor 回复。
        """
        board.history.append({"role": "user", "content": user_input})

        # 阶段 A：Evaluator 评估
        self._emit(EVENT_EVALUATING_START, board)
        patches: list[Patch] = []
        for ev in self.evaluators:
            patch = ev.evaluate(board, user_input)
            patches.append(patch)

        # 合并所有补丁（业务层定义合并逻辑）
        for patch in patches:
            self.merge_patch(board, patch)

        self._emit(EVENT_EVALUATING_DONE, board, patches=patches)

        # 阶段 B：终态检测
        terminated = False
        if self.termination_checker and self.termination_checker.should_terminate(board):
            terminated = True
            self._emit(EVENT_SESSION_END, board)

        # 阶段 C：Actor 发声
        directive = self.directive_extractor.extract(board)
        self._emit(EVENT_ACTING_START, board, directive=directive)

        history = [m for m in board.history if m["role"] != "system"]
        reply = self.actor.act_with_history(directive, history)
        board.history.append({"role": "assistant", "content": reply})

        self._emit(EVENT_ACTING_DONE, board, reply=reply)
        self._emit(EVENT_TURN_COMPLETE, board)
        return reply, terminated
