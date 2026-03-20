import json
from rich.console import Console
from rich.panel import Panel
from .models import Blackboard, EvaluatorPatch
from .evaluator import EvaluatorAgent
from .actor import ActorAgent
from .candidate import CandidateAgent

console = Console()

STATUS_STYLE = {
    "INIT": "dim white",
    "GATHERING_SIGNALS": "bold cyan",
    "SATISFIED": "bold green",
    "NEEDS_PROBING": "bold yellow",
    "FATAL_FLAW": "bold red",
}

STATUS_ICON = {
    "INIT": "  ",
    "GATHERING_SIGNALS": ".. ",
    "SATISFIED": "+  ",
    "NEEDS_PROBING": "?  ",
    "FATAL_FLAW": "X  ",
}

# 状态优先级：越高越紧急
_PRIORITY = {
    "FATAL_FLAW": 4,
    "NEEDS_PROBING": 3,
    "GATHERING_SIGNALS": 2,
    "INIT": 1,
    "SATISFIED": 0,
}


def render_blackboard(board: Blackboard) -> None:
    """用 rich 渲染累积记分牌"""
    dim_map = {d.node_id: d.category for d in board.rubric}

    lines: list[str] = []
    for node_id, node in board.state_tree.items():
        style = STATUS_STYLE.get(node.status, "white")
        icon = STATUS_ICON.get(node.status, "")
        category = dim_map.get(node_id, "-")
        header = f"[{style}]{icon}{node.status}[/]  [{style}]{category}[/] / [cyan]{node_id}[/]"

        parts = [header]
        if node.positive_signals:
            joined = "; ".join(node.positive_signals)
            parts.append(f"  [green]+({len(node.positive_signals)})[/] {joined}")
        if node.negative_signals:
            joined = "; ".join(node.negative_signals)
            parts.append(f"  [red]-({len(node.negative_signals)})[/] {joined}")
        if node.probe_suggestion:
            parts.append(f"  [yellow]Probe:[/] {node.probe_suggestion}")
        lines.append("\n".join(parts))

    console.print(Panel(
        "\n".join(lines),
        title="Signal Radar — Accumulated Scoreboard",
        border_style="bright_blue",
    ))


def apply_patch(board: Blackboard, patch: EvaluatorPatch) -> None:
    """将 Evaluator 的 Patch 合并到黑板"""
    for node_id, new_status in patch.updates.items():
        if node_id in board.state_tree:
            board.state_tree[node_id].status = new_status
    for node_id, signal in patch.new_positive_signals.items():
        if node_id in board.state_tree:
            board.state_tree[node_id].positive_signals.append(signal)
    for node_id, signal in patch.new_negative_signals.items():
        if node_id in board.state_tree:
            board.state_tree[node_id].negative_signals.append(signal)
    for node_id, suggestion in patch.probe_suggestions.items():
        if node_id in board.state_tree:
            board.state_tree[node_id].probe_suggestion = suggestion


def extract_directive(board: Blackboard) -> str:
    """
    引擎桥接层：从黑板中提取最紧急的一条导演指令。
    剥离所有 Rubric 标签，只输出纯文本指示给 Actor。
    """
    # 全部 SATISFIED → 收尾
    all_satisfied = all(
        node.status == "SATISFIED" for node in board.state_tree.values()
    )
    if all_satisfied:
        return "所有考核维度已通过。用一句话给候选人正面总结，结束面试。"

    # 按优先级排序，取最紧急的有 probe_suggestion 的节点
    candidates = [
        (node_id, node)
        for node_id, node in board.state_tree.items()
        if node.status not in ("SATISFIED",) and node.probe_suggestion
    ]
    candidates.sort(key=lambda x: _PRIORITY.get(x[1].status, 0), reverse=True)

    if candidates:
        _, top_node = candidates[0]
        return top_node.probe_suggestion

    # 没有 probe_suggestion，但有未完成的维度 → 通用指令
    return "继续倾听。如果候选人停顿或跑偏，用一句话把他拉回正轨。"


def _get_chat_history(board: Blackboard) -> list[dict[str, str]]:
    """提取干净的对话历史（过滤 system message）"""
    return [m for m in board.history if m["role"] != "system"]


def _invoke_actor(board: Blackboard, actor: ActorAgent, directive: str) -> str:
    """调用 Actor 并渲染导演指令"""
    console.print(
        f"[dim][ Engine ] 导演指令 → [/][italic yellow]\"{directive}\"[/]"
    )
    history = _get_chat_history(board)
    if history:
        return actor.act_with_history(directive, history)
    return actor.act(directive)


def run_loop(
    board: Blackboard,
    evaluator: EvaluatorAgent,
    actor: ActorAgent,
) -> None:
    """核心事件循环（人类候选人模式）"""
    console.print(
        Panel(
            f"[bold magenta]{board.topic}[/]  |  Level: [bold]{board.interview_level}[/]\n"
            f"物理约束: {json.dumps(board.global_constants, ensure_ascii=False)}",
            title="=== UBE Interview Session ===",
            border_style="bright_blue",
        )
    )
    render_blackboard(board)

    # 开场白
    console.print("\n[dim][ Engine ] 唤醒前台面试官...[/]")
    directive = extract_directive(board)
    greeting = _invoke_actor(board, actor, directive)
    board.history.append({"role": "assistant", "content": greeting})
    console.print(Panel(greeting, title="[bold blue]面试官[/]", border_style="blue"))

    while True:
        console.print()
        user_input = console.input("[bold green]候选人 (你): [/]")
        if user_input.strip().lower() in ("quit", "exit", "q"):
            console.print("[dim]面试结束。[/]")
            break
        _run_turn(board, evaluator, actor, user_input)


def _run_turn(
    board: Blackboard,
    evaluator: EvaluatorAgent,
    actor: ActorAgent,
    user_input: str,
) -> None:
    """一个完整回合：候选人发言 → 考官评估 → 引擎提取指令 → Actor 发问"""
    board.history.append({"role": "user", "content": user_input})

    # 阶段 A：考官静默评估
    console.print("\n[dim][ Engine ] 唤醒后台考官评估中...[/]")
    patch = evaluator.evaluate(board, user_input)

    patch_details = []
    if patch.updates:
        patch_details.append(f"[bold]status updates:[/] {json.dumps(patch.updates, ensure_ascii=False)}")
    if patch.new_positive_signals:
        patch_details.append(f"[bold green]+signals:[/] {json.dumps(patch.new_positive_signals, ensure_ascii=False)}")
    if patch.new_negative_signals:
        patch_details.append(f"[bold red]-signals:[/] {json.dumps(patch.new_negative_signals, ensure_ascii=False)}")
    if patch.probe_suggestions:
        patch_details.append(f"[bold yellow]probes:[/] {json.dumps(patch.probe_suggestions, ensure_ascii=False)}")

    console.print(
        Panel(
            f"[italic]{patch.internal_thought}[/]\n\n" + "\n".join(patch_details),
            title="[bold red]考官内审 (CoT + Patch)[/]",
            border_style="red",
        )
    )

    apply_patch(board, patch)
    render_blackboard(board)

    # 阶段 B：引擎提取指令 → Actor 发声
    console.print("[dim][ Engine ] 唤醒前台面试官...[/]")
    directive = extract_directive(board)
    response = _invoke_actor(board, actor, directive)
    board.history.append({"role": "assistant", "content": response})
    console.print(
        Panel(response, title="[bold blue]面试官[/]", border_style="blue")
    )


def run_auto(
    board: Blackboard,
    evaluator: EvaluatorAgent,
    actor: ActorAgent,
    candidate: CandidateAgent,
    max_turns: int = 5,
) -> None:
    """Self-Play 自动对抗：AI 面试官 vs AI 候选人"""
    console.print(
        Panel(
            f"[bold magenta]{board.topic}[/]  |  Level: [bold]{board.interview_level}[/]\n"
            f"物理约束: {json.dumps(board.global_constants, ensure_ascii=False)}\n"
            f"对抗轮数: [bold]{max_turns}[/]",
            title="=== UBE Self-Play Arena ===",
            border_style="bright_red",
        )
    )
    render_blackboard(board)

    # 开场白
    console.print("\n[dim][ Engine ] 唤醒前台面试官...[/]")
    directive = extract_directive(board)
    greeting = _invoke_actor(board, actor, directive)
    board.history.append({"role": "assistant", "content": greeting})
    console.print(Panel(greeting, title="[bold blue]面试官[/]", border_style="blue"))

    # Self-Play 循环
    for turn in range(1, max_turns + 1):
        console.print(f"\n[bold]{'='*50}[/]")
        console.print(f"[bold bright_red]  Round {turn}/{max_turns}[/]")
        console.print(f"[bold]{'='*50}[/]")

        console.print("\n[dim][ Engine ] AI 候选人思考中...[/]")
        answer = candidate.answer(board)
        console.print(
            Panel(answer, title="[bold green]AI 候选人[/]", border_style="green")
        )

        _run_turn(board, evaluator, actor, answer)

    # 结束总结
    console.print(f"\n[bold]{'='*50}[/]")
    console.print("[bold bright_red]  Self-Play Complete[/]")
    console.print(f"[bold]{'='*50}[/]\n")
    render_blackboard(board)
