"""后台考官 Agent：纯函数，只读黑板 + 用户发言 → 吐 JSON Patch，绝不和用户说话"""

import json
from typing import List

from .llm import LLMClient
from .models import Blackboard, EvaluatorPatch, RubricDimension


class EvaluatorAgent:

    def __init__(self, client: LLMClient, target_dimensions: List[RubricDimension]):
        self._client = client
        self.target_dimensions = target_dimensions

    def evaluate(self, board: Blackboard, user_input: str) -> EvaluatorPatch:
        # 切片：捞取负责的考点运行时状态
        target_ids = [d.node_id for d in self.target_dimensions]
        slice_state = {
            k: v.model_dump()
            for k, v in board.state_tree.items()
            if k in target_ids
        }

        # 构建每个维度的评判规则表
        rules_block = "\n".join(
            f"  - [{d.category}] {d.node_id}: {d.eval_rule}"
            for d in self.target_dimensions
        )

        # JSON schema 提示，让所有 provider 都能返回正确结构
        schema_hint = json.dumps(
            EvaluatorPatch.model_json_schema(), ensure_ascii=False, indent=2
        )

        system_prompt = (
            "你是一位严苛的系统设计考官。你只输出 JSON，绝不输出任何其他内容。\n"
            f"请严格按照以下 JSON Schema 返回：\n{schema_hint}"
        )

        user_prompt = (
            f"当前面试级别：{board.interview_level}\n"
            f"当前题目：{board.topic}\n\n"
            f"【物理约束】: {json.dumps(board.global_constants, ensure_ascii=False)}\n\n"
            f"【你负责监控的能力维度及评判规则】:\n{rules_block}\n\n"
            f"【各维度当前运行时状态（累积记分牌）】:\n"
            f"{json.dumps(slice_state, ensure_ascii=False, indent=2)}\n\n"
            f"【候选人最新发言】: \"{user_input}\"\n\n"
            "=== 状态机规则 ===\n"
            "- INIT: 该维度尚未被候选人触及\n"
            "- GATHERING_SIGNALS: 候选人开始涉及但证据不足以判定\n"
            "- SATISFIED: 该维度已充分展现所需能力\n"
            "- NEEDS_PROBING: 存在疑点或盲点，需要前台追问\n"
            "- FATAL_FLAW: 出现致命逻辑漏洞（如违反物理常识、方案自相矛盾）\n\n"
            "=== 输出要求 ===\n"
            "请严格基于能力维度的评判规则进行推演：\n"
            "- internal_thought: 逐维度分析候选人此轮发言\n"
            "- updates: 只包含状态确实需要变化的 node_id → 新状态\n"
            "- new_positive_signals: 正面证据（展现了该维度要求的思维能力）\n"
            "- new_negative_signals: 负面证据（盲点、逻辑漏洞、违背常识）\n"
            "- probe_suggestions: 当状态为 NEEDS_PROBING 时，给出尖锐的追问建议\n\n"
            "注意：不要因为候选人提到了某个技术名词就给 SATISFIED，"
            "要看他是否展现了该维度要求的思维能力。"
            "信号应该是具体的观察，不是空泛的评语。\n\n"
            "只返回纯 JSON，不要 markdown 代码块。"
        )

        raw = self._client.chat(system=system_prompt, user=user_prompt)

        # 清理可能的 markdown 包裹
        text = raw.strip()
        if text.startswith("```"):
            text = text.split("\n", 1)[1] if "\n" in text else text[3:]
            if text.endswith("```"):
                text = text[:-3]
            text = text.strip()

        return EvaluatorPatch.model_validate_json(text)
