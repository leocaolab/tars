"""面试考官 — 实现框架的 IEvaluator 接口。"""

import json
import sys
from typing import List

from ube_core import Blackboard, Patch, IEvaluator
from ube_core.llm import LLMClient
from .models import EvaluatorPatch, RubricDimension

# 终态：不再需要评估
_TERMINAL = {"SATISFIED", "FATAL_FLAW"}


class InterviewEvaluator(IEvaluator):

    def __init__(self, client: LLMClient, target_dimensions: List[RubricDimension]):
        self._client = client
        self.target_dimensions = target_dimensions

    @staticmethod
    def _get_status(node) -> str:
        """兼容 dict 或 Pydantic 对象"""
        if isinstance(node, dict):
            return node.get("status", "INIT")
        return getattr(node, "status", "INIT")

    @staticmethod
    def _node_to_dict(node) -> dict:
        """兼容 dict 或 Pydantic 对象的序列化"""
        if hasattr(node, "model_dump"):
            return node.model_dump()
        return node

    def evaluate(self, board: Blackboard, user_input: str) -> Patch:
        target_ids = [d.node_id for d in self.target_dimensions]

        # === Fix 3: 前置剪枝 — 终态维度不浪费 Token ===
        active_ids = [
            tid for tid in target_ids
            if tid in board.state_tree
            and self._get_status(board.state_tree[tid]) not in _TERMINAL
        ]
        if not active_ids:
            return Patch(updates={})

        # === Fix 1: 安全序列化 — 兼容 Pydantic 对象和 dict ===
        slice_state = {
            k: self._node_to_dict(v)
            for k, v in board.state_tree.items()
            if k in active_ids
        }

        # 只发送活跃维度的规则（节省 Token）
        active_rules = "\n".join(
            f"  - [{d.category}] {d.node_id}: {d.eval_rule}"
            for d in self.target_dimensions
            if d.node_id in active_ids
        )

        ctx = board.context
        schema_hint = json.dumps(
            EvaluatorPatch.model_json_schema(), ensure_ascii=False, indent=2
        )

        system_prompt = (
            "你是一位严苛的系统设计考官。你只输出 JSON，绝不输出任何其他内容。\n"
            f"请严格按照以下 JSON Schema 返回：\n{schema_hint}"
        )

        # === Fix 2: 补充考官上一句提问，消除上下文致盲 ===
        last_question = ""
        for msg in reversed(board.history):
            if msg.get("role") == "assistant":
                last_question = msg["content"]
                break

        user_prompt = (
            f"当前面试级别：{ctx.get('interview_level', '')}\n"
            f"当前题目：{ctx.get('topic', '')}\n\n"
            f"【物理约束】: {json.dumps(ctx.get('global_constants', {}), ensure_ascii=False)}\n\n"
            f"【你负责监控的能力维度及评判规则】:\n{active_rules}\n\n"
            f"【各维度当前运行时状态（累积记分牌）】:\n"
            f"{json.dumps(slice_state, ensure_ascii=False, indent=2)}\n\n"
            f"【考官的最新提问】: \"{last_question}\"\n"
            f"【候选人的回答】: \"{user_input}\"\n\n"
            "=== 状态机规则 ===\n"
            "- INIT: 该维度尚未被候选人触及\n"
            "- GATHERING_SIGNALS: 候选人开始涉及但证据不足以判定\n"
            "- SATISFIED: 该维度已充分展现所需能力\n"
            "- NEEDS_PROBING: 存在疑点或盲点，需要前台追问\n"
            "- FATAL_FLAW: 出现致命逻辑漏洞\n\n"
            "=== 输出要求 ===\n"
            "- internal_thought: 逐维度分析候选人此轮发言\n"
            "- updates: Dict[node_id → 新状态]，key 必须是上面列出的 node_id（如 \"discovery.cujs_and_metrics\"）\n"
            "- new_positive_signals: Dict[node_id → 一句话正面证据]，key 必须是 node_id\n"
            "- new_negative_signals: Dict[node_id → 一句话负面证据]，key 必须是 node_id\n"
            '- probe_suggestions: Dict[node_id → {"question": "追问话术", "urgency": 1-5}]，key 必须是 node_id\n'
            "  urgency 评分标准：5=致命架构缺陷必须立刻打断质问，3=重要但不紧急，1=小瑕疵稍后再问\n\n"
            "极其重要：所有字典的 key 必须使用上面【能力维度】中列出的 node_id（带点号的），"
            "例如 \"design.math_and_probabilistic\"。绝对不要使用自创的描述性 key。\n\n"
            "只返回纯 JSON，不要 markdown 代码块。"
        )

        # 重试最多 3 次
        last_err = None
        for attempt in range(3):
            raw = self._client.chat(system=system_prompt, user=user_prompt)

            text = raw.strip()
            if text.startswith("```"):
                text = text.split("\n", 1)[1] if "\n" in text else text[3:]
                if text.endswith("```"):
                    text = text[:-3]
                text = text.strip()

            start = text.find("{")
            end = text.rfind("}")
            if start != -1 and end != -1:
                text = text[start:end + 1]

            try:
                patch_data = EvaluatorPatch.model_validate_json(text)
                return Patch(updates={"_evaluator_patch": patch_data.model_dump()})
            except Exception as e:
                last_err = e

        print(f"[Evaluator] JSON 解析失败 (3 次重试): {last_err}", file=sys.stderr)
        return Patch(updates={})
