"""Entry point — wires the blackboard + three agents over a tars Pipeline.

Run:
    export TARS_PROVIDER=anthropic          # any provider in ~/.tars/config.toml
    export TARS_MODEL=claude-sonnet-4-5
    python -m interview_sim.main             # human candidate (you type)
    python -m interview_sim.main --auto       # self-play: AI candidate vs AI interviewer
    python -m interview_sim.main --auto --turns=8
"""

import json
import sys
from pathlib import Path

from pydantic import ValidationError

from .actor import ActorAgent
from .candidate import CandidateAgent
from .engine import run_auto, run_loop, save_log
from .evaluator import EvaluatorAgent
from .runtime import build_role
from .session_factory import SessionFactory, load_blueprint, load_rubric

BLUEPRINTS_DIR = Path(__file__).parent / "blueprints"


def _parse_args(argv: list[str]) -> tuple[bool, int, str | None]:
    auto_mode = "--auto" in argv
    max_turns = 5
    model: str | None = None
    for arg in argv:
        if arg.startswith("--turns="):
            raw = arg.split("=", 1)[1]
            try:
                max_turns = int(raw)
            except ValueError:
                print(f"错误：--turns 需要一个整数，收到 {raw!r}。", file=sys.stderr)
                sys.exit(2)
        elif arg.startswith("--model="):
            model = arg.split("=", 1)[1]
    return auto_mode, max_turns, model


def main():
    auto_mode, max_turns, model = _parse_args(sys.argv[1:])

    # 1. tars 角色（替代旧的 llm/ factory + 各家 client）
    try:
        role = build_role(model=model)
    except SystemExit:
        raise
    except Exception as e:
        print(
            f"错误：无法创建 tars Pipeline：{e}\n"
            "请先运行 `tars init` 并在 ~/.tars/config.toml 配置好 "
            "$TARS_PROVIDER，并设置好对应的 API key 环境变量。",
            file=sys.stderr,
        )
        sys.exit(1)

    # 2. 加载静态考纲 + 题目蓝图
    try:
        rubric = load_rubric()
    except (OSError, json.JSONDecodeError, ValidationError) as e:
        print(f"错误：加载考纲 rubric 失败：{e}", file=sys.stderr)
        sys.exit(1)

    blueprint_path = str(BLUEPRINTS_DIR / "ticketmaster.json")
    try:
        blueprint = load_blueprint(blueprint_path)
    except (OSError, json.JSONDecodeError, ValidationError) as e:
        print(
            f"错误：加载题目蓝图失败（{blueprint_path}）：{e}\n"
            "请确认该文件存在且为合法 JSON。",
            file=sys.stderr,
        )
        sys.exit(1)

    # 3. 创世：静态蓝图 + 考纲 → 动态初始黑板
    factory = SessionFactory(rubric)
    board = factory.create(blueprint)

    # 4. 注册 Agents（三者共用同一个 tars 角色；想给考官配更强的模型，
    #    可在这里 build_role(model=...) 单独构造一个评估角色）
    evaluator = EvaluatorAgent(role=role, target_dimensions=board.rubric)
    actor = ActorAgent(role=role)

    # 5. 启动（崩溃时仍落盘日志，便于事后排查）
    try:
        if auto_mode:
            candidate = CandidateAgent(role=role)
            run_auto(board, evaluator, actor, candidate, max_turns=max_turns)
        else:
            run_loop(board, evaluator, actor)
    except KeyboardInterrupt:
        print("\n已中断。正在保存日志...", file=sys.stderr)
        save_log(board)
        sys.exit(130)
    except Exception as e:
        print(f"运行过程中发生未处理异常：{e}\n正在保存日志...", file=sys.stderr)
        save_log(board)
        sys.exit(1)


if __name__ == "__main__":
    main()
