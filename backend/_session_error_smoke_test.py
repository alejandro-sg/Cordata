"""A session that cannot start says why, to the person, instead of "Internal error" and then "HTTP 400".

The model key lives in the macOS Keychain; when it is missing (a fresh machine, a renamed item) building the agent raises
`RuntimeError("MODEL_PROVIDER=… but an API key is not set …")`. Through ACP that reached the client as a bare "Internal
error", and the daemon's `{"error": …}` answer reached the terminal as "daemon transport: HTTP 400". This checks the first
half end to end up to the daemon's wording (`_why`); `hub.rs` has the test for the second.

    backend/.venv/bin/python _session_error_smoke_test.py
"""

import acp

import provider_settings  # noqa: F401  (imported before acp_server, which chdirs)

from acp_server import CordataAcpAgent
from daemon.chat import _why
from harness_testing import check

REASON = "MODEL_PROVIDER=openai_compatible but an API key is not set. For the key: run `make key`."


def main() -> None:
    agent = object.__new__(CordataAcpAgent)

    def refuse(session_id, cwd):
        raise RuntimeError(REASON)

    agent._build_session = refuse
    try:
        agent._open_session("s1", "/tmp")
    except acp.RequestError as error:
        shown = _why(error.to_error_obj())
        check(shown == REASON, f"the client is told the reason, not 'Internal error' ({shown!r})")
    else:
        check(False, "a setup failure is raised")

    def fine(session_id, cwd):
        return "state"

    agent._build_session = fine
    check(agent._open_session("s2", "/tmp") == "state", "a session that builds is returned untouched")

    def broken(session_id, cwd):
        raise KeyError("a bug, not a setup problem")

    agent._build_session = broken
    try:
        agent._open_session("s3", "/tmp")
    except KeyError:
        check(True, "a real bug still surfaces as itself")
    print("\nALL OK")


if __name__ == "__main__":
    main()
