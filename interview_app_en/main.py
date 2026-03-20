"""Interview app entry point — assembles framework engine + business components."""

import sys
from pathlib import Path

from ube_core.engine import AgentEngine
from ube_core.router import DirectiveRouter
from ube_core.llm import create_client
from .config import settings
from .session_factory import SessionFactory, load_rubric, load_blueprint
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

    client = create_client(
        provider=settings.llm_provider,
        api_key=settings.llm_api_key,
        model=settings.llm_model,
    )

    rubric = load_rubric()
    blueprint = load_blueprint(str(BLUEPRINTS_DIR / f"{blueprint_name}.json"))

    factory = SessionFactory(rubric)
    board = factory.create(blueprint)

    dims = [RubricDimension(**d) for d in board.context["rubric"]]

    engine = AgentEngine(
        evaluators=[InterviewEvaluator(client=client, target_dimensions=dims)],
        actor=(actor := InterviewActor(client=client, persona=persona)),
        directive_extractor=DirectiveRouter(
            scorer=InterviewScorer(),
            max_depth_turns=2,
        ),
        merge_patch=merge_patch,
        termination_checker=InterviewTerminationChecker(),
        on_event=on_event,
    )

    if auto_mode:
        candidate = CandidateAgent(client=client, persona=candidate_persona)
        run_selfplay(engine, board, candidate, max_turns=max_turns)
    else:
        run_interactive(engine, board)


if __name__ == "__main__":
    main()
