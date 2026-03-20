"""面试考官 — 实现框架的 IEvaluator 接口。"""

import json
from typing import List

from ube_core import Blackboard, Patch, IEvaluator
from ube_core.llm import LLMClient
from .models import EvaluatorPatch, RubricDimension


class InterviewEvaluator(IEvaluator):

    def __init__(self, client: LLMClient, target_dimensions: List[RubricDimension]):
        self._client = client
        self.target_dimensions = target_dimensions

    def evaluate(self, board: Blackboard, user_input: str) -> Patch:
        # 从泛型 state_tree 中读取面试专属状态
        target_ids = [d.node_id for d in self.target_dimensions]
        slice_state = {
            k: v for k, v in board.state_tree.items() if k in target_ids
        }

        ctx = board.context
        rules_block = "\n".join(
            f"  - [{d.category}] {d.node_id}: {d.eval_rule}"
            for d in self.target_dimensions
        )

        schema_hint = json.dumps(
            EvaluatorPatch.model_json_schema(), ensure_ascii=False, indent=2
        )

        system_prompt = (
            "你是一位严苛的系统设计考官。你只输出 JSON，绝不输出任何其他内容。\n"
            f"请严格按照以下 JSON Schema 返回：\n{schema_hint}"
        )

        user_prompt = (
            f"当前面试级别：{ctx.get('interview_level', '')}\n"
            f"当前题目：{ctx.get('topic', '')}\n\n"
            f"【物理约束】: {json.dumps(ctx.get('global_constants', {}), ensure_ascii=False)}\n\n"
            f"【你负责监控的能力维度及评判规则】:\n{rules_block}\n\n"
            f"【各维度当前运行时状态（累积记分牌）】:\n"
            f"{json.dumps(slice_state, ensure_ascii=False, indent=2)}\n\n"
            f"【候选人最新发言】: \"{user_input}\"\n\n"
            "=== 状态机规则 ===\n"
            "- INIT: 该维度尚未被候选人触及\n"
            "- GATHERING_SIGNALS: 候选人开始涉及但证据不足以判定\n"
            "- SATISFIED: 该维度已充分展现所需能力\n"
            "- NEEDS_PROBING: 存在疑点或盲点，需要前台追问\n"
            "- FATAL_FLAW: 出现致命逻辑漏洞\n\n"
            "=== 输出要求 ===\n"
            "- internal_thought: 逐维度分析候选人此轮发言\n"
            "- updates: 只包含状态确实需要变化的 node_id → 新状态\n"
            "- new_positive_signals: 正面证据\n"
            "- new_negative_signals: 负面证据\n"
            "- probe_suggestions: NEEDS_PROBING 时的追问建议\n\n"
            "只返回纯 JSON，不要 markdown 代码块。"
        )

        # 重试最多 3 次 — LLM 偶尔返回非法 JSON
        last_err = None
        for attempt in range(3):
            raw = self._client.chat(system=system_prompt, user=user_prompt)

            text = raw.strip()
            if text.startswith("```"):
                text = text.split("\n", 1)[1] if "\n" in text else text[3:]
                if text.endswith("```"):
                    text = text[:-3]
                text = text.strip()

            # 尝试提取第一个 { 到最后一个 } 之间的内容
            start = text.find("{")
            end = text.rfind("}")
            if start != -1 and end != -1:
                text = text[start:end + 1]

            try:
                patch_data = EvaluatorPatch.model_validate_json(text)
                return Patch(updates={"_evaluator_patch": patch_data.model_dump()})
            except Exception as e:
                last_err = e

        # 全部失败 — 返回空补丁，不崩溃
        import sys
        print(f"[Evaluator] JSON 解析失败 (3 次重试): {last_err}", file=sys.stderr)
        return Patch(updates={})
