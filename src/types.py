import time
from abc import ABC, abstractmethod
from typing import Dict, List, Any, Literal, Optional
from pydantic import BaseModel, Field

# 1. 令牌环的三个绝对状态
ControlState = Literal["USER_TURN", "AI_EVALUATING", "AI_ACTING"]

# 2. 状态增量 (JSON Patch)
Patch = Dict[str, Any]


class ChatMessage(BaseModel):
    role: Literal["user", "assistant", "system"]
    content: str
    timestamp: float = Field(default_factory=time.time)


# 3. 黑板：框架的 Single Source of Truth
class Blackboard(BaseModel):
    session_id: str
    control_state: ControlState
    state_tree: Dict[str, Any]  # 用户的业务状态树 (Rubric)
    history: List[ChatMessage] = Field(default_factory=list)
    context: Dict[str, Any] = Field(default_factory=dict)


class ActionResponse(BaseModel):
    message: Optional[str] = None
    tool_calls: Optional[List[Any]] = None
    end_session: bool = False


# ==========================================
# 核心接口定义 (Ports)
# ==========================================


class IEvaluator(ABC):
    @property
    @abstractmethod
    def name(self) -> str:
        pass

    @abstractmethod
    async def evaluate(self, board: Blackboard) -> Patch:
        """纯逻辑函数，只读黑板，输出增量 Patch"""
        pass


class IActor(ABC):
    @abstractmethod
    async def act(self, board: Blackboard) -> ActionResponse:
        """根据最新状态树，生成动作或回复"""
        pass


class IBlackboardStore(ABC):
    @abstractmethod
    async def get(self, session_id: str) -> Optional[Blackboard]:
        pass

    @abstractmethod
    async def create(self, session_id: str, initial_state: Any) -> None:
        pass

    @abstractmethod
    async def update_patch(self, session_id: str, patch: Patch) -> None:
        pass

    @abstractmethod
    async def append_message(self, session_id: str, message: ChatMessage) -> None:
        pass


class IEventBus(ABC):
    @abstractmethod
    async def emit(self, event: str, session_id: str, new_state: ControlState) -> None:
        pass

    @abstractmethod
    def on(self, event: str, handler: callable) -> None:
        pass
