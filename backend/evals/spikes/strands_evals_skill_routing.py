"""Spike: Strands Evals (strands-agents-evals 1.4.0) for skill search and routing. Not wired into CI or requirements.

    pip install strands-agents-evals   # adds only `rich`; or `pip install --target <dir> --no-deps` and set PYTHONPATH
    backend/.venv/bin/python backend/evals/spikes/strands_evals_skill_routing.py

What it showed (2026-10-05, SDK 1.57.2, evals 1.4.0):

A. **Retrieval as an `Experiment`** — 27 natural requests -> the skills `skill_search` lists, scored by a custom deterministic
   `Evaluator` (hit@1, hit@4): 54 judgements in 0.14 s, no model, a JSON-serialisable experiment and a per-case report. It works,
   but it is a runner around a loop this harness already has (`_skill_search_smoke_test.py` asserts the same recall); the only
   thing it adds is the report and the serialisation.
B. **Trajectory evaluators on a real Strands loop** — `SkillInvoked` / `ToolCalled` read the OpenTelemetry spans of an agent run
   (`StrandsEvalsTelemetry().setup_in_memory_exporter()` + `StrandsInMemorySessionMapper`) and judge "did it load the right skill /
   call the right tool" deterministically and for free. This is the part the harness does not have: it needs the agent built
   with `trace_attributes={"gen_ai.conversation.id": id, "session.id": id}` and a case per request, and it would turn "Laya
   recommended X, the model activated Y" from a calibration signal into a test.
C. **The LLM-judged evaluators** (`SkillSelectionAccuracyEvaluator`, `ToolSelectionAccuracyEvaluator`, ...) default to a Bedrock
   Sonnet judge — AWS credentials, or a `model=` override, and tokens per case. Not for CI; fine for an occasional audit.

Recommendation: do not add the dependency for A. Adopt B (deterministic trajectory checks over scripted runs) the day a routing
or skill-activation regression needs catching that the recall test cannot see.
"""
import os, sys, time, json
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
os.environ.setdefault("CORDATA_HOME", __import__("tempfile").mkdtemp())
os.environ.pop("CORDATA_SKILL_EMBED", None)

import skill_search as ss
from agent_common import SKILLS_DIR
from strands_evals import Case, Experiment
from strands_evals.evaluators import Evaluator, SkillInvoked, ToolCalled
from strands_evals.types.evaluation import EvaluationOutput

CASES = [
 ("write a spec for the new billing feature", "spec"), ("review my plan before I start coding", "review"),
 ("the tests are failing and I can't figure out why", "diagnosing-bugs"), ("help me design a cleaner interface for this module", "design-an-interface"),
 ("turn this into github issues", "to-issues"), ("break the PRD into tickets", "to-issues"), ("let's do test driven development for the parser", "tdd"),
 ("I have a fuzzy idea, interrogate me about it", "grill"), ("fix merge conflicts in my branch", "resolving-merge-conflicts"),
 ("check whether the code still matches the spec", "check"), ("write an article from my notes", "writing-shape"),
 ("set up pre-commit hooks", "setup-pre-commit"), ("research how other projects handle rate limiting", "research"),
 ("make the responses terse like a caveman", "caveman"), ("run the QA pass on this change", "qa"), ("triage the open bugs", "triage"),
 ("write a PRD for the onboarding flow", "to-prd"), ("explain this codebase architecture and improve it", "improve-codebase-architecture"),
 ("hand off this work to another agent", "handoff"), ("define the ubiquitous language for the domain", "ubiquitous-language"),
 ("prototype a quick throwaway version", "prototype"), ("teach me how this works", "teach"), ("implement the next task from the spec", "build"),
 ("this bug keeps coming back, trace it into the spec", "backprop"), ("create a new skill for our workflow", "skill-authoring"),
 ("build a small game for me", "build-a-game"), ("refactor plan with small commits", "request-refactor-plan"),
]

# ------------------------------------------------------------------ A. retrieval as an Experiment
index = ss.SkillIndex(ss.load_skills([SKILLS_DIR]), cache_path=None)
index.warm()
for _ in range(600):
    if index._built:
        break
    time.sleep(0.1)
print("index semantic:", index.semantic)


class HitAtK(Evaluator[str, list]):
    """Is the expected skill among the first k listed? (deterministic, free)"""

    def __init__(self, k: int):
        super().__init__(name=f"hit@{k}")
        self.k = k

    def evaluate(self, case):
        want = case.expected_output
        got = list(case.actual_output or [])[: self.k]
        ok = want in got
        return [EvaluationOutput(score=1.0 if ok else 0.0, test_pass=ok, reason=f"wanted {want}; listed {got}")]

    async def evaluate_async(self, case):
        return self.evaluate(case)


def listed(case: Case) -> list:
    return [h.name for h in index.search(case.input, 4)]


cases = [Case[str, str](name=q[:40], input=q, expected_output=want) for q, want in CASES]
exp = Experiment[str, list](cases=cases, evaluators=[HitAtK(1), HitAtK(4)])
t0 = time.time()
reports = exp.run_evaluations(listed)
reports = reports if isinstance(reports, list) else [reports]
print(f"A: ran {len(cases)} cases x 2 evaluators in {time.time() - t0:.2f}s; {len(reports)} report(s)")
for r in reports:
    print("  attrs:", [a for a in dir(r) if not a.startswith("_")][:14])
    print("  overall:", getattr(r, "overall_score", None), "| scores:", r.scores[:3], "...", "| n:", len(r.scores))
    for c in r.cases[:2]: print("  case:", {k: (str(v)[:70]) for k, v in c.items()})
    print("  detailed:", str(r.detailed_results[:1])[:200])
try:
    exp.to_file(os.path.join(os.environ["CORDATA_HOME"], "exp_skills.json"))
    print("A: experiment serialises to JSON ok")
except Exception as e:
    print("A: to_file failed:", type(e).__name__, e)

# ------------------------------------------------------------------ B. trajectory evaluators, deterministic
from strands import Agent
from strands.vended_plugins.skills import AgentSkills
from strands_evals.mappers import StrandsInMemorySessionMapper
from strands_evals.telemetry import StrandsEvalsTelemetry
from harness_testing import ScriptedModel, tool as call, text

telemetry = StrandsEvalsTelemetry().setup_in_memory_exporter()
exporter = telemetry.in_memory_exporter
skills = ss.load_skills([SKILLS_DIR])


def run_agent(case: Case) -> dict:
    model = ScriptedModel([call("skills", {"skill_name": "spec"}), text("wrote the spec")])
    agent = Agent(model=model, plugins=[AgentSkills(skills=skills)], trace_attributes={"gen_ai.conversation.id": case.session_id, "session.id": case.session_id}, callback_handler=None)
    out = agent(case.input)
    session = StrandsInMemorySessionMapper().map_to_session(exporter.get_finished_spans(), session_id=case.session_id)
    return {"output": str(out), "trajectory": session}


bcase = Case[str, str](name="spec", input="write a spec for billing", expected_output="wrote the spec")
exp2 = Experiment[str, str](cases=[bcase], evaluators=[SkillInvoked(skill_name="spec", name="invoked-spec"), SkillInvoked(skill_name="tdd", name="invoked-tdd"), ToolCalled(tool_name="skills")])
try:
    rep = exp2.run_evaluations(run_agent)
    rep = rep if isinstance(rep, list) else [rep]
    for r in rep:
        print("B:", r.cases[0].get("evaluator"), "->", r.scores, r.cases[0].get("evaluation_reason") or r.reasons)
except Exception as e:
    import traceback; traceback.print_exc()
    print("B failed:", type(e).__name__, str(e)[:300])

sys.stdout.flush()
os._exit(0)
