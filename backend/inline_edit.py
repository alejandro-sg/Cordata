"""⌘I in the app's editor (Copilot's inline chat): rewrite a selection by an instruction, in place.

One model call, no tools: the selection, a little context on either side, the file's language and the instruction in;
the replacement code out. The session's own model answers (an edit is reasoning, not rewriting). The selection and
context are bounded; the reply is stripped of fences and prose so it can replace the selection as it stands. The
window applies it as one undoable edit and nothing is saved until the person saves.
"""

from __future__ import annotations

from collections.abc import Callable

MAX_SELECTION = 20_000
MAX_CONTEXT = 3_000
MAX_INSTRUCTION = 2_000

SYSTEM = (
    "You edit code. You are given a selection from a file, some code before and after it, and an instruction. Reply "
    "with the replacement for the selection only: the code that should stand where the selection is, nothing else — no "
    "explanation, no code fences, no surrounding code. Keep the file's indentation and style. If the instruction cannot "
    "be done on this selection, return the selection unchanged."
)


def strip_reply(text: str) -> str:
    """The model's reply as replacement code: a fenced block's inside when there is one, else the reply as it is."""
    text = (text or "").strip("\n")
    if "```" in text:
        start = text.find("```")
        after = text.find("\n", start)
        end = text.find("```", after + 1) if after >= 0 else -1
        if after >= 0 and end > after:
            return text[after + 1 : end].rstrip("\n")
    return text


def build_prompt(selection: str, before: str, after: str, instruction: str, language: str, path: str) -> str:
    return (
        f"File: {path or 'untitled'} ({language or 'text'})\n"
        f"Instruction: {instruction[:MAX_INSTRUCTION]}\n\n"
        f"Code before the selection:\n<before>\n{before[-MAX_CONTEXT:]}\n</before>\n\n"
        f"The selection to replace:\n<selection>\n{selection}\n</selection>\n\n"
        f"Code after the selection:\n<after>\n{after[:MAX_CONTEXT]}\n</after>"
    )


def edit(params: dict, generate: Callable[[str], str]) -> dict:
    """``{ok, code}`` or ``{ok: False, reason}``."""
    selection = str(params.get("code") or "")
    instruction = str(params.get("instruction") or "").strip()
    if not instruction:
        return {"ok": False, "reason": "say what to change"}
    if len(selection) > MAX_SELECTION:
        return {"ok": False, "reason": f"the selection is over {MAX_SELECTION} characters — select less"}
    prompt = build_prompt(selection, str(params.get("before") or ""), str(params.get("after") or ""), instruction, str(params.get("language") or ""), str(params.get("path") or ""))
    try:
        code = strip_reply(generate(prompt))
    except Exception as error:
        return {"ok": False, "reason": f"the model did not answer: {str(error)[:200]}"}
    if not code.strip() and selection.strip():
        return {"ok": False, "reason": "the model returned nothing"}
    return {"ok": True, "code": code}
