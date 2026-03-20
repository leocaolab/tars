import asyncio
from src.types import IEvaluator, IActor, Blackboard, Patch, ActionResponse
from src.engine import AgentEngine
from src.adapters.memory import LocalEventBus, MemoryStore


class CacheEvaluator(IEvaluator):
    @property
    def name(self) -> str:
        return "CacheChecker"

    async def evaluate(self, board: Blackboard) -> Patch:
        if board.state_tree.get("cache_layer") == "PASSED":
            return {}

        latest_msg = board.history[-1].content.lower() if board.history else ""

        if "redis" in latest_msg:
            print("✅ [Evaluator后台监控] 探测到正确架构设计，打上补丁。")
            return {"state_tree.cache_layer": "PASSED"}

        return {}


class InterviewerActor(IActor):
    async def act(self, board: Blackboard) -> ActionResponse:
        state = board.state_tree

        if state.get("cache_layer") == "PENDING":
            return ActionResponse(message="你的 QPS 这么高，数据库直接扛得住吗？")
        else:
            return ActionResponse(message="缓存设计得不错。那我们接下来聊聊高可用。")


async def main():
    store = MemoryStore()
    bus = LocalEventBus()

    await store.create("session_101", initial_state={"cache_layer": "PENDING"})

    engine = AgentEngine(
        store=store,
        bus=bus,
        evaluators=[CacheEvaluator()],
        actor=InterviewerActor(),
    )

    print("--- 第一回合 ---")
    await engine.push_input("session_101", "我直接用 MySQL 抗住 10万 并发。")
    await asyncio.sleep(0.1)
    board = await store.get("session_101")
    print(f"AI 回复: {board.history[-1].content}")

    print("\n--- 第二回合 ---")
    await engine.push_input("session_101", "哦对，前面得加个 Redis 集群。")
    await asyncio.sleep(0.1)
    board = await store.get("session_101")
    print(f"AI 回复: {board.history[-1].content}")


if __name__ == "__main__":
    asyncio.run(main())
