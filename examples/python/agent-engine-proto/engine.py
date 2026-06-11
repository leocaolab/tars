import asyncio
import logging
from typing import List
from .types import IBlackboardStore, IEventBus, IEvaluator, IActor, ChatMessage

logger = logging.getLogger(__name__)


class AgentEngine:
    def __init__(
        self,
        store: IBlackboardStore,
        bus: IEventBus,
        evaluators: List[IEvaluator],
        actor: IActor,
    ):
        self.store = store
        self.bus = bus
        self.evaluators = evaluators
        self.actor = actor
        self._register_event_handlers()

    def _register_event_handlers(self):
        self.bus.on("STATE_CHANGED", self._handle_state_change)

    async def _handle_state_change(self, session_id: str, new_state: str):
        if new_state == "AI_EVALUATING":
            self._spawn(self._run_evaluators(session_id), session_id, new_state)
        elif new_state == "AI_ACTING":
            self._spawn(self._run_actor(session_id), session_id, new_state)

    def _spawn(self, coro, session_id: str, phase: str) -> None:
        """Launch a background phase task, ensuring failures are surfaced.

        Without a done-callback an exception in the spawned coroutine is
        swallowed, leaving the session stuck in its AI_* state. We log the
        failure and attempt to recover the session back to USER_TURN.
        """
        task = asyncio.create_task(coro)

        def _on_done(t: asyncio.Task) -> None:
            try:
                exc = t.exception()
            except asyncio.CancelledError:
                exc = None
            if exc is not None:
                logger.error(
                    "Background phase %s failed for session %s",
                    phase,
                    session_id,
                    exc_info=exc,
                )
                asyncio.create_task(self._recover_session(session_id))

        task.add_done_callback(_on_done)

    async def _recover_session(self, session_id: str) -> None:
        """Release a stuck session by returning control to the user."""
        try:
            await self.store.update_patch(
                session_id, {"control_state": "USER_TURN"}
            )
            await self.bus.emit("STATE_CHANGED", session_id, "USER_TURN")
        except Exception:
            logger.exception(
                "Failed to recover stuck session %s", session_id
            )

    async def push_input(self, session_id: str, user_text: str) -> None:
        """对外暴露的唯一 API：用户提交输入"""
        await self.store.append_message(
            session_id, ChatMessage(role="user", content=user_text)
        )

        # 剥夺用户控制权，启动评估阶段
        await self.store.update_patch(session_id, {"control_state": "AI_EVALUATING"})
        await self.bus.emit("STATE_CHANGED", session_id, "AI_EVALUATING")

    async def _run_evaluators(self, session_id: str) -> None:
        board = await self.store.get(session_id)
        if not board:
            logger.error(
                "Session %s missing during AI_EVALUATING; recovering.",
                session_id,
            )
            await self._recover_session(session_id)
            return

        # 并发执行所有 Evaluator —— 单个失败不应让其他结果丢失
        tasks = [evaluator.evaluate(board) for evaluator in self.evaluators]
        results = await asyncio.gather(*tasks, return_exceptions=True)

        # 展平并合并所有的 JSON Patch
        merged_patch: dict = {}
        for evaluator, result in zip(self.evaluators, results):
            if isinstance(result, BaseException):
                logger.error(
                    "Evaluator %s failed for session %s",
                    getattr(evaluator, "name", evaluator),
                    session_id,
                    exc_info=result,
                )
                continue
            for key, value in result.items():
                if key in merged_patch and merged_patch[key] != value:
                    logger.warning(
                        "Patch key %r overwritten while merging evaluator %s "
                        "output (session %s): %r -> %r",
                        key,
                        getattr(evaluator, "name", evaluator),
                        session_id,
                        merged_patch[key],
                        value,
                    )
                merged_patch[key] = value

        # 强制将状态流转至发声阶段
        merged_patch["control_state"] = "AI_ACTING"
        await self.store.update_patch(session_id, merged_patch)
        await self.bus.emit("STATE_CHANGED", session_id, "AI_ACTING")

    async def _run_actor(self, session_id: str) -> None:
        board = await self.store.get(session_id)
        if not board:
            logger.error(
                "Session %s missing during AI_ACTING; recovering.",
                session_id,
            )
            await self._recover_session(session_id)
            return

        try:
            response = await self.actor.act(board)
        except Exception:
            logger.exception(
                "Actor failed for session %s; recovering.", session_id
            )
            await self._recover_session(session_id)
            return

        if response.message:
            await self.store.append_message(
                session_id, ChatMessage(role="assistant", content=response.message)
            )

        if not response.end_session:
            await self.store.update_patch(
                session_id, {"control_state": "USER_TURN"}
            )
            await self.bus.emit("STATE_CHANGED", session_id, "USER_TURN")
