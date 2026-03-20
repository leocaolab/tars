"""UBE Core — pure framework contracts, zero business knowledge."""

from abc import ABC, abstractmethod
from typing import Any, Dict, List, Optional
from pydantic import BaseModel, Field


class Blackboard(BaseModel):
    """The framework's Single Source of Truth.

    The framework does not care what is inside state_tree or context.
    It only manages control flow between Evaluators and Actors.
    """
    session_id: str
    context: Dict[str, Any] = Field(default_factory=dict, description="Business-layer read context")
    state_tree: Dict[str, Any] = Field(default_factory=dict, description="Business-layer mutable state tree")
    history: List[Dict[str, str]] = Field(default_factory=list, description="Conversation history")


class Patch(BaseModel):
    """Evaluator output — an incremental update dict."""
    updates: Dict[str, Any] = Field(default_factory=dict)


class IEvaluator(ABC):
    """Backend evaluator interface — reads board, outputs Patch."""

    @abstractmethod
    def evaluate(self, board: Blackboard, user_input: str) -> Patch:
        """Pure function: board + user input -> incremental patch."""

    @property
    def name(self) -> str:
        return self.__class__.__name__


class IActor(ABC):
    """Frontend speaker interface — receives directive, generates reply."""

    @abstractmethod
    def act(self, directive: str) -> str:
        """Single-turn: directive -> reply text."""

    @abstractmethod
    def act_with_history(self, directive: str, history: list[dict[str, str]]) -> str:
        """Multi-turn: directive + chat history -> reply text."""


class DirectiveExtractor(ABC):
    """Directive extractor interface — business layer extracts a plain-text directive from the board."""

    @abstractmethod
    def extract(self, board: Blackboard) -> str:
        """Extract the most urgent directive from the board state."""


class TerminationChecker(ABC):
    """Termination checker interface — decides whether to end the session."""

    @abstractmethod
    def should_terminate(self, board: Blackboard) -> bool:
        """Return True if the session should end."""


class NodeScorer(ABC):
    """Node scorer interface — business implements the scoring formula, framework handles routing.

    The framework calls score() on all active nodes and picks the highest.
    """

    @abstractmethod
    def score(self, node_id: str, node: dict, board: Blackboard) -> float:
        """Score a node for priority. Higher = more urgent to probe."""

    def get_probe_text(self, node: dict) -> str | None:
        """Extract probe text from a node. Default: reads 'probe_suggestion'."""
        return node.get("probe_suggestion")

    def is_terminal(self, node: dict) -> bool:
        """Check if a node is in terminal state. Default: SATISFIED or FATAL_FLAW."""
        return node.get("status") in ("SATISFIED", "FATAL_FLAW")

    def get_prerequisites(self, node_id: str, board: Blackboard) -> list[str]:
        """Return prerequisite node IDs. Default: empty."""
        return []
