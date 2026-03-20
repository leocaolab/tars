"""面试应用入口 — 组装框架引擎 + 业务组件。"""

import sys
from pathlib import Path

from ube_core.engine import AgentEngine
from ube_core.llm import create_client
from .config import settings
from .session_factory import SessionFactory, load_rubric, load_blueprint
from .evaluator import InterviewEvaluator
from .actor import InterviewActor
from .candidate import CandidateAgent
from .bridge import merge_patch, InterviewDirectiveExtractor
from .cli import on_event, run_interactive, run_selfplay
from .models import RubricDimension

BLUEPRINTS_DIR = Path(__file__).parent / "blueprints"


def main():
    auto_mode = "--auto" in sys.argv
    max_turns = 5
    persona = None
    for arg in sys.argv:
        if arg.startswith("--turns="):
            max_turns = int(arg.split("=", 1)[1])
        elif arg.startswith("--persona="):
            persona = arg.split("=", 1)[1]

    # 1. LLM 客户端
    client = create_client(
        provider=settings.llm_provider,
        api_key=settings.llm_api_key,
        model=settings.llm_model,
    )

    # 2. 加载静态配置
    rubric = load_rubric()
    blueprint = load_blueprint(str(BLUEPRINTS_DIR / "ticketmaster.json"))

    # 3. 创世
    factory = SessionFactory(rubric)
    board = factory.create(blueprint)

    # 4. 从 context 中恢复 RubricDimension 列表（给 Evaluator 用）
    dims = [RubricDimension(**d) for d in board.context["rubric"]]

    # 5. 组装框架引擎 — 注入业务组件
    engine = AgentEngine(
        evaluators=[InterviewEvaluator(client=client, target_dimensions=dims)],
        actor=(actor := InterviewActor(client=client, persona=persona)),
        directive_extractor=InterviewDirectiveExtractor(),
        merge_patch=merge_patch,
        on_event=on_event,
    )

    # 6. 启动
    if auto_mode:
        candidate = CandidateAgent(client=client)
        run_selfplay(engine, board, candidate, max_turns=max_turns)
    else:
        run_interactive(engine, board)


if __name__ == "__main__":
    main()
