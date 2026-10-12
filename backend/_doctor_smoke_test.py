"""``cordata doctor``: local checks that name the gap and the one command that closes it. No network, no model.

    backend/.venv/bin/python _doctor_smoke_test.py
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

os.environ["CORDATA_HOME"] = tempfile.mkdtemp(prefix="doctor-home-")

import provider_settings  # noqa: F401,E402
from harness_testing import check  # noqa: E402
from orchestrator import doctor  # noqa: E402


def main() -> None:
    print("\n=== MCP servers: a moved repo is the classic break ===")
    home = Path(tempfile.mkdtemp())
    good_script = home / "server.py"
    good_script.write_text("")
    config = home / "mcp.json"
    config.write_text(json.dumps({"mcpServers": {"ok": {"command": sys.executable, "args": [str(good_script)]}}}))
    result = doctor.check_mcp_servers(config)
    check(result.level == doctor.OK and "every path exists" in result.detail, f"valid paths pass ({result.detail})")
    config.write_text(json.dumps({"mcpServers": {"pihole": {"command": "/old/place/.venv/bin/python3", "args": ["/old/place/server.py"]}}}))
    result = doctor.check_mcp_servers(config)
    check(result.level == doctor.FAIL and "pihole" in result.detail and "/old/place" in result.detail and str(config) in result.fix, f"a path that no longer exists fails and says which server ({result.detail})")
    config.write_text("{not json")
    check(doctor.check_mcp_servers(config).level == doctor.FAIL, "a broken file fails")
    check(doctor.check_mcp_servers(home / "missing.json").level == doctor.OK, "no file means none configured")

    print("\n=== the other checks never crash and always have a fix when they are not ok ===")
    checks = doctor.run_checks(str(home))
    check(len(checks) >= 12 and all(c.level in (doctor.OK, doctor.WARN, doctor.FAIL) for c in checks), f"{len(checks)} checks ran")
    check(all(c.fix for c in checks if c.level != doctor.OK), "every warning or failure carries the command that fixes it")
    check(any(c.name == "git" and c.level == doctor.WARN and "git init" in c.fix for c in checks), "a folder that is not a repository is flagged with `git init`")
    text = doctor.render(checks)
    check(text.startswith("cordata doctor") and "→" in text, "the report is one screen with the fixes under the findings")
    check("sk-" not in text and not any(v and v in text for k, v in os.environ.items() if k.endswith("API_KEY")), "no secret is read or printed")

    class Prefs:
        warnings: list = []

        def get(self, key, default=None):
            return {"vault.enabled": True}.get(key, default)

    check(doctor.check_vault(Prefs()).level == doctor.OK, "the vault on is fine")
    check(doctor.check_pools(Prefs()).level == doctor.WARN and "orchestrator.pools" in doctor.check_pools(Prefs()).fix + doctor.check_pools(Prefs()).detail, "placeholder pools are called out")

    print("\n=== through the CLI ===")
    done = subprocess.run([sys.executable, "-m", "orchestrator", "doctor", "--json", "--cwd", str(home)], capture_output=True, text=True, cwd=Path(__file__).parent)
    data = json.loads(done.stdout)
    check(done.returncode in (0, 1) and isinstance(data, list) and {"name", "level", "detail", "fix"} <= set(data[0]), "`doctor --json` prints the checks")
    print("\nall doctor checks passed")


if __name__ == "__main__":
    main()
