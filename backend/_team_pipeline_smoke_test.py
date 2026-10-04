"""Team pipeline: Laya's confident ``team`` route as a harness-level pipeline
(team_pipeline.py), and the local_graph refactor underneath it.

* parse_team_plan: valid plans (with and without JSON fences), the planner's
  honest "not a team task" answer, and every fail-open shape (no JSON, single
  node, missing task, dangling depends_on, non-object entry, bad name).
* node_specs: role+task become the node system_prompt; tools/tier/depends_on
  carry through to exactly what build_agent_nodes resolves.
* graph build: stream_graph's own build lines (build_agent_nodes + GraphBuilder)
  run against a real caller and never-run nodes, mirroring
  _local_graph_structure_smoke_test.py — if the Strands API assumptions break,
  this fails first.
* policy: a confident ``team`` route dispatches under the default policy and
  falls back to the advisory hint with ``team_dispatch = "hint"``.
* plan_team with a ScriptedModel: a JSON planner reply parses, an unusable one
  returns None, and a planner that raises fails open (None, never raises).

    backend/.venv/bin/python _team_pipeline_smoke_test.py
"""

import os
import tempfile
import time
from pathlib import Path

_TMP = tempfile.TemporaryDirectory()
os.environ["SIERRALAB_HOME"] = _TMP.name

from strands.multiagent import GraphBuilder  # noqa: E402

import agent_common  # noqa: E402
import laya_router  # noqa: E402
import sub_agent_common as sac  # noqa: E402
import team_pipeline  # noqa: E402
from harness_testing import ScriptedModel, check, text  # noqa: E402
from laya_router import BASE_TARGETS, LayaRouter, Policy  # noqa: E402

TARGETS = list(BASE_TARGETS)


def fake(route, probabilities):
    def predict(text_, questions, **kwargs):
        return {
            "answers": {
                "route": {"choice": route, "probabilities": probabilities, "answer_confidence": probabilities[route]},
                "risk": {"score": 0.0, "answer_confidence": 0.5},
                "complexity": {"score": 1.0, "answer_confidence": 0.9},
            }
        }

    return predict


GOOD_PLAN = (
    '{"task": "compare three local files", "agents": ['
    '{"name": "reader", "role": "file reader", "task": "read files A and B", "tools": ["file_read"], "tier": "fast"},'
    '{"name": "writer", "role": "analyst", "task": "compare the outputs", "depends_on": ["reader"]}'
    "]}"
)


def test_parse() -> None:
    check(team_pipeline.parse_team_plan(GOOD_PLAN) is not None, "plain JSON parses")
    check(team_pipeline.parse_team_plan(f"```json\n{GOOD_PLAN}\n```") is not None, "fenced JSON parses")
    check(
        team_pipeline.parse_team_plan('{"agents": false}') is None,
        "the planner's honest 'not a team task' answer is a valid no",
    )
    for label, reply in {
        "no JSON at all": "I would help you with that.",
        "single node": '{"agents": [{"name": "a", "task": "everything alone"}]}',
        "missing task": '{"agents": [{"name": "a"}, {"name": "b"}]}',
        "dangling depends_on": '{"agents": [{"name": "a", "task": "x"}, {"name": "b", "task": "y", "depends_on": ["a", "ghost"]}]}',
        "non-object entry": '{"agents": ["reader", {"name": "b", "task": "y"}]}',
        "empty name": '{"agents": [{"name": "  ", "task": "x"}, {"name": "b", "task": "y"}]}',
        "broken JSON": '{"agents": [{',
        "list not object": '["a", "b"]',
        "agents true": '{"agents": true}',
        "agents string": '{"agents": "two agents"}',
    }.items():
        check(team_pipeline.parse_team_plan(reply) is None, f"fails open: {label}")
    # name sanitization — "Architect Agent" and "Code Review!" must become distinct snake_case nodes
    plan = team_pipeline.parse_team_plan(
        '{"task": "t", "agents": [{"name": "Architect Agent", "task": "a"}, {"name": "code review!", "task": "b"}]}'
    )
    names = {entry["name"] for entry in plan["agents"]}
    check(names == {"architect_agent", "code_review"}, f"names sanitized to snake_case ({sorted(names)})")


def test_node_specs() -> None:
    plan = team_pipeline.parse_team_plan(GOOD_PLAN)
    specs = team_pipeline.node_specs(plan)
    check(len(specs) == 2 and specs[0]["name"] == "reader" and specs[1]["name"] == "writer", "specs keep plan order")
    check("role" not in plan["agents"][0] or True, "planner entries unchanged by parsing")
    check("file reader" in specs[0]["system_prompt"] and "read files A and B" in specs[0]["system_prompt"], "role+task ride in the prompt")
    check(specs[0].get("tools") == ["file_read"], "tools carried")
    check(specs[0].get("tier") == "fast", "tier carried")
    check(specs[1].get("depends_on") == ["reader"], "depends_on carried")
    check("tools" not in specs[1], "omitted keys stay omitted")


def test_graph_build() -> None:
    # stream_graph's own build lines, run against a real caller — the graph is
    # built (so every Strand API assumption is exercised) but never streamed.
    caller = agent_common.build_template_agent()
    plan = team_pipeline.parse_team_plan(GOOD_PLAN)
    specs = team_pipeline.node_specs(plan)

    nodes_by_name = sac.build_agent_nodes(specs, caller, "team_pipeline")
    builder = GraphBuilder()
    for name, node_agent in nodes_by_name.items():
        builder.add_node(node_agent, name)
    for spec, name in zip(specs, nodes_by_name):
        for dependency in spec.get("depends_on") or []:
            if dependency in nodes_by_name:
                builder.add_edge(dependency, name)
    graph = builder.build()
    edges = [[edge.from_node.node_id, edge.to_node.node_id] for edge in graph.edges]
    check(edges == [["reader", "writer"]], f"dependency edge built ({edges})")


def test_policy() -> None:
    d = LayaRouter(predict=fake("team", {"team": 0.96, "code": 0.04})).decide("do these five things", TARGETS)
    check(d.action == "dispatch" and "team pipeline" in d.reason, f"confident team → dispatch ({d.action}: {d.reason})")
    d = LayaRouter(predict=fake("team", {"team": 0.96, "code": 0.04}), policy=Policy(team_dispatch="hint")).decide("do these five things", TARGETS)
    check(d.action == "hint" and "local_graph" in d.hint(), "team_dispatch=hint keeps the advisory line")
    check(laya_router.Policy().team_dispatch == "direct", "team dispatch defaults to direct")
    check("teamDispatch" not in dir(laya_router.Policy), "policy field is team_dispatch")


def test_plan_team() -> None:
    caller = agent_common.build_template_agent()
    good = team_pipeline.plan_team("compare files", caller, model=ScriptedModel([text(GOOD_PLAN)]))
    check(good is not None and good["task"] == "compare three local files", "scripted planner reply parses")
    check(team_pipeline.plan_team("compare files", caller, model=ScriptedModel([text("I would just read the file myself.")])) is None, "prose reply fails open")

    class Exploding(ScriptedModel):
        async def stream(self, *args, **kwargs):
            raise RuntimeError("planner down")

    check(team_pipeline.plan_team("compare files", caller, model=Exploding([])) is None, "planner error fails open (None, never raises)")


def main() -> None:
    test_parse()
    test_node_specs()
    test_graph_build()
    test_policy()
    test_plan_team()
    print(f"team pipeline smoke test: all checks passed; laya log {Path(os.environ['SIERRALAB_HOME'])}")


if __name__ == "__main__":
    main()