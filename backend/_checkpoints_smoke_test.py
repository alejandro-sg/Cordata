"""Checkpoints: is ``auto`` mode really reversible, and honest about where it is not?

Real git, real files, no model:

* **what counts as a write** — the classifier errs toward "may write", never toward "read-only";
* **taking one** — works in a folder that is not a repository and in one that is; the project's own
  ``.git`` is left exactly as it was; secrets, dependency directories, ignored files and huge files are
  never copied; odd file names, executable bits and symlinks survive;
* **restoring** — edits are reverted, created files removed (and their empty directories), deleted files
  recreated, ignored files untouched; a dry run changes nothing; a restore can itself be undone; a file
  somebody edited after the turn is flagged before it is overwritten;
* **turns** — a turn's first checkpoint is "before it changed anything"; undo picks the latest turn not yet
  undone; undoing only some paths leaves the rest;
* **refusals** — your home directory, a path outside the project, a made-up id: all say no;
* **upkeep** — history is bounded, and concurrent snapshots do not collide.

    backend/.venv/bin/python _checkpoints_smoke_test.py
"""

import os
import shutil
import subprocess
import tempfile
import threading
from pathlib import Path

HOME = tempfile.mkdtemp(prefix="ckpt-home-")
os.environ["SIERRALAB_HOME"] = HOME

import checkpoints  # noqa: E402
from harness_testing import check  # noqa: E402


def project(name: str = "p") -> Path:
    root = Path(tempfile.mkdtemp(prefix=f"ckpt-{name}-")).resolve()
    (root / "src").mkdir()
    (root / "src" / "app.py").write_text("print('v1')\n")
    (root / "README.md").write_text("hello\n")
    return root


def git(root: Path, *args: str) -> str:
    return subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True, check=True).stdout


def tree_of(root: Path) -> dict[str, str]:
    out = {}
    for path in sorted(root.rglob("*")):
        if path.is_file() and ".git" not in path.parts:
            out[str(path.relative_to(root))] = path.read_text(errors="replace")
    return out


def shadow_files(root: Path) -> list[str]:
    g = checkpoints._Git(str(root))
    return g.out("ls-tree", "-r", "--name-only", "HEAD").splitlines()


def test_classifier() -> None:
    w = checkpoints.writes_files
    for call in (("file_write", {"path": "a", "content": "x"}), ("apply_patch", {"patch": "x"}), ("python_repl", {"code": "1"}), ("editor", {"command": "str_replace"}), ("claude_code_engineer", {"task": "x"}),
                 ("shell", {"command": "echo hi > out.txt"}), ("shell", {"command": "rm -rf build"}), ("shell", {"command": "sed -i s/a/b/ f"}), ("shell", {"command": "git checkout main"}),
                 ("shell", {"command": "git branch -D old"}), ("shell", {"command": "find . -name x -delete"}), ("shell", {"command": "ls && touch a"}), ("shell", {"command": "cat $(echo f)"}),
                 ("shell", {"command": "unknown-tool --go"}), ("shell", {"command": "cat a | tee b"}), ("shell", {"command": "sort -o out in"}), ("shell", {"command": "git commit -am x"})):
        check(w(*call) is True, f"may write: {call[0]} {call[1].get('command', '')[:40]}")
    for call in (("editor", {"command": "view", "path": "a"}), ("shell", {"command": "ls -la"}), ("shell", {"command": "cat a.txt | grep x | wc -l"}), ("shell", {"command": "git status && git diff"}),
                 ("shell", {"command": "git log --oneline"}), ("shell", {"command": "pytest -q"}), ("shell", {"command": "cd sub && cargo test"}), ("shell", {"command": "find . -name '*.py'"}),
                 ("shell", {"command": "sed -n 1,5p f"}), ("shell", {"command": "echo hello"}), ("shell", {"command": "python3 -m pytest tests"}), ("file_read", {"path": "a"}), ("calculator", {})):
        check(w(*call) is False, f"read-only: {call[0]} {call[1].get('command', '')[:40]}")
    check(w("some_generated_tool", {}, extra_tools={"some_generated_tool"}) is True, "a generated tool is assumed to write")


def test_a_snapshot_and_a_restore() -> None:
    root = project("basic")
    (root / ".env").write_text("SECRET=hunter2\n")
    (root / "key.pem").write_text("-----BEGIN-----\n")
    (root / "node_modules" / "dep").mkdir(parents=True)
    (root / "node_modules" / "dep" / "i.js").write_text("x")
    (root / "my file [1] *.txt").write_text("odd name\n")
    (root / "run.sh").write_text("#!/bin/sh\necho hi\n")
    os.chmod(root / "run.sh", 0o755)
    (root / "link").symlink_to("README.md")
    (root / "big.bin").write_bytes(b"\0" * 20_000)
    (root / "empty_dir_keep").mkdir()

    first = checkpoints.take(root, tool="file_write", agent="orchestrator", session_id="s1", turn="t1", call="write src/app.py", paths=["src/app.py"], max_file_mb=0.01)
    check(first.ok and first.id and len(first.id) == 40 and first.outside == [], f"the first snapshot works in a folder that is not a repository ({first.reason})")
    files = shadow_files(root)
    check("src/app.py" in files and "README.md" in files and "my file [1] *.txt" in files and "run.sh" in files and "link" in files, f"project files are in it ({files})")
    check(".env" not in files and "key.pem" not in files, "secrets files are never copied")
    check(not any(f.startswith("node_modules/") for f in files), "dependency directories are not copied")
    check("big.bin" not in files, "a file over the size limit is left out")
    again = checkpoints.take(root, tool="shell", turn="t1", max_file_mb=0.01)
    check(again.ok and "big.bin" not in shadow_files(root), "…and stays out of later snapshots")
    check(oct(os.stat(shadow_mode := checkpoints.shadow_dir(root)).st_mode & 0o777) == "0o700", "the snapshot directory is private (0700)")

    # A turn's worth of damage.
    (root / "src" / "app.py").write_text("print('v2 broken')\n")
    (root / "README.md").unlink()
    (root / "new_dir").mkdir()
    (root / "new_dir" / "created.txt").write_text("made by the agent\n")
    (root / "src" / "extra.py").write_text("x = 1\n")
    (root / ".env").write_text("SECRET=changed\n")

    dry = checkpoints.restore(root, first.id, dry_run=True)
    actions = {f["path"]: f["action"] for f in dry["files"]}
    check(dry["ok"] and dry["dry_run"] and actions == {"src/app.py": "revert", "README.md": "recreate", "new_dir/created.txt": "delete", "src/extra.py": "delete"}, f"a dry run says exactly what would change ({actions})")
    check((root / "src" / "app.py").read_text() == "print('v2 broken')\n" and not (root / "README.md").exists(), "…and changes nothing")

    done = checkpoints.restore(root, first.id)
    check(done["ok"] and done["count"] == 4 and done["undo_with"], f"restore ok ({done.get('reason')}, {done.get('failed')})")
    check((root / "src" / "app.py").read_text() == "print('v1')\n", "an edited file is back")
    check((root / "README.md").read_text() == "hello\n", "a deleted file is recreated")
    check(not (root / "new_dir").exists() and not (root / "src" / "extra.py").exists(), "created files are removed, and the directory they leave empty with them")
    check((root / ".env").read_text() == "SECRET=changed\n", "a secrets file is never rolled back (it was never copied)")
    check((root / "big.bin").exists() and (root / "empty_dir_keep").exists(), "files the snapshot never held are left alone")
    check(os.access(root / "run.sh", os.X_OK) and (root / "link").is_symlink(), "the executable bit and symlinks survive")

    redo = checkpoints.restore(root, done["undo_with"])
    check(redo["ok"] and (root / "src" / "app.py").read_text() == "print('v2 broken')\n" and (root / "new_dir" / "created.txt").exists(), "a restore can itself be undone")
    shutil.rmtree(root)


def test_the_projects_own_git_is_untouched() -> None:
    root = project("git")
    git(root, "init", "-q", "-b", "main")
    git(root, "config", "user.email", "a@b.c")
    git(root, "config", "user.name", "a")
    (root / ".gitignore").write_text("ignored.log\nbuild/\n")
    git(root, "add", "-A")
    git(root, "commit", "-qm", "init")
    head, status, branches = git(root, "rev-parse", "HEAD"), git(root, "status", "--porcelain"), git(root, "branch", "-a")
    index_before = (root / ".git" / "index").read_bytes()

    (root / "ignored.log").write_text("keep me\n")
    (root / "build").mkdir()
    (root / "build" / "out.o").write_text("object\n")
    snap = checkpoints.take(root, tool="shell", turn="t1")
    (root / "src" / "app.py").write_text("print('changed')\n")
    (root / "ignored.log").write_text("keep me, edited\n")
    checkpoints.restore(root, snap.id)
    index_after = (root / ".git" / "index").read_bytes()  # before any git command of ours: `git status` refreshes the index itself
    check(index_after == index_before, "the project's own index was never written")
    check(git(root, "rev-parse", "HEAD") == head and git(root, "branch", "-a") == branches, "the project's HEAD and branches are unchanged")
    check(git(root, "status", "--porcelain") == status, "…and so is its status")
    check(git(root, "log", "--oneline").count("\n") == 1, "no commit was added to the project's history")
    check((root / "ignored.log").read_text() == "keep me, edited\n" and (root / "build" / "out.o").exists(), "git-ignored files are outside the checkpoint and are not touched by a restore")
    check("ignored.log" not in shadow_files(root) and "build/out.o" not in shadow_files(root), "…and are not copied")
    check((root / "src" / "app.py").read_text() == "print('v1')\n", "tracked changes are still reverted")
    shutil.rmtree(root)


def test_turns_and_undo() -> None:
    root = project("turns")
    t1_first = checkpoints.take(root, tool="file_write", agent="orchestrator", session_id="s", turn="turn-1", call="write a")
    (root / "a.txt").write_text("turn one\n")
    checkpoints.take(root, tool="shell", agent="debugger", session_id="s", turn="turn-1", call="make")
    (root / "b.txt").write_text("turn one too\n")
    t1_end = checkpoints.take(root, kind="end", session_id="s", turn="turn-1")
    t2_first = checkpoints.take(root, tool="apply_patch", agent="orchestrator", session_id="s", turn="turn-2", call="patch")
    (root / "src" / "app.py").write_text("print('turn two')\n")
    (root / "c.txt").write_text("turn two\n")
    checkpoints.take(root, kind="end", session_id="s", turn="turn-2")

    rows = checkpoints.turns(root)
    check([r["turn"] for r in rows] == ["turn-2", "turn-1"], f"turns, newest first ({[r['turn'] for r in rows]})")
    one = rows[1]
    check(one["first"] == t1_first.id and one["end"] == t1_end.id and one["calls"] == 2, "a turn's first checkpoint is the project before it changed anything")
    check(set(one["agents"]) == {"orchestrator", "debugger"} and set(one["tools"]) == {"file_write", "shell"}, f"who and what ({one['agents']}, {one['tools']})")
    check({f["path"] for f in one["files"]} == {"a.txt", "b.txt"} and rows[0]["file_count"] == 2, f"the files each turn changed ({[f['path'] for f in one['files']]})")
    check(rows[0]["first"] == t2_first.id and not any(r["undone"] for r in rows), "none are undone yet")

    (root / "src" / "app.py").write_text("print('the person edited this after the turn')\n")
    preview = checkpoints.undo_turn(root, dry_run=True)
    check(preview["turn"] == "turn-2" and preview["edited_since"] == ["src/app.py"], f"undo warns about a file edited after the turn ({preview['edited_since']})")
    check((root / "c.txt").exists(), "…and a preview changes nothing")

    undone = checkpoints.undo_turn(root)
    check(undone["ok"] and not (root / "c.txt").exists() and (root / "a.txt").exists(), "undoing the latest turn leaves the earlier one alone")
    check((root / "src" / "app.py").read_text() == "print('v1')\n", "…and puts the file back to before the turn, person's edit included (the preview warned)")
    check(checkpoints.turns(root)[0]["undone"] is True, "the turn is marked undone")
    second = checkpoints.undo_turn(root)
    check(second["turn"] == "turn-1" and not (root / "a.txt").exists() and not (root / "b.txt").exists(), "the next undo goes one turn further back")
    check(checkpoints.undo_turn(root)["ok"] is False, "with nothing left, undo says so")

    # Partial: only one path.
    (root / "x.txt").write_text("x\n")
    (root / "y.txt").write_text("y\n")
    snap = checkpoints.take(root, tool="shell", session_id="s", turn="turn-3")
    (root / "x.txt").write_text("x changed\n")
    (root / "y.txt").write_text("y changed\n")
    part = checkpoints.restore(root, snap.id, paths=["x.txt"])
    check(part["ok"] and (root / "x.txt").read_text() == "x\n" and (root / "y.txt").read_text() == "y changed\n", "restoring one path leaves the others as they are")
    shutil.rmtree(root)


def test_refusals() -> None:
    root = project("refuse")
    snap = checkpoints.take(root, tool="shell", turn="t")
    for bad in ("../../etc/passwd", "/etc/hosts", "src/../../x"):
        out = checkpoints.restore(root, snap.id, paths=[bad], dry_run=True)
        check(out["ok"] is False and "outside the project" in out["reason"], f"a path outside the project is refused ({bad})")
    check(checkpoints.restore(root, "nothex!!")["ok"] is False and checkpoints.restore(root, "deadbeefdeadbeef")["ok"] is False, "a made-up checkpoint id is refused")
    check(checkpoints.restore(root, "")["ok"] is False, "an empty id is refused")
    for place in (str(Path.home()), "/", "/tmp", "/Users"):
        result = checkpoints.take(place, tool="shell")
        check(result.ok is False and "refusing" in (result.reason or ""), f"{place} is never checkpointed")
    check(checkpoints.take(root / "nope", tool="shell").ok is False, "a missing directory is reported, not raised")
    outside = checkpoints.take(root, tool="file_write", paths=["src/app.py", "/etc/hosts", "../elsewhere.txt"])
    check(outside.ok and outside.outside == ["/etc/hosts", "../elsewhere.txt"], f"paths outside the project are reported as not covered ({outside.outside})")

    real_which = shutil.which
    checkpoints._VIABLE.clear()
    shutil.which = lambda name, *a, **k: None if name == "git" else real_which(name, *a, **k)
    try:
        result = checkpoints.take(root, tool="shell")
    finally:
        shutil.which = real_which
        checkpoints._VIABLE.clear()
    check(result.ok is False and "git is not installed" in result.reason, "no git: not protected, and it says so")
    big = project("huge")
    old_max = checkpoints.MAX_PROJECT_FILES
    checkpoints.MAX_PROJECT_FILES = 5
    try:
        for i in range(10):
            (big / f"f{i}.txt").write_text("x")
        result = checkpoints.take(big, tool="shell")
    finally:
        checkpoints.MAX_PROJECT_FILES = old_max
        checkpoints._VIABLE.clear()
    check(result.ok is False and "more than" in result.reason, f"a project that is too large is declined with a reason ({result.reason})")
    shutil.rmtree(root)
    shutil.rmtree(big)


def test_upkeep() -> None:
    root = project("upkeep")
    ids = []
    for i in range(30):
        (root / "n.txt").write_text(f"{i}\n")
        ids.append(checkpoints.take(root, tool="shell", session_id="s", turn=f"t{i}", call=f"step {i}").id)
    check(checkpoints.prune(root, keep=40) is False, "a short history is left alone")
    check(checkpoints.prune(root, keep=10) is True, "a long one is rebuilt from the newest")
    git_ = checkpoints._Git(str(root))
    check(int(git_.out("rev-list", "--count", "HEAD").strip()) == 10, "…down to the newest ten")
    check(git_.out("show", "HEAD:n.txt").strip() == "29", "…with the same content")
    rows = checkpoints.list_checkpoints(root, 20)
    check(len(rows) == 10 and rows[0]["call"] == "step 29" and rows[-1]["call"] == "step 20", "…and the same messages")

    threads, results = [], []
    for i in range(8):
        (root / f"c{i}.txt").write_text(str(i))

        def run(i=i):
            results.append(checkpoints.take(root, tool="shell", session_id="s", turn=f"c{i}"))

        threads.append(threading.Thread(target=run))
    [t.start() for t in threads]
    [t.join() for t in threads]
    check(len(results) == 8 and all(r.ok for r in results) and len({r.id for r in results}) == 8, f"eight snapshots at once all succeed, each its own ({[r.reason for r in results if not r.ok]})")
    shutil.rmtree(root)


def main() -> None:
    try:
        test_classifier()
        test_a_snapshot_and_a_restore()
        test_the_projects_own_git_is_untouched()
        test_turns_and_undo()
        test_refusals()
        test_upkeep()
    finally:
        shutil.rmtree(HOME, ignore_errors=True)
    print("\ncheckpoints: all checks passed")


if __name__ == "__main__":
    main()
