"""CLI rendering layer — Rich terminal UI via framework event callbacks."""

import json
import time
from pathlib import Path
from rich.console import Console
from rich.markdown import Markdown
from rich.panel import Panel

from ube_core import Blackboard
from ube_core.engine import AgentEngine
from .candidate import CandidateAgent

console = Console(record=True)

STATUS_STYLE = {
    "INIT": "dim white", "GATHERING_SIGNALS": "bold cyan",
    "SATISFIED": "bold green", "NEEDS_PROBING": "bold yellow", "FATAL_FLAW": "bold red",
}
STATUS_ICON = {
    "INIT": "  ", "GATHERING_SIGNALS": ".. ", "SATISFIED": "+  ",
    "NEEDS_PROBING": "?  ", "FATAL_FLAW": "X  ",
}


def render_blackboard(board: Blackboard) -> None:
    rubric = board.context.get("rubric", [])
    dim_map = {d["node_id"]: d["category"] for d in rubric}
    lines: list[str] = []
    for node_id, node in board.state_tree.items():
        status = node.get("status", "INIT")
        style = STATUS_STYLE.get(status, "white")
        icon = STATUS_ICON.get(status, "")
        category = dim_map.get(node_id, "-")
        header = f"[{style}]{icon}{status}[/]  [{style}]{category}[/] / [cyan]{node_id}[/]"
        parts = [header]
        pos = node.get("positive_signals", [])
        neg = node.get("negative_signals", [])
        probe = node.get("probe_suggestion")
        if pos:
            parts.append(f"  [green]+({len(pos)})[/] {'; '.join(pos)}")
        if neg:
            parts.append(f"  [red]-({len(neg)})[/] {'; '.join(neg)}")
        if probe:
            parts.append(f"  [yellow]Probe:[/] {probe}")
        lines.append("\n".join(parts))
    console.print(Panel("\n".join(lines), title="Signal Radar", border_style="bright_blue"))


def on_event(event: str, board: Blackboard, extra: dict) -> None:
    if event == "EVALUATING_START":
        console.print("\n[dim][ Engine ] Evaluator running...[/]")
    elif event == "EVALUATING_DONE":
        for patch in extra.get("patches", []):
            ev = patch.updates.get("_evaluator_patch", {})
            if not ev:
                continue
            details = []
            thought = ev.get("internal_thought", "")
            if ev.get("updates"):
                details.append(f"[bold]status:[/] {json.dumps(ev['updates'], ensure_ascii=False)}")
            if ev.get("new_positive_signals"):
                details.append(f"[bold green]+signals:[/] {json.dumps(ev['new_positive_signals'], ensure_ascii=False)}")
            if ev.get("new_negative_signals"):
                details.append(f"[bold red]-signals:[/] {json.dumps(ev['new_negative_signals'], ensure_ascii=False)}")
            if ev.get("probe_suggestions"):
                details.append(f"[bold yellow]probes:[/] {json.dumps(ev['probe_suggestions'], ensure_ascii=False)}")
            console.print(Panel(
                f"[italic]{thought}[/]\n\n" + "\n".join(details),
                title="[bold red]Evaluator Internal (CoT + Patch)[/]", border_style="red",
            ))
        render_blackboard(board)
    elif event == "ACTING_START":
        directive = extra.get("directive", "")
        console.print(f'[dim][ Engine ] Directive -> [/][italic yellow]"{directive}"[/]')
    elif event == "ACTING_DONE":
        reply = extra.get("reply", "")
        console.print(Panel(reply, title="[bold blue]Interviewer[/]", border_style="blue"))


def _generate_report(engine: AgentEngine, board: Blackboard) -> None:
    from .reporter import InterviewReporter
    client = engine.actor._client if hasattr(engine.actor, "_client") else None
    if not client:
        return
    console.print("\n[dim][ Engine ] Generating interview report...[/]")
    reporter = InterviewReporter(client)
    report = reporter.generate_report(board)
    console.print(Panel(Markdown(report), title="[bold]Hire Packet[/]", border_style="bright_magenta"))
    log_dir = Path("logs")
    log_dir.mkdir(exist_ok=True)
    ts = time.strftime("%Y%m%d_%H%M%S")
    report_file = log_dir / f"report_{board.session_id}_{ts}.md"
    report_file.write_text(report, encoding="utf-8")
    console.print(f"[dim][ Engine ] Report -> {report_file}[/]")


def save_log(board: Blackboard, log_dir: str = "logs") -> str:
    path = Path(log_dir)
    path.mkdir(exist_ok=True)
    ts = time.strftime("%Y%m%d_%H%M%S")
    log_file = path / f"selfplay_{board.session_id}_{ts}.txt"
    json_file = path / f"blackboard_{board.session_id}_{ts}.json"
    log_file.write_text(console.export_text(), encoding="utf-8")
    json_file.write_text(json.dumps(board.model_dump(), indent=2, ensure_ascii=False), encoding="utf-8")
    console.print(f"\n[dim][ Engine ] Log -> {log_file}[/]")
    console.print(f"[dim][ Engine ] Snapshot -> {json_file}[/]")
    return str(log_file)


def _get_persona_label(engine: AgentEngine) -> str:
    actor = engine.actor
    if hasattr(actor, "persona"):
        return f"{actor.persona['name']}"
    return "default"


def run_interactive(engine: AgentEngine, board: Blackboard) -> None:
    ctx = board.context
    console.print(Panel(
        f"[bold magenta]{ctx.get('topic', '')}[/]  |  Level: [bold]{ctx.get('interview_level', '')}[/]\n"
        f"Constraints: {json.dumps(ctx.get('global_constants', {}), ensure_ascii=False)}",
        title="=== UBE Interview Session ===", border_style="bright_blue",
    ))
    render_blackboard(board)
    engine.generate_greeting(board)
    while True:
        console.print()
        user_input = console.input("[bold green]Candidate (you): [/]")
        if user_input.strip().lower() in ("quit", "exit", "q"):
            console.print("[dim]Interview ended.[/]")
            break
        _, terminated = engine.push_input(board, user_input)
        if terminated:
            console.print("\n[bold bright_red][ Engine ] Termination triggered — interview auto-ended[/]")
            break
    _generate_report(engine, board)
    save_log(board)


def run_selfplay(
    engine: AgentEngine,
    board: Blackboard,
    candidate: CandidateAgent,
    max_turns: int = 5,
) -> None:
    ctx = board.context
    persona_label = _get_persona_label(engine)
    console.print(Panel(
        f"[bold magenta]{ctx.get('topic', '')}[/]  |  Level: [bold]{ctx.get('interview_level', '')}[/]\n"
        f"Constraints: {json.dumps(ctx.get('global_constants', {}), ensure_ascii=False)}\n"
        f"Interviewer: [bold]{persona_label}[/]  |  "
        f"Candidate: [bold]{candidate.persona['level']}[/] ({candidate.persona_key})  |  "
        f"Rounds: [bold]{max_turns}[/]",
        title="=== UBE Self-Play Arena ===", border_style="bright_red",
    ))
    render_blackboard(board)
    engine.generate_greeting(board)
    for turn in range(1, max_turns + 1):
        console.print(f"\n[bold]{'='*50}[/]")
        console.print(f"[bold bright_red]  Round {turn}/{max_turns}[/]")
        console.print(f"[bold]{'='*50}[/]")
        console.print("\n[dim][ Engine ] AI Candidate thinking...[/]")
        answer = candidate.answer(board)
        console.print(Panel(answer, title="[bold green]AI Candidate[/]", border_style="green"))
        _, terminated = engine.push_input(board, answer)
        if terminated:
            console.print(f"\n[bold bright_red][ Engine ] Termination triggered — interview ended at Round {turn}[/]")
            break
    console.print(f"\n[bold]{'='*50}[/]")
    console.print("[bold bright_red]  Self-Play Complete[/]")
    console.print(f"[bold]{'='*50}[/]\n")
    render_blackboard(board)
    _generate_report(engine, board)
    save_log(board)
