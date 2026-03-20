"""前台面试官 Agent：只看黑板红绿灯 + probe_suggestion，生成自然语言追问"""

import json

from .llm import LLMClient
from .models import Blackboard


class ActorAgent:

    def __init__(self, client: LLMClient):
        self._client = client

    def act(self, board: Blackboard) -> str:
        dim_map = {d.node_id: d.category for d in board.rubric}

        # 按优先级分层：NEEDS_PROBING > FATAL_FLAW > GATHERING_SIGNALS > INIT
        focus_areas = {}
        for k, v in board.state_tree.items():
            if v.status in ("NEEDS_PROBING", "FATAL_FLAW", "GATHERING_SIGNALS", "INIT"):
                entry = {
                    "category": dim_map.get(k, "Unknown"),
                    "status": v.status,
                    "negative_signals": v.negative_signals,
                }
                if v.probe_suggestion:
                    entry["probe_suggestion"] = v.probe_suggestion
                focus_areas[k] = entry

        # 全部 SATISFIED → 总结收尾
        if not focus_areas:
            focus_areas = {
                k: {
                    "category": dim_map.get(k, "Unknown"),
                    "status": v.status,
                    "positive_signals": v.positive_signals,
                }
                for k, v in board.state_tree.items()
            }

        system_prompt = (
            f"你是一位 {board.interview_level} 级别的高级系统设计面试官。\n"
            f"当前正在面试：{board.topic}\n\n"
            f"【全局物理约束】: {json.dumps(board.global_constants, ensure_ascii=False)}\n\n"
            f"【后台考官给你的指示 — 当前焦点维度】:\n"
            f"{json.dumps(focus_areas, ensure_ascii=False, indent=2)}\n\n"
            "=== 发问策略 ===\n"
            "1. 最高优先级：如果有 NEEDS_PROBING 维度且附带 probe_suggestion，用它来追问\n"
            "2. 如果有 FATAL_FLAW，直接点出矛盾，要求候选人修正\n"
            "3. 如果有 GATHERING_SIGNALS，引导候选人深入该维度\n"
            "4. 如果有 INIT，在合适的时机自然引入该话题\n"
            "5. 全部 SATISFIED → 简短正面总结并结束\n\n"
            "不要啰嗦，不要暴露后台 JSON 或维度 ID，直接说人话。\n"
            "你的追问应该引导候选人展现思维能力，而非索要特定技术名词。"
        )

        # Actor 需要完整对话历史来维持上下文
        # 过滤掉 system message（已通过 system_prompt 注入）
        chat_messages = [m for m in board.history if m["role"] != "system"]

        # 开场时没有对话历史，用 chat() 单轮；否则用 chat_multi() 多轮
        if not chat_messages:
            return self._client.chat(
                system=system_prompt,
                user="请开场。",
            )

        return self._client.chat_multi(
            system=system_prompt,
            messages=chat_messages,
        )
