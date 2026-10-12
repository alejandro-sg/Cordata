"""Research brief and deterministic grounding for the orchestrator.

The research role investigates the codebase before planning begins, producing a structured
brief (findings, constraints, risks, approach, open questions). Findings must be grounded in
verifiable evidence:
- kind="file": 'path:line' with a short verbatim quote within ±3 lines
- kind="doc": ref must name an existing project file
- kind="command": verifiable only when doc refs name an existing project file

A brief is verified deterministically without model calls. Findings with no verifiable
evidence are dropped. If fewer than half of findings survive (or none exist), the brief
is refused and a retry prompt is generated.
"""

from __future__ import annotations

import copy
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any

#: Schema for a research brief produced by a worker.
RESEARCH_SCHEMA: dict[str, Any] = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "title": "ResearchBrief",
    "type": "object",
    "required": ["findings"],
    "properties": {
        "findings": {
            "type": "array",
            "items": {
                "type": "object",
                "required": ["id", "claim", "evidence"],
                "properties": {
                    "id": {"type": "string"},
                    "claim": {"type": "string"},
                    "evidence": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["kind", "ref"],
                            "properties": {
                                "kind": {
                                    "type": "string",
                                    "enum": ["file", "doc", "command"],
                                },
                                "ref": {"type": "string"},
                                "quote": {"type": "string"},
                            },
                        },
                    },
                },
            },
        },
        "constraints": {
            "type": "array",
            "items": {"type": "string"},
        },
        "risks": {
            "type": "array",
            "items": {"type": "string"},
        },
        "approach": {
            "type": "string",
        },
        "open_questions": {
            "type": "array",
            "items": {"type": "string"},
        },
    },
}

#: Prompt instructions for the research worker.
RESEARCH_BRIEF: str = (
    "You are a research worker exploring the repository.\n"
    "Your role is read-only: do not edit or create files.\n"
    "All findings must be grounded in evidence.\n"
    "Cite file evidence as 'path:line' with a short verbatim quote from that location.\n"
    "Doc evidence must cite an existing project file path.\n"
    "Return a structured brief conforming to the schema with findings, constraints, "
    "risks, approach, and open_questions."
)


@dataclass
class Verified:
    """The result of verifying a research brief against a repository root."""

    brief: dict[str, Any]
    dropped: list[str]
    refused: bool
    reasons: list[str]

    def __iter__(self):
        yield self.brief
        yield self.dropped
        yield self.refused
        yield self.reasons


def _is_forbidden_path(rel_path: str) -> bool:
    """Check if any path component is .env or .git (or variant)."""
    clean = rel_path.replace("\\", "/").strip("/")
    parts = [p for p in clean.split("/") if p and p != "."]
    for p in parts:
        if p == ".git":
            return True
        if p == ".env":
            return True
        # .env.* files are forbidden except standard committed template files
        if p.startswith(".env.") and not p.endswith((".example", ".sample", ".template", ".dist")):
            return True
        if p.startswith(".env") and not p.endswith((".example", ".sample", ".template", ".dist")):
            return True
    return False


def _check_doc_ref(ref: str, root_real: str) -> tuple[bool, str]:
    """Verify that a doc reference points to an existing file in the project."""
    if not isinstance(ref, str) or not ref.strip():
        return False, "doc ref is empty"
    clean_ref = ref.strip()

    # Determine candidate file path
    target = os.path.join(root_real, clean_ref) if not os.path.isabs(clean_ref) else clean_ref
    if not os.path.exists(target) and ":" in clean_ref:
        path_candidate, _, _ = clean_ref.rpartition(":")
        if path_candidate:
            target_candidate = os.path.join(root_real, path_candidate) if not os.path.isabs(path_candidate) else path_candidate
            if os.path.exists(target_candidate):
                target = target_candidate
                clean_ref = path_candidate

    resolved = os.path.realpath(target)

    try:
        if os.path.commonpath([root_real, resolved]) != root_real:
            return False, f"doc ref {ref!r} escapes root directory"
    except ValueError:
        return False, f"doc ref {ref!r} escapes root directory"

    if resolved == root_real:
        return False, f"doc ref {ref!r} is root directory"

    rel = os.path.relpath(resolved, root_real)
    if _is_forbidden_path(rel) or _is_forbidden_path(clean_ref):
        return False, f"doc ref {ref!r} references forbidden path (.env or .git)"

    if not os.path.exists(resolved):
        return False, f"doc ref {ref!r} does not exist"
    if not os.path.isfile(resolved):
        return False, f"doc ref {ref!r} is not a file"

    return True, ""


def _verify_file_evidence(ev: dict[str, Any], root_real: str) -> tuple[bool, str]:
    """Verify kind=file evidence: path:line, root-contained, not .env/.git, line in range, quote ±3."""
    ref = ev.get("ref")
    if not isinstance(ref, str) or ":" not in ref:
        return False, f"file ref {ref!r} must parse as 'path:line'"

    path_part, sep, line_part = ref.rpartition(":")
    path_part = path_part.strip()
    line_part = line_part.strip()
    if not path_part or not line_part:
        return False, f"file ref {ref!r} must parse as 'path:line'"

    try:
        line_num = int(line_part)
    except ValueError:
        return False, f"file ref {ref!r} line {line_part!r} is not an integer"

    if line_num < 1:
        return False, f"file ref {ref!r} line {line_num} must be >= 1"

    target = os.path.join(root_real, path_part) if not os.path.isabs(path_part) else path_part
    resolved = os.path.realpath(target)

    # Path must resolve inside realpath of root and not be a symlink escape
    try:
        if os.path.commonpath([root_real, resolved]) != root_real:
            return False, f"path {path_part!r} escapes root directory"
    except ValueError:
        return False, f"path {path_part!r} escapes root directory"

    if resolved == root_real:
        return False, f"path {path_part!r} is root directory"

    # Must not be .env or .git
    rel = os.path.relpath(resolved, root_real)
    if _is_forbidden_path(rel) or _is_forbidden_path(path_part):
        return False, f"path {path_part!r} references forbidden path (.env or .git)"

    if not os.path.exists(resolved):
        return False, f"file {path_part!r} does not exist"
    if not os.path.isfile(resolved):
        return False, f"path {path_part!r} is not a regular file"

    try:
        with open(resolved, "r", encoding="utf-8", errors="replace") as f:
            content = f.read()
    except Exception as err:
        return False, f"failed to read file {path_part!r}: {err}"

    lines = content.splitlines()
    total_lines = len(lines)
    if total_lines == 0:
        return False, f"file {path_part!r} is empty"
    if line_num > total_lines:
        return False, f"line {line_num} out of range (1..{total_lines}) for {path_part!r}"

    quote = ev.get("quote")
    if not isinstance(quote, str):
        return False, f"file evidence {ref!r} missing quote string"

    norm_quote = " ".join(quote.split())
    if not norm_quote:
        return False, f"file evidence {ref!r} quote is empty"

    start_line = max(1, line_num - 3)
    end_line = min(total_lines, line_num + 3)

    matched = False
    for l_idx in range(start_line, end_line + 1):
        line_content = lines[l_idx - 1]
        norm_line = " ".join(line_content.split())
        if norm_quote in norm_line:
            matched = True
            break

    if not matched:
        # Check if the quote spans across lines within the ±3 window
        window_lines = [lines[l_idx - 1] for l_idx in range(start_line, end_line + 1)]
        norm_window = " ".join(" ".join(wl.split()) for wl in window_lines)
        if norm_quote in norm_window:
            matched = True

    if not matched:
        return False, f"quote {quote!r} not found within ±3 lines of line {line_num} in {path_part!r}"

    return True, ""


def verify(brief: Any, root: str | Path) -> Verified:
    """Verify a research brief against the project root.

    Dropped findings have no verifiable evidence.
    refused=True when survivors are fewer than half of total findings, or when there are none.
    """
    root_str = str(root)
    root_real = os.path.realpath(root_str)

    if not isinstance(brief, dict):
        return Verified(
            brief={} if brief is None else brief,
            dropped=[],
            refused=True,
            reasons=["brief is not a dictionary"],
        )

    raw_findings = brief.get("findings")
    if not isinstance(raw_findings, list) or len(raw_findings) == 0:
        clean_brief = copy.deepcopy(brief)
        clean_brief["findings"] = []
        return Verified(
            brief=clean_brief,
            dropped=[],
            refused=True,
            reasons=["no findings in brief"],
        )

    total_findings = len(raw_findings)
    surviving_findings: list[dict[str, Any]] = []
    dropped_ids: list[str] = []
    reasons: list[str] = []

    for f in raw_findings:
        if not isinstance(f, dict):
            dropped_ids.append("")
            reasons.append("finding item is not a dictionary")
            continue

        fid = str(f.get("id", ""))
        evidence_list = f.get("evidence")
        if not isinstance(evidence_list, list) or len(evidence_list) == 0:
            dropped_ids.append(fid)
            reasons.append(f"finding {fid!r} has no evidence")
            continue

        # Check if finding has doc ref naming an existing project file
        has_valid_doc_ref = any(
            isinstance(e, dict) and e.get("kind") == "doc" and _check_doc_ref(str(e.get("ref", "")), root_real)[0]
            for e in evidence_list
        )

        verifiable_evidence: list[dict[str, Any]] = []
        ev_reasons: list[str] = []

        for ev in evidence_list:
            if not isinstance(ev, dict):
                ev_reasons.append("evidence item is not a dictionary")
                continue
            kind = ev.get("kind")
            ref = str(ev.get("ref", ""))

            if kind == "file":
                ok, err = _verify_file_evidence(ev, root_real)
                if ok:
                    verifiable_evidence.append(ev)
                else:
                    ev_reasons.append(err)
            elif kind == "doc":
                ok, err = _check_doc_ref(ref, root_real)
                if ok:
                    verifiable_evidence.append(ev)
                else:
                    ev_reasons.append(err)
            elif kind == "command":
                # doc and command evidence count as verifiable only when doc refs name an existing project file
                if has_valid_doc_ref:
                    verifiable_evidence.append(ev)
                else:
                    ok, _ = _check_doc_ref(ref, root_real)
                    if ok:
                        verifiable_evidence.append(ev)
                    else:
                        ev_reasons.append(f"command evidence {ref!r} is not verifiable without an existing doc ref")
            else:
                ev_reasons.append(f"unknown evidence kind {kind!r}")

        if not verifiable_evidence:
            dropped_ids.append(fid)
            detail = "; ".join(ev_reasons) if ev_reasons else "no verifiable evidence"
            reasons.append(f"finding {fid!r} dropped: {detail}")
        else:
            survivor = copy.deepcopy(f)
            survivor["evidence"] = verifiable_evidence
            surviving_findings.append(survivor)

    num_survivors = len(surviving_findings)
    refused = (num_survivors == 0) or (num_survivors < total_findings / 2)

    if refused:
        if num_survivors == 0:
            reasons.append("refused: all findings were dropped")
        else:
            reasons.append(f"refused: survivors ({num_survivors}) are fewer than half of findings ({total_findings})")

    clean_brief = copy.deepcopy(brief)
    clean_brief["findings"] = surviving_findings

    return Verified(
        brief=clean_brief,
        dropped=dropped_ids,
        refused=refused,
        reasons=reasons,
    )


def render(brief: Any) -> str:
    """Render a research brief as markdown for research.md."""
    if hasattr(brief, "__dataclass_fields__"):
        from dataclasses import asdict
        data = asdict(brief)
    elif isinstance(brief, dict):
        data = brief
    else:
        data = getattr(brief, "__dict__", {})

    lines: list[str] = ["# Research Brief", ""]

    findings = data.get("findings") or []
    if findings:
        lines.append("## Findings")
        for f in findings:
            if not isinstance(f, dict):
                continue
            fid = f.get("id", "")
            claim = f.get("claim", "")
            id_prefix = f"**[{fid}]** " if fid else ""
            lines.append(f"- {id_prefix}{claim}")
            for ev in f.get("evidence") or []:
                if not isinstance(ev, dict):
                    continue
                kind = ev.get("kind", "")
                ref = ev.get("ref", "")
                quote = ev.get("quote", "")
                if quote:
                    lines.append(f"  - [{kind}] `{ref}`: {quote!r}")
                else:
                    lines.append(f"  - [{kind}] `{ref}`")
        lines.append("")

    constraints = data.get("constraints") or []
    if constraints:
        lines.append("## Constraints")
        if isinstance(constraints, str):
            lines.append(constraints)
        else:
            for c in constraints:
                lines.append(f"- {c}")
        lines.append("")

    risks = data.get("risks") or []
    if risks:
        lines.append("## Risks")
        if isinstance(risks, str):
            lines.append(risks)
        else:
            for r in risks:
                lines.append(f"- {r}")
        lines.append("")

    approach = data.get("approach")
    if approach:
        lines.append("## Approach")
        if isinstance(approach, str):
            lines.append(approach)
        elif isinstance(approach, (list, tuple)):
            for a in approach:
                lines.append(f"- {a}")
        lines.append("")

    open_questions = data.get("open_questions") or []
    if open_questions:
        lines.append("## Open Questions")
        if isinstance(open_questions, str):
            lines.append(open_questions)
        else:
            for q in open_questions:
                lines.append(f"- {q}")
        lines.append("")

    return "\n".join(lines).rstrip() + "\n"


def prompt(goal: str, feedback: Any = None) -> str:
    """Generate the prompt for the research role, optionally adding feedback for a retry."""
    parts: list[str] = [
        f"Goal: {goal}",
        "",
        RESEARCH_BRIEF,
    ]

    if feedback:
        parts.append("")
        parts.append("## Feedback from previous attempt")
        if isinstance(feedback, (list, tuple, set)):
            for item in feedback:
                parts.append(f"- {item}")
        else:
            parts.append(str(feedback))
        parts.append("")
        parts.append("Address the feedback above: ground all findings with verifiable file or doc evidence.")

    return "\n".join(parts).strip() + "\n"


__all__ = [
    "RESEARCH_SCHEMA",
    "RESEARCH_BRIEF",
    "Verified",
    "verify",
    "render",
    "prompt",
]
