"""UBE Core — 纯粹的框架契约，零业务知识。"""

from abc import ABC, abstractmethod
from typing import Any, Dict, List, Optional
from pydantic import BaseModel, Field


class Blackboard(BaseModel):
    """框架的 Single Source of Truth。

    框架不关心 state_tree 里面装的是什么，
    也不关心 context 里面有什么业务属性。
    它只负责在 Evaluator 和 Actor 之间流转控制权。
    """
    session_id: str
    context: Dict[str, Any] = Field(default_factory=dict, description="业务层的只读上下文")
    state_tree: Dict[str, Any] = Field(default_factory=dict, description="业务层的可变状态树")
    history: List[Dict[str, str]] = Field(default_factory=list, description="对话记录")


class Patch(BaseModel):
    """Evaluator 输出的增量补丁 — 框架只知道这是一个字典。"""
    updates: Dict[str, Any] = Field(default_factory=dict)


class IEvaluator(ABC):
    """后台评估器接口 — 只读黑板，输出 Patch。"""

    @abstractmethod
    def evaluate(self, board: Blackboard, user_input: str) -> Patch:
        """纯函数：黑板 + 用户输入 → 增量补丁"""

    @property
    def name(self) -> str:
        return self.__class__.__name__


class IActor(ABC):
    """前台发声器接口 — 接收导演指令，生成回复。"""

    @abstractmethod
    def act(self, directive: str) -> str:
        """单轮：导演指令 → 回复文本"""

    @abstractmethod
    def act_with_history(self, directive: str, history: list[dict[str, str]]) -> str:
        """多轮：导演指令 + 对话历史 → 回复文本"""


class DirectiveExtractor(ABC):
    """导演指令提取器接口 — 业务层实现，从黑板中提取一条纯文本指令给 Actor。"""

    @abstractmethod
    def extract(self, board: Blackboard) -> str:
        """从黑板状态中提取最紧急的一条导演指令。"""


class TerminationChecker(ABC):
    """终态检测器接口 — 业务层实现，判断是否应该结束会话。"""

    @abstractmethod
    def should_terminate(self, board: Blackboard) -> bool:
        """检查黑板状态，返回 True 表示会话应该结束。"""
