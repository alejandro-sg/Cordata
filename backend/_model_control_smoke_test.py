"""Regression check for model_control_tool.set_active_model's fail-closed
behavior: when oMLX's /models catalog can't be fetched (unreachable,
timeout, bad JSON — _fetch_model_catalog swallows all three into []), the
switch must be refused, not silently accepted for an unverified model_id.
Monkeypatches _fetch_model_catalog directly — no real oMLX server needed,
no network dependency. Run directly:

    backend/.venv/bin/python _model_control_smoke_test.py
"""

import model_control_tool as mct


class _StubModel:
    def __init__(self):
        self.update_config_calls: list[dict] = []

    def update_config(self, **kwargs):
        self.update_config_calls.append(kwargs)

    def get_config(self):
        return {}


class _StubAgent:
    def __init__(self):
        self.model = _StubModel()


class _StubToolContext:
    def __init__(self):
        self.agent = _StubAgent()


def check(condition: bool, label: str) -> None:
    assert condition, f"FAIL: {label}"
    print(f"ok {label}")


def main() -> None:
    original_fetch = mct._fetch_model_catalog

    try:
        # oMLX unreachable: catalog fetch returns [] (same shape a real
        # URLError/TimeoutError produces, per _fetch_model_catalog's own
        # except clause) — must refuse, not accept blind.
        mct._fetch_model_catalog = lambda: []
        ctx = _StubToolContext()
        result = mct.set_active_model("some-typo-d-model-id", ctx)
        check("Could not reach oMLX" in result, "refuses the switch when the catalog can't be fetched")
        check(ctx.agent.model.update_config_calls == [], "does not call update_config when refusing")

        # oMLX reachable, but the requested id genuinely isn't loaded.
        mct._fetch_model_catalog = lambda: [{"id": "real-model-a"}, {"id": "real-model-b"}]
        ctx = _StubToolContext()
        result = mct.set_active_model("not-a-real-model", ctx)
        check("is not currently loaded" in result, "refuses a genuinely-unlisted model id")
        check(ctx.agent.model.update_config_calls == [], "does not call update_config for an unlisted id")

        # oMLX reachable and the id is actually loaded — the real success path.
        ctx = _StubToolContext()
        result = mct.set_active_model("real-model-a", ctx)
        check("switched to 'real-model-a'" in result, "accepts a genuinely-loaded model id")
        check(len(ctx.agent.model.update_config_calls) == 1, "calls update_config exactly once on success")
    finally:
        mct._fetch_model_catalog = original_fetch

    print("PASS: set_active_model fails closed when the catalog can't be verified")


if __name__ == "__main__":
    main()
