"""Generic patch merge utilities for blackboard state_tree."""

from .types import Blackboard, Patch


def resolve_node_id(key: str, valid_ids: set[str]) -> str | None:
    """Fuzzy-match an LLM-returned key to a valid node_id.

    Tries exact match first, then substring matching on the tail segment.
    """
    if key in valid_ids:
        return key
    key_lower = key.lower().replace("_", ".").replace("-", ".")
    for nid in valid_ids:
        tail = nid.split(".")[-1]
        if tail in key_lower or key_lower in nid:
            return nid
    return None


def merge_evaluator_patch(
    board: Blackboard,
    patch: Patch,
    patch_key: str = "_evaluator_patch",
    positive_field: str = "positive_signals",
    negative_field: str = "negative_signals",
    probe_field: str = "probe_suggestion",
    urgency_field: str = "probe_urgency",
) -> None:
    """Generic merge: unpack a structured evaluator patch into board.state_tree.

    Expected patch structure inside patch.updates[patch_key]:
      - updates: Dict[node_id, new_status]
      - new_positive_signals: Dict[node_id, str|List[str]]
      - new_negative_signals: Dict[node_id, str|List[str]]
      - probe_suggestions: Dict[node_id, str|{question, urgency}]
    """
    ev_patch = patch.updates.get(patch_key)
    if not ev_patch:
        return

    valid_ids = set(board.state_tree.keys())

    # Status updates
    for key, new_status in ev_patch.get("updates", {}).items():
        nid = resolve_node_id(key, valid_ids)
        if nid:
            board.state_tree[nid]["status"] = new_status

    # Positive signals (str or List[str], always extend)
    for key, signal in ev_patch.get("new_positive_signals", {}).items():
        nid = resolve_node_id(key, valid_ids)
        if nid:
            signals = signal if isinstance(signal, list) else [signal]
            board.state_tree[nid].setdefault(positive_field, []).extend(signals)

    # Negative signals
    for key, signal in ev_patch.get("new_negative_signals", {}).items():
        nid = resolve_node_id(key, valid_ids)
        if nid:
            signals = signal if isinstance(signal, list) else [signal]
            board.state_tree[nid].setdefault(negative_field, []).extend(signals)

    # Probe suggestions (str or {question, urgency})
    for key, suggestion in ev_patch.get("probe_suggestions", {}).items():
        nid = resolve_node_id(key, valid_ids)
        if nid:
            if isinstance(suggestion, dict):
                board.state_tree[nid][probe_field] = suggestion.get("question", "")
                board.state_tree[nid][urgency_field] = suggestion.get("urgency", 3)
            else:
                board.state_tree[nid][probe_field] = suggestion
                board.state_tree[nid][urgency_field] = 3
