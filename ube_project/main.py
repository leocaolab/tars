import sys
from pathlib import Path
from .config import settings
from .llm import create_client
from .session_factory import SessionFactory, load_rubric, load_blueprint
from .evaluator import EvaluatorAgent
from .actor import ActorAgent
from .candidate import CandidateAgent
from .engine import run_loop, run_auto

BLUEPRINTS_DIR = Path(__file__).parent / "blueprints"


def main():
    auto_mode = "--auto" in sys.argv
    max_turns = 5
    for arg in sys.argv:
        if arg.startswith("--turns="):
            max_turns = int(arg.split("=", 1)[1])

    # 1. 创建 LLM 客户端
    client = create_client(
        provider=settings.llm_provider,
        api_key=settings.llm_api_key,
        model=settings.llm_model,
    )

    # 2. 加载静态考纲 + 题目蓝图
    rubric = load_rubric()
    blueprint = load_blueprint(str(BLUEPRINTS_DIR / "ticketmaster.json"))

    # 3. 创世
    factory = SessionFactory(rubric)
    board = factory.create(blueprint)

    # 4. 注册 Agents
    evaluator = EvaluatorAgent(client=client, target_dimensions=board.rubric)
    actor = ActorAgent(client=client)

    # 5. 启动
    if auto_mode:
        candidate = CandidateAgent(client=client)
        run_auto(board, evaluator, actor, candidate, max_turns=max_turns)
    else:
        run_loop(board, evaluator, actor)


if __name__ == "__main__":
    main()
