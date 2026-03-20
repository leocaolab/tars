"""UBE Core Engine — stateless state-machine dispatcher, no UI, no business knowledge."""

from typing import Callable, List, Optional
from .types import Blackboard, Patch, IEvaluator, IActor, DirectiveExtractor, TerminationChecker

# Event types
EVENT_EVALUATING_START = "EVALUATING_START"
EVENT_EVALUATING_DONE = "EVALUATING_DONE"
EVENT_ACTING_START = "ACTING_START"
EVENT_ACTING_DONE = "ACTING_DONE"
EVENT_TURN_COMPLETE = "TURN_COMPLETE"
EVENT_SESSION_END = "SESSION_END"

# Callback signature: (event, board, extra_dict) -> None
EventCallback = Callable[[str, Blackboard, dict], None]


class AgentEngine:
    """Universal Blackboard Engine — inversion of control, event-driven.

    The engine knows nothing about business data. It only:
    1. Appends user input to history
    2. Calls Evaluators to collect Patches
    3. Merges patches via business-layer merge_patch
    4. Checks termination via TerminationChecker
    5. Extracts directive via DirectiveExtractor
    6. Feeds directive to Actor for reply
    7. Emits events at each stage
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
        """Generate opening: extract directive -> Actor speaks -> append to history."""
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
        """Process one turn of user input. Returns (reply, terminated).

        Flow: append input -> evaluate -> merge patches -> check termination -> extract directive -> act.
        """
        board.history.append({"role": "user", "content": user_input})

        # Phase A: Evaluate
        self._emit(EVENT_EVALUATING_START, board)
        patches: list[Patch] = []
        for ev in self.evaluators:
            patch = ev.evaluate(board, user_input)
            patches.append(patch)

        for patch in patches:
            self.merge_patch(board, patch)

        self._emit(EVENT_EVALUATING_DONE, board, patches=patches)

        # Phase B: Termination check
        terminated = False
        if self.termination_checker and self.termination_checker.should_terminate(board):
            terminated = True
            self._emit(EVENT_SESSION_END, board)

        # Phase C: Actor speaks
        directive = self.directive_extractor.extract(board)
        self._emit(EVENT_ACTING_START, board, directive=directive)

        history = [m for m in board.history if m["role"] != "system"]
        reply = self.actor.act_with_history(directive, history)
        board.history.append({"role": "assistant", "content": reply})

        self._emit(EVENT_ACTING_DONE, board, reply=reply)
        self._emit(EVENT_TURN_COMPLETE, board)
        return reply, terminated
