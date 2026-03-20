import asyncio
from typing import List
from .types import IBlackboardStore, IEventBus, IEvaluator, IActor, ChatMessage


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
            asyncio.create_task(self._run_evaluators(session_id))
        elif new_state == "AI_ACTING":
            asyncio.create_task(self._run_actor(session_id))

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
            return

        # 并发执行所有 Evaluator
        tasks = [evaluator.evaluate(board) for evaluator in self.evaluators]
        patches = await asyncio.gather(*tasks)

        # 展平并合并所有的 JSON Patch
        merged_patch: dict = {}
        for p in patches:
            merged_patch.update(p)

        # 强制将状态流转至发声阶段
        merged_patch["control_state"] = "AI_ACTING"
        await self.store.update_patch(session_id, merged_patch)
        await self.bus.emit("STATE_CHANGED", session_id, "AI_ACTING")

    async def _run_actor(self, session_id: str) -> None:
        board = await self.store.get(session_id)
        if not board:
            return

        response = await self.actor.act(board)

        if response.message:
            await self.store.append_message(
                session_id, ChatMessage(role="assistant", content=response.message)
            )

        if not response.end_session:
            await self.store.update_patch(
                session_id, {"control_state": "USER_TURN"}
            )
            await self.bus.emit("STATE_CHANGED", session_id, "USER_TURN")
