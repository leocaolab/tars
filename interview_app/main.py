"""面试应用入口 — 组装框架引擎 + 业务组件。"""

import sys
from pathlib import Path

from ube_core.engine import AgentEngine
from ube_core.router import DirectiveRouter
from ube_core.llm import create_client
from .config import settings
from .session_factory import SessionFactory, SessionFactoryV2, load_rubric, load_blueprint
from .evaluator import InterviewEvaluator
from .actor import InterviewActor
from .candidate import CandidateAgent
from .bridge import merge_patch, InterviewScorer, InterviewTerminationChecker
from .cli import on_event, run_interactive, run_selfplay
from .models import RubricDimension

BLUEPRINTS_DIR = Path(__file__).parent / "blueprints"


def main():
    auto_mode = "--auto" in sys.argv
    max_turns = 5
    persona = None
    candidate_persona = None
    blueprint_name = "ticketmaster"
    for arg in sys.argv:
        if arg.startswith("--turns="):
            max_turns = int(arg.split("=", 1)[1])
        elif arg.startswith("--persona="):
            persona = arg.split("=", 1)[1]
        elif arg.startswith("--candidate="):
            candidate_persona = arg.split("=", 1)[1]
        elif arg.startswith("--blueprint="):
            blueprint_name = arg.split("=", 1)[1]

    v2_mode = "--v2" in sys.argv

    client = create_client(
        provider=settings.llm_provider,
        api_key=settings.llm_api_key,
        model=settings.llm_model,
    )

    bp_path = str(BLUEPRINTS_DIR / f"{blueprint_name}.json")

    if v2_mode:
        # V2: three-layer rubric (universal + domain + inline custom)
        factory_v2 = SessionFactoryV2()
        board = factory_v2.create(bp_path)
    else:
        # Legacy: single rubric.json
        rubric = load_rubric()
        blueprint = load_blueprint(bp_path)
        factory = SessionFactory(rubric)
        board = factory.create(blueprint)

    # 4. 从 context 中恢复 RubricDimension 列表（给 Evaluator 用）
    dims = [RubricDimension(**d) for d in board.context["rubric"]]

    # 5. 组装框架引擎 — 注入业务组件
    engine = AgentEngine(
        evaluators=[InterviewEvaluator(client=client, target_dimensions=dims)],
        actor=(actor := InterviewActor(client=client, persona=persona)),
        directive_extractor=DirectiveRouter(
            scorer=InterviewScorer(),
            max_depth_turns=2,
            all_terminal_message="所有考核维度已评估完毕。用一句话给候选人总结，结束面试。",
            timeout_message="在当前话题上的深挖已经足够了。请用一句话优雅地收束，然后转移到下一个关键问题。",
            fallback_message="继续倾听。如果候选人停顿或跑偏，用一句话把他拉回正轨。",
        ),
        merge_patch=merge_patch,
        termination_checker=InterviewTerminationChecker(),
        on_event=on_event,
    )

    # 6. 启动
    if auto_mode:
        candidate = CandidateAgent(client=client, persona=candidate_persona)
        run_selfplay(engine, board, candidate, max_turns=max_turns)
    else:
        run_interactive(engine, board)


if __name__ == "__main__":
    main()
