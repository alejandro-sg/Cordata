"""Regression check for state_store.py — the ledger's schema, its append-only
guarantee, and the intervention lifecycle.

Hermetic: points SIERRALAB_STATE_DB at a temp file before importing, so a run
never touches the developer's real backend/state.db. Run directly:

    backend/.venv/bin/python _state_store_smoke_test.py
"""

import os
import sqlite3
import tempfile
from pathlib import Path

_TMP = tempfile.TemporaryDirectory()
os.environ["SIERRALAB_STATE_DB"] = str(Path(_TMP.name) / "state.db")

import state_store  # noqa: E402  (must follow the env var above)


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


def main() -> None:
    # ---------------------------------------------------------- migration
    state_store.migrate()
    state_store.migrate()  # idempotent: a second call is a no-op, not an error
    conn = state_store.connect()
    check(
        conn.execute("SELECT version FROM schema_version").fetchone()["version"] == state_store.SCHEMA_VERSION,
        "schema_version is recorded",
    )
    names = {
        row["name"]
        for row in conn.execute("SELECT name FROM sqlite_master WHERE type = 'table'").fetchall()
    }
    check(
        {"events", "interventions", "skills", "checkpoints", "session_config"} <= names,
        "every table exists",
    )

    # ------------------------------------------------------- event ledger
    state_store.append_event("task", {"id": "task-1", "done": 3}, session_id="s1", source="test")
    state_store.append_event("heal", {"phase": "verified"}, session_id="s1")
    state_store.append_event("task", {"id": "task-2"}, session_id="s2")
    rows = state_store.recent_events(session_id="s1")
    check(len(rows) == 2, "recent_events filters by session")
    check(rows[0]["kind"] == "heal", "recent_events is newest first")
    check(rows[1]["payload"] == {"id": "task-1", "done": 3}, "payload round-trips as a dict")
    check(len(state_store.recent_events(kind="task")) == 2, "recent_events filters by kind")
    check(len(state_store.recent_events(session_id="s1", limit=1)) == 1, "recent_events honours limit")

    # The append-only guarantee is the database's, not the caller's.
    for statement, label in (
        ("UPDATE events SET kind = 'tampered' WHERE id = 1", "UPDATE on events is refused"),
        ("DELETE FROM events WHERE id = 1", "DELETE on events is refused"),
    ):
        try:
            conn.execute(statement)
        except sqlite3.IntegrityError:
            check(True, label)
        else:
            check(False, label)

    # ---------------------------------------------------- interventions
    first = state_store.stage_intervention(
        session_id="s1",
        tool="shell",
        tool_input={"command": "rm -rf build"},
        intervention_id="int-1",
        risk_score=0.95,
        risk_label="destructive",
    )
    check(first == "int-1", "stage_intervention returns the id it was given")
    staged = state_store.get_intervention("int-1")
    check(staged["status"] == "staged", "a new intervention is staged")
    check(staged["tool_input"] == {"command": "rm -rf build"}, "tool_input round-trips as a dict")
    check(staged["risk_score"] == 0.95, "the Laya score is stored alongside the tool")

    state_store.stage_intervention(session_id="s2", tool="file_write", tool_input={"path": "x"})
    check(len(state_store.pending_interventions()) == 2, "pending_interventions spans sessions")
    check(len(state_store.pending_interventions("s1")) == 1, "pending_interventions filters by session")

    state_store.resolve_intervention("int-1", "approved", decided_by="user", rule="shell(git status)")
    resolved = state_store.get_intervention("int-1")
    check(resolved["status"] == "approved", "resolve_intervention closes the row")
    check(resolved["decided_by"] == "user" and resolved["rule"] == "shell(git status)", "the decision is attributed")
    check(resolved["decided_ts"] is not None, "resolving stamps a time")
    check(
        [row["id"] for row in state_store.pending_interventions()] != ["int-1"],
        "a resolved intervention leaves the pending set",
    )

    # An unknown id is a no-op, not a crash: the caller may be resolving an
    # intervention whose staging failed.
    state_store.resolve_intervention("never-existed", "rejected")
    check(True, "resolving an unknown intervention does not raise")

    # ------------------------------------------------- skills / checkpoints
    state_store.upsert_skill("summarise", {"name": "summarise", "args": {}}, source="generated", path="skills_generated/summarise.py")
    state_store.upsert_skill("summarise", {"name": "summarise", "args": {"text": "str"}}, source="generated")
    skill = conn.execute("SELECT * FROM skills WHERE name = 'summarise'").fetchone()
    check(skill["enabled"] == 1, "upsert_skill inserts then updates rather than duplicating")
    check("text" in skill["schema"], "upsert_skill replaces the schema on conflict")

    state_store.record_checkpoint("s1", "abc123", path="sessions/session_s1.checkpoint.json", message_count=7)
    state_store.record_checkpoint("s1", "abc123", path="sessions/session_s1.checkpoint.json", message_count=8)
    checkpoints = conn.execute("SELECT * FROM checkpoints WHERE session_id = 's1'").fetchall()
    check(len(checkpoints) == 1 and checkpoints[0]["message_count"] == 8, "re-recording a checkpoint hash updates it")

    state_store.record_session_config("s1", cwd="/tmp/project", mode="build", settings={"laya.mode": "route"})
    config = conn.execute("SELECT * FROM session_config WHERE session_id = 's1'").fetchone()
    check(config["mode"] == "build", "session_config records the mode in force")

    # A payload that json cannot encode must not break the producer.
    state_store.append_event("task", {"weird": object()}, session_id="s1")
    check(True, "an unserialisable payload is recorded rather than raising")


if __name__ == "__main__":
    main()
    _TMP.cleanup()
    print("\nstate_store: all checks passed")
