"""Semantic skill search with Laya as the decision engine (skill_search.py).

The 49 skills that ship were all listed in the system prompt on every model call (19 KB, ~5k tokens). Now the few that
fit the request are listed, found by embedding search, and Laya — the harness's typed local classifier — decides
whether one clearly fits. This holds the line on:

* **the index** — skills load from folders (a broken one is skipped, the first of two same-named wins); search ranks
  by meaning plus a keyword boost; a request is never answered with nothing; embeddings are cached by content hash,
  so a second session embeds only what changed; with no embedder it degrades to keywords instead of failing;
* **the decision** — one `choice` question over the shortlist plus an explicit `none`; a confident pick is named, an
  abstention or `none` or a score under the gate is not; Laya not loaded yet is "no decision" without asking; a raising
  predict is "no decision"; outcomes feed the same calibrator the router uses;
* **through a real Strands agent** (documented surfaces only: `AgentSkills.set_available_skills`, `ContextInjector`,
  the `Plugin` base) — only the few relevant skills are listed, the confident one first; the hint reaches the model for
  that call and never the stored conversation; `search_skills` finds any skill and makes it activatable (before it,
  an unlisted skill is not); the listing changes from turn to turn; and every failure lists everything rather than
  hiding a skill.

Hermetic: a deterministic bag-of-words embedder, a scripted Laya, `harness_testing.ScriptedModel`. The real numbers
(recall on the shipped skills, Laya's confident-and-right rate) are in the module docstring and were measured live.

    backend/.venv/bin/python _skill_search_smoke_test.py
"""

import hashlib
import math
import os
import tempfile
from pathlib import Path

os.environ.setdefault("SIERRALAB_REFLECT", "off")

from strands import Agent

import skill_search as ss
from harness_testing import ScriptedModel, check, text, tool as call
from laya_calibration import Calibrator

TMP = Path(tempfile.mkdtemp()).resolve()

LIBRARY = {
    "spec": "Create or amend the project specification. Use when the user asks to write a spec or requirements document.",
    "tdd": "Test-driven development: write a failing test first, then make it pass. Use for red-green-refactor work.",
    "to-issues": "Break a plan or PRD into independently-grabbable tickets on the project issue tracker.",
    "resolving-merge-conflicts": "Resolve git merge conflicts carefully, keeping both sides' intent.",
    "design-an-interface": "Design a module's public interface several different ways and compare them.",
    "caveman": "Respond tersely, dropping filler words, while keeping all technical substance.",
    "setup-pre-commit": "Set up pre-commit hooks, linting and formatting checks on commit.",
    "research": "Investigate a question against primary sources and write the findings to a markdown file.",
    "handoff": "Compact the conversation into a handoff document for another agent to pick up.",
    "qa": "Interactive QA session: the user reports bugs and the agent files issues.",
    "build-a-game": "Build a small playable game from a short description.",
    "grill": "Interrogate a fuzzy idea one question at a time before it becomes a spec.",
}


def write_library(root: Path, skills: dict[str, str]) -> Path:
    for name, description in skills.items():
        folder = root / name
        folder.mkdir(parents=True, exist_ok=True)
        (folder / "SKILL.md").write_text(f"---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\nFollow the {name} procedure: step one, step two, step three.\n", encoding="utf-8")
    return root


SKILLS_DIR = write_library(TMP / "skills", LIBRARY)

# A tiny synonym table, so the fake embedder is *semantic* in the one way the tests need: "ticket" is near "issue".
SYNONYMS = {"ticket": "issue", "tickets": "issues", "conflict": "merge", "conflicts": "merge", "terse": "caveman", "brief": "caveman"}
DIMS = 128
EMBED_CALLS: list[int] = []


def fake_embed(texts: list[str]) -> list[list[float]]:
    EMBED_CALLS.append(len(texts))
    out = []
    for item in texts:
        vector = [0.0] * DIMS
        for word in ss._WORD.findall(item.lower()):
            word = SYNONYMS.get(word, word)
            if word in ss._STOP or len(word) <= 2:
                continue
            vector[int(hashlib.md5(word.encode()).hexdigest(), 16) % DIMS] += 1.0
        norm = math.sqrt(sum(v * v for v in vector)) or 1.0
        out.append([v / norm for v in vector])
    return out


def index_for(directory: Path = SKILLS_DIR, **kwargs) -> ss.SkillIndex:
    return ss.SkillIndex(ss.load_skills([directory]), embed=fake_embed, cache_path=None, **kwargs)


# --------------------------------------------------------------------------- the index


def test_loading() -> None:
    broken = TMP / "mixed"
    write_library(broken, {"alpha": "First alpha skill."})
    (broken / "not-a-skill").mkdir()
    (broken / "bad").mkdir()
    (broken / "bad" / "SKILL.md").write_text("no frontmatter at all", encoding="utf-8")
    duplicate = write_library(TMP / "dup", {"alpha": "A different alpha — must lose to the first source."})
    skills = ss.load_skills([broken, duplicate, TMP / "does-not-exist"])
    check([s.name for s in skills] == ["alpha"], "a folder without SKILL.md, a skill with bad frontmatter and a missing source are skipped, not fatal")
    check(skills[0].description == "First alpha skill.", "the first source wins a name clash — a project cannot shadow a shipped skill by accident of order")
    single = ss.load_skills([SKILLS_DIR / "spec"])
    check([s.name for s in single] == ["spec"], "a single skill folder loads too")


def test_search() -> None:
    index = index_for()
    check(len(index) == len(LIBRARY), f"the whole library is indexed ({len(index)})")
    hits = index.search("break this PRD into tickets", 4)
    check(hits[0].name == "to-issues", f"'tickets' finds the issue-tracker skill by meaning ({[h.name for h in hits]})")
    check([h.name for h in index.search("merge conflict in my branch", 3)][0] == "resolving-merge-conflicts", "a merge conflict finds its skill")
    check(index.search("respond more terse and brief please", 2)[0].name == "caveman", "a paraphrase finds the skill whose words it does not share")
    check(all(a.score >= b.score for a, b in zip(hits, hits[1:])), "results are best first")
    check(len(index.search("zzz qqq xxx", 5)) == 5, "a request that matches nothing well still gets the nearest few — what to do with a weak match is Laya's call")
    check(index.search("   ", 5) == [] and ss.SkillIndex([], embed=fake_embed, cache_path=None).search("anything") == [], "an empty request or an empty library has no hits")
    check(index.semantic, "the index reports that it is semantic")


def test_the_cache() -> None:
    path = TMP / "cache" / "skill_index.json"
    EMBED_CALLS.clear()
    first = ss.SkillIndex(ss.load_skills([SKILLS_DIR]), embed=fake_embed, cache_path=path)
    first.build()
    check(EMBED_CALLS == [len(LIBRARY)] and path.exists(), "the first build embeds every skill and writes the cache")
    check(oct(path.stat().st_mode & 0o777) == "0o600", "…readable by this user only")
    EMBED_CALLS.clear()
    ss.SkillIndex(ss.load_skills([SKILLS_DIR]), embed=fake_embed, cache_path=path).build()
    check(EMBED_CALLS == [], "a second session embeds nothing: the vectors come from the cache")
    (SKILLS_DIR / "spec" / "SKILL.md").write_text("---\nname: spec\ndescription: Changed wording for the specification skill.\n---\n\nBody changed too.\n", encoding="utf-8")
    try:
        ss.SkillIndex(ss.load_skills([SKILLS_DIR]), embed=fake_embed, cache_path=path).build()
        check(EMBED_CALLS == [1], f"a changed skill is re-embedded alone ({EMBED_CALLS})")
    finally:
        write_library(SKILLS_DIR, {"spec": LIBRARY["spec"]})
    path.write_text("{ not json", encoding="utf-8")
    EMBED_CALLS.clear()
    ss.SkillIndex(ss.load_skills([SKILLS_DIR]), embed=fake_embed, cache_path=path).build()
    check(EMBED_CALLS == [len(LIBRARY)], "an unreadable cache is rebuilt, not trusted")


def test_no_embedder_means_keywords() -> None:
    original = ss._cpu_embedder
    ss._cpu_embedder = lambda: None
    try:
        index = ss.SkillIndex(ss.load_skills([SKILLS_DIR]), cache_path=None)
        hits = index.search("set up pre-commit hooks for linting", 3)
        check(not index.semantic and hits[0].name == "setup-pre-commit", f"with no sentence-transformer it ranks by keyword overlap instead of failing ({[h.name for h in hits]})")
    finally:
        ss._cpu_embedder = original


def test_the_real_embedder() -> None:
    """The real sentence-transformer, on the CPU, when it is cached (no network is used or needed). Skipped otherwise.

    Why CPU, in one assertion: the shared embedder lives on the Apple GPU, and Metal aborts the process when two threads
    submit at once — Laya warms on another thread at the very moment a first turn wants a query embedded."""
    saved = os.environ.pop("SIERRALAB_SKILL_EMBED", None)
    try:
        model = ss._cpu_embedder()
        if model is None:
            print("skip the real embedder: sentence-transformers or the model is not available here")
            return
        check(str(getattr(model, "device", "cpu")) == "cpu", "the skill embedder is on the CPU, not the GPU a second thread may be using")
        shipped = Path(__file__).parent / "skills"  # the library that really ships, not the throwaway one the other tests build
        index = ss.SkillIndex(ss.load_skills([shipped]), cache_path=TMP / "real_cache.json")
        index.warm()
        for _ in range(600):
            if index._built:
                break
            import time

            time.sleep(0.1)
        check(index._built and index.semantic, "the index builds in the background and goes semantic")
        # 27 natural requests, each with the skill that should be listed (hit@4) and, ideally, first (hit@1). Measured on the
        # shipped library: 27/27 in the nearest four, 22/27 first. Thresholds leave room for a skill being added or reworded.
        cases = [
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
        ranked = [(want, [h.name for h in index.search(query, 4)]) for query, want in cases]
        in_four = sum(1 for want, names in ranked if want in names)
        first = sum(1 for want, names in ranked if names and names[0] == want)
        check(in_four >= 25, f"real embeddings put the right skill in the nearest four for {in_four}/{len(cases)} natural requests")
        check(first >= 20, f"…and first for {first}/{len(cases)}")
        check((TMP / "real_cache.json").exists(), "and the vectors were cached for the next session")
    finally:
        if saved is not None:
            os.environ["SIERRALAB_SKILL_EMBED"] = saved


def test_the_switch_and_a_cold_index() -> None:
    os.environ["SIERRALAB_SKILL_EMBED"] = "off"
    try:
        check(ss._cpu_embedder() is None, "SIERRALAB_SKILL_EMBED=off means no embedder is loaded")
        index = ss.SkillIndex(ss.load_skills([SKILLS_DIR]), cache_path=None)
        hits = index.search("set up pre-commit hooks", 3)
        check(hits and hits[0].name == "setup-pre-commit" and not index.semantic, "…and requests are ranked by keywords, immediately, without waiting on anything")
    finally:
        os.environ.pop("SIERRALAB_SKILL_EMBED", None)
        os.environ["SIERRALAB_SKILL_EMBED"] = "off"  # the scripted default (harness_testing)


# --------------------------------------------------------------------------- the decision


def laya(choice: str, probabilities: dict, *, confidence: float | None = None, low: bool = False):
    seen: list[dict] = []

    def predict(text_, questions, **kwargs):
        seen.append({"text": text_, "questions": questions, "kwargs": kwargs})
        top = confidence if confidence is not None else probabilities.get(choice, 0.0)
        return {"answers": {"skill": {"choice": choice, "probabilities": probabilities, "answer_confidence": top, **({"low_confidence": True} if low else {})}}}

    predict.seen = seen
    return predict


def test_the_decision() -> None:
    hits = index_for().search("write a spec", 3)
    names = [h.name for h in hits]
    predict = laya(names[0], {names[0]: 0.9, names[1]: 0.05, names[2]: 0.03, "none": 0.02})
    decider = ss.SkillDecider(predict, None)
    decision = decider.decide("write a spec", hits)
    check(decision.chosen == names[0] and not decision.abstained and decision.confidence >= 0.9, f"a confident choice names the skill ({decision.reason})")
    asked = predict.seen[0]["questions"]["skill"]
    check(asked["type"] == "choice" and set(asked["criteria"]) == {*names, "none"}, "Laya is asked one choice over the shortlist plus an explicit 'none'")
    check(all(len(v) <= ss.OPTION_CHARS for v in asked["criteria"].values()) and "skill" in asked["instructions"].lower(), "each option is a clipped description")
    check(predict.seen[0]["kwargs"] == {"min_confidence": ss.MIN_CONFIDENCE}, "with the router's abstention floor")

    none = ss.SkillDecider(laya("none", {names[0]: 0.1, "none": 0.85}), None).decide("hello", hits)
    check(none.chosen is None and not none.abstained and "no skill is needed" in none.reason, "a confident 'none' names nothing — and is not an abstention")
    flat = ss.SkillDecider(laya(names[0], {names[0]: 0.4, names[1]: 0.3, "none": 0.3}, low=True), None).decide("hmm", hits)
    check(flat.chosen is None and flat.abstained and "abstained" in flat.reason, "Laya's own low-confidence flag is an abstention")
    gated = ss.SkillDecider(laya(names[0], {names[0]: 0.55, "none": 0.45}), None, gate=0.7).decide("maybe", hits)
    check(gated.chosen is None and gated.abstained and "gate" in gated.reason, "a score under the gate makes no claim")

    asked_at_all = laya(names[0], {names[0]: 0.9})
    not_ready = ss.SkillDecider(asked_at_all, None, ready=lambda: False).decide("x", hits)
    check(not_ready.chosen is None and not asked_at_all.seen and "not loaded" in not_ready.reason, "while Laya's weights are still loading it is not asked — a turn never waits on a model load")

    def boom(*args, **kwargs):
        raise RuntimeError("laya fell over")

    check(ss.SkillDecider(boom, None).decide("x", hits).chosen is None, "a raising predict is 'no decision', never an exception")
    check(ss.SkillDecider(lambda *a, **k: None, None).decide("x", hits).chosen is None, "and so is no answer at all")
    check(ss.SkillDecider(predict, None).decide("x", []).reason == "nothing to choose between", "an empty shortlist is not put to Laya")
    unknown = ss.SkillDecider(laya("made-up", {"made-up": 0.9}), None).decide("x", hits)
    check(unknown.chosen is None and "unknown option" in unknown.reason, "an option that was not offered is refused")


def test_outcomes_calibrate() -> None:
    calibrator = Calibrator(TMP / "cal.json")
    hits = index_for().search("write a spec", 3)
    names = [h.name for h in hits]
    decider = ss.SkillDecider(laya(names[0], {names[0]: 0.9, "none": 0.1}), calibrator)
    decision = decider.decide("write a spec", hits)
    before = calibrator.calibrated("skill", names[0], decision.raw)
    for _ in range(12):
        decider.record(decision, activated=False)
    after = calibrator.calibrated("skill", names[0], decision.raw)
    check(after < before, f"a skill Laya keeps naming that the model keeps ignoring loses confidence ({before:.2f} → {after:.2f})")
    decider.record(ss.SkillDecision(), True)
    check(calibrator.calibrated("skill", names[0], decision.raw) == after, "an abstention teaches nothing")


# --------------------------------------------------------------------------- through a real agent


def agent_for(decider=None, prefs=None, steps=None, **kwargs) -> tuple[Agent, ScriptedModel, list]:
    events: list[dict] = []
    model = ScriptedModel(steps or [text("ok")] * 6)

    class Prefs(dict):
        pass

    plugins = ss.build_skill_plugins(
        [SKILLS_DIR], decider=decider, preferences=(lambda: Prefs(prefs or {})), notify=events.append, index=kwargs.get("index") or index_for()
    )
    agent = Agent(model=model, plugins=plugins, system_prompt="base prompt", callback_handler=None)
    return agent, model, events


def listed(prompt: str) -> list[str]:
    block = prompt[prompt.index("<available_skills>") :]
    return [chunk.split("</name>")[0] for chunk in block.split("<name>")[1:]]


def stock_listing_size() -> int:
    from strands.vended_plugins.skills import AgentSkills

    model = ScriptedModel([text("ok")])
    Agent(model=model, plugins=[AgentSkills(skills=ss.load_skills([SKILLS_DIR]))], system_prompt="base", callback_handler=None)("hello")
    return len(str(model.calls[0]["system_prompt"]))


def test_only_the_relevant_skills_are_listed() -> None:
    hits = index_for().search("break this PRD into tickets", 8)
    names = [h.name for h in hits]
    predict = laya("to-issues", {"to-issues": 0.92, names[1]: 0.04, "none": 0.04})
    agent, model, events = agent_for(ss.SkillDecider(predict, None))
    agent("break this PRD into tickets")
    prompt = str(model.calls[0]["system_prompt"])
    shown = listed(prompt)
    check(len(shown) == ss.LISTED and shown[0] == "to-issues", f"{ss.LISTED} of {len(LIBRARY)} skills are listed, Laya's pick first ({shown})")
    check(len(prompt) < 0.5 * stock_listing_size(), f"the listing is {len(prompt):,} characters against {stock_listing_size():,} for the stock one")
    last = model.calls[0]["last_text"]
    check('<skill_hint>The "to-issues" skill fits this request.' in last and "break this PRD into tickets" in last, "the model is told, for this call, which skill fits — folded into the user's own message")
    check(not any("skill_hint" in str(m) for m in agent.messages), "…and the hint never enters the stored conversation (the injector is ephemeral by design)")
    check(events and events[0]["chosen"] == "to-issues" and events[0]["listed"] == shown and events[0]["semantic"], f"a `skills` event says what was chosen and listed ({events[0]})")

    agent, model, events = agent_for(ss.SkillDecider(laya("none", {"none": 0.9}), None))
    agent("rename this variable")
    check("skill_hint" not in model.calls[0]["last_text"], "a confident 'none' adds no hint")
    check(len(listed(str(model.calls[0]["system_prompt"]))) == ss.LISTED, "…but the nearest skills are still listed, so the model can choose for itself")

    agent, model, _ = agent_for(ss.SkillDecider(laya(names[0], {names[0]: 0.4, "none": 0.3}, low=True), None))
    agent("hmm, something about tickets maybe")
    check("skill_hint" not in model.calls[0]["last_text"] and len(listed(str(model.calls[0]["system_prompt"]))) == ss.LISTED, "an abstaining Laya makes no claim; the shortlist is listed")

    agent, model, _ = agent_for(None)
    agent("break this PRD into tickets")
    check(listed(str(model.calls[0]["system_prompt"]))[0] == "to-issues" and "skill_hint" not in model.calls[0]["last_text"], "with no decider (the web front door, or Laya not loaded) semantic search alone still picks the listing")


def test_pinned_skills_are_always_in_front_of_the_model() -> None:
    """The promotion gate puts a candidate lesson in force with SIERRALAB_EXTRA_SKILLS and measures whether the agent
    does better. If the search could hide it, the gate would be measuring the search."""
    candidate = write_library(TMP / "candidate", {"zebra-lesson": "Obscure unrelated wording about zebras and nothing else."})
    skills = ss.load_skills([SKILLS_DIR, candidate])
    index = ss.SkillIndex(skills, embed=fake_embed, cache_path=None)
    events: list[dict] = []
    model = ScriptedModel([text("ok")])
    plugins = ss.build_skill_plugins([SKILLS_DIR], pinned_sources=[candidate], index=index, notify=events.append)
    agent = Agent(model=model, plugins=plugins, system_prompt="base", callback_handler=None)
    agent("resolve the merge conflict in my branch")
    shown = listed(str(model.calls[0]["system_prompt"]))
    check(shown[0] == "zebra-lesson" and "resolving-merge-conflicts" in shown and len(shown) == ss.LISTED + 1, f"a pinned candidate is listed first on a request it has nothing to do with, beside the usual {ss.LISTED} ({shown})")
    check("zebra-lesson" in index.skills, "…and it is in the searchable library too")

    plain, plain_model, _ = agent_for(None)
    plain("resolve the merge conflict in my branch")
    check("zebra-lesson" not in listed(str(plain_model.calls[0]["system_prompt"])), "without the pin (the baseline arm) it is not")


def test_the_listing_follows_the_request() -> None:
    agent, model, _ = agent_for(None, steps=[text("a"), text("b")])
    agent("resolve the merge conflict in my branch")
    agent("set up pre-commit hooks")
    first, second = (listed(str(c["system_prompt"])) for c in model.calls[:2])
    check(first[0] == "resolving-merge-conflicts" and second[0] == "setup-pre-commit" and first != second, f"the listing is re-chosen every turn ({first[0]} → {second[0]})")


def test_search_skills_widens_what_can_be_activated() -> None:
    request = "resolve the merge conflict in my branch"
    top = [h.name for h in index_for().search(request, ss.LISTED)]
    unlisted = next(name for name in LIBRARY if name not in top)
    agent, model, _ = agent_for(None, steps=[call("skills", {"skill_name": unlisted}), text("x")])
    agent(request)
    refused = model.calls[1]["last_text"]
    check(unlisted not in listed(str(model.calls[0]["system_prompt"])) and "not found" in refused.lower(), f"a skill that is not listed ({unlisted}) cannot be activated by name…")

    agent, model, _ = agent_for(None, steps=[call("search_skills", {"query": LIBRARY[unlisted]}), call("skills", {"skill_name": unlisted}), text("done")])
    agent(request)
    found = model.calls[1]["last_text"]
    check(unlisted in found and "Best matches" in found, "…`search_skills` searches the whole library by meaning and says what it found")
    check(f"Follow the {unlisted} procedure" in model.calls[2]["last_text"], "…and what it found can then be activated: its full instructions arrive")
    check("search_skills" in model.calls[0]["tools"], "the search tool is always there")

    agent, model, _ = agent_for(None, steps=[call("search_skills", {"query": "zzz"}), text("x")])
    agent("hi")
    check("Best matches" in model.calls[1]["last_text"], "a poor query still returns the nearest skills")


def test_switches_and_failures() -> None:
    agent, model, events = agent_for(None, prefs={"context.skillSearch": False})
    agent("resolve the merge conflict in my branch")
    check(len(listed(str(model.calls[0]["system_prompt"]))) == len(LIBRARY) and not events, "context.skillSearch=false lists every skill, as before")

    agent, model, _ = agent_for(None, prefs={"context.skillsListed": 2})
    agent("resolve the merge conflict in my branch")
    check(len(listed(str(model.calls[0]["system_prompt"]))) == 2, "context.skillsListed sets how many")
    agent, model, _ = agent_for(None, prefs={"context.skillsListed": 99})
    agent("hi")
    check(len(listed(str(model.calls[0]["system_prompt"]))) == 12, "…within bounds")

    class Broken(ss.SkillIndex):
        def search(self, query, k=5):
            raise RuntimeError("index fell over")

    agent, model, _ = agent_for(None, index=Broken(ss.load_skills([SKILLS_DIR]), embed=fake_embed, cache_path=None))
    agent("anything")
    check(len(listed(str(model.calls[0]["system_prompt"]))) == len(LIBRARY), "a selection that fails lists every skill — slower to read, never a skill out of reach")


def test_outcomes_are_recorded_through_the_agent() -> None:
    calibrator = Calibrator(TMP / "cal2.json")
    names = [h.name for h in index_for().search("break this PRD into tickets", 8)]
    predict = laya("to-issues", {"to-issues": 0.9, "none": 0.1})
    decider = ss.SkillDecider(predict, calibrator)
    agent, model, _ = agent_for(decider, steps=[call("skills", {"skill_name": "to-issues"}), text("done")])
    agent("break this PRD into tickets")
    entry = calibrator.maps.get("skill/to-issues")
    check(entry is not None and sum(trials for _, trials in entry["bins"]) == 1 and sum(wins for wins, _ in entry["bins"]) == 1, "Laya named a skill and the model activated it: recorded as a hit")

    agent, model, _ = agent_for(decider, steps=[text("answered without it")])
    agent("break this PRD into tickets")
    entry = calibrator.maps.get("skill/to-issues")
    check(sum(trials for _, trials in entry["bins"]) == 2 and sum(wins for wins, _ in entry["bins"]) == 1, "named but ignored: recorded as a miss")

    agent, model, _ = agent_for(ss.SkillDecider(laya("none", {"none": 0.9}), calibrator), steps=[text("ok")])
    agent("hello")
    check(sum(trials for _, trials in calibrator.maps["skill/to-issues"]["bins"]) == 2, "a turn with no claim teaches nothing")


def main() -> None:
    test_loading()
    test_search()
    test_the_cache()
    test_no_embedder_means_keywords()
    test_the_switch_and_a_cold_index()
    test_the_real_embedder()
    test_the_decision()
    test_outcomes_calibrate()
    test_only_the_relevant_skills_are_listed()
    test_pinned_skills_are_always_in_front_of_the_model()
    test_the_listing_follows_the_request()
    test_search_skills_widens_what_can_be_activated()
    test_switches_and_failures()
    test_outcomes_are_recorded_through_the_agent()
    print("\nPASS: skills are found by meaning, Laya decides when one clearly fits, the prompt lists a few instead of all, and every skill stays one search away")


if __name__ == "__main__":
    main()
