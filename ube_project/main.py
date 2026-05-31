import json
import sys
from pathlib import Path
from pydantic import ValidationError
from .config import settings
from .llm import create_client
from .session_factory import SessionFactory, load_rubric, load_blueprint
from .evaluator import EvaluatorAgent
from .actor import ActorAgent
from .candidate import CandidateAgent
from .engine import run_loop, run_auto, save_log

BLUEPRINTS_DIR = Path(__file__).parent / "blueprints"


def main():
    auto_mode = "--auto" in sys.argv
    max_turns = 5
    for arg in sys.argv:
        if arg.startswith("--turns="):
            raw = arg.split("=", 1)[1]
            try:
                max_turns = int(raw)
            except ValueError:
                print(f"错误：--turns 需要一个整数，收到 {raw!r}。", file=sys.stderr)
                sys.exit(2)

    # 1. 创建 LLM 客户端
    try:
        client = create_client(
            provider=settings.llm_provider,
            api_key=settings.llm_api_key.get_secret_value(),
            model=settings.llm_model,
        )
    except (ValueError, ImportError) as e:
        print(
            f"错误：无法创建 LLM 客户端（provider={settings.llm_provider!r}）：{e}\n"
            "请检查 llm_provider 配置并确认对应 SDK 已安装。",
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

    # 3. 创世
    factory = SessionFactory(rubric)
    board = factory.create(blueprint)

    # 4. 注册 Agents
    evaluator = EvaluatorAgent(client=client, target_dimensions=board.rubric)
    actor = ActorAgent(client=client)

    # 5. 启动（包裹异常处理：崩溃时仍落盘日志，便于事后排查）
    try:
        if auto_mode:
            candidate = CandidateAgent(client=client)
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
