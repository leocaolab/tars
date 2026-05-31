import asyncio
import functools
import logging
from typing import Any, Callable, Dict, List
from ..types import (
    IBlackboardStore,
    IEventBus,
    Blackboard,
    Patch,
    ChatMessage,
    ControlState,
)

logger = logging.getLogger(__name__)


class LocalEventBus(IEventBus):
    def __init__(self):
        self._handlers: Dict[str, List[Callable]] = {}

    def on(self, event: str, handler: Callable) -> None:
        if event not in self._handlers:
            self._handlers[event] = []
        self._handlers[event].append(handler)

    async def emit(self, event: str, session_id: str, new_state: ControlState) -> None:
        if event not in self._handlers:
            return
        for handler in self._handlers[event]:
            # Isolate handlers: a failure in one must not prevent the
            # remaining handlers from running.
            try:
                if asyncio.iscoroutinefunction(handler):
                    await handler(session_id, new_state)
                else:
                    # Run sync handlers off the event loop so a blocking
                    # handler does not stall the loop.
                    loop = asyncio.get_running_loop()
                    await loop.run_in_executor(
                        None, functools.partial(handler, session_id, new_state)
                    )
            except Exception:
                logger.exception(
                    "Event handler for %r failed (session_id=%s, new_state=%s)",
                    event,
                    session_id,
                    new_state,
                )


class MemoryStore(IBlackboardStore):
    def __init__(self):
        self.db: Dict[str, Blackboard] = {}

    async def get(self, session_id: str) -> Blackboard | None:
        board = self.db.get(session_id)
        return board.model_copy(deep=True) if board else None

    async def create(self, session_id: str, initial_state: Any) -> None:
        if session_id in self.db:
            raise KeyError(
                f"Session {session_id!r} already exists; "
                "refusing to overwrite existing history."
            )
        self.db[session_id] = Blackboard(
            session_id=session_id,
            control_state="USER_TURN",
            state_tree=initial_state,
        )

    async def append_message(self, session_id: str, message: ChatMessage) -> None:
        try:
            board = self.db[session_id]
        except KeyError:
            raise KeyError(f"Unknown session: {session_id!r}")
        board.history.append(message)

    async def update_patch(self, session_id: str, patch: Patch) -> None:
        try:
            board = self.db[session_id]
        except KeyError:
            raise KeyError(f"Unknown session: {session_id!r}")

        for key, value in patch.items():
            if key == "control_state":
                board.control_state = value
            elif key.startswith("state_tree."):
                state_key = key.split(".", 1)[1]
                board.state_tree[state_key] = value
            elif key.startswith("context."):
                ctx_key = key.split(".", 1)[1]
                board.context[ctx_key] = value
