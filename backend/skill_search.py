"""Semantic skill search, with Laya as the decision engine.

The problem. The 49 skills that ship (plus whatever a project has learned) were all listed in the system prompt, in
full, on every model call: 19 KB, ~5k tokens, most of it about skills a given request will never touch — and a model
choosing among forty-nine descriptions is choosing badly. The Strands docs call the intended pattern *progressive
disclosure*: the agent sees a little, loads the rest on demand. This module makes "a little" mean "the few skills
that are relevant to this request".

Two engines, each doing the job it is good at.

* **Semantic search (recall).** Every skill is embedded once (name, description and the opening of its instructions,
  with the same local sentence-transformer the harness's memory already runs — `multi-qa-MiniLM-L6-cos-v1`, trained
  for exactly "short question → the passage that answers it"). A request is embedded and the nearest skills form a
  shortlist. Cheap, deterministic, wide: it will not miss the skill, and it does not know when *none* applies.
* **Laya (the decision).** Laya, the harness's typed local classifier, is asked one `choice` question over the
  shortlist plus an explicit `none` — "which skill, if any, should guide this request?" — and answers with a
  calibrated probability and an abstention flag, exactly as it does for routing (`laya_router`). Confident → that
  skill is listed first and the model is told, once and ephemerally, that it fits. Unsure or `none` → no claim is made;
  the shortlist is listed and the model decides. Outcomes feed back: a skill Laya picked that the model then
  activated is a hit, one it ignored a miss, and the same `Calibrator` that calibrates routes calibrates these.

What the agent sees, through documented Strands surfaces only:

* `AgentSkills.set_available_skills` (docs: "Managing skills at runtime" — the XML block is refreshed before each
  invocation) lists the top few skills, not all of them. Activation is unchanged: the `skills` tool, by name.
* `search_skills` — a tool, for when the listed ones do not cover it: semantic search over the *whole* library, and
  whatever it returns becomes activatable.
* `ContextInjector` (docs: plugins/context-injector) carries Laya's hint into the model input for that one call
  without touching the stored conversation.

Failure modes are all "less clever", never "broken": no embedder → lexical scoring; Laya not loaded yet (it warms in
the background) or abstaining → the shortlist alone; the whole feature switched off (`context.skillSearch: false`) →
every skill is listed, as before. A skill is never *removed* from reach: anything in the library is one
`search_skills` away.
"""

from __future__ import annotations

import hashlib
import json
import logging
import math
import os
import re
import threading
import time
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from strands import tool
from strands.hooks import AfterInvocationEvent, BeforeInvocationEvent
from strands.plugins import Plugin, hook
from strands.types.tools import ToolContext
from strands.vended_plugins.context_injector import ContextInjector
from strands.vended_plugins.skills import AgentSkills, Skill

logger = logging.getLogger(__name__)

EMBED_MODEL = "multi-qa-MiniLM-L6-cos-v1"
#: Skills listed in the prompt for a turn (the nearest few, with Laya's pick first).
LISTED = 4
#: How many nearest skills are put to Laya as the choice's options.
CANDIDATES = 8
#: Laya's gate for naming a skill, as `laya.threshold` is for routing: below this, no claim is made.
GATE = 0.6
MIN_CONFIDENCE = 0.5
#: Weight of keyword overlap added to the embedding similarity. On 27 natural requests against the 49 shipped skills
#: the nearest-four recall was 25/27 for embeddings alone and 27/27 with this — a request that shares a word with a
#: skill's description ("PRD", "merge", "issues") is a signal embeddings alone under-weight.
LEXICAL_WEIGHT = 0.3
#: How much of a skill's description Laya reads per option, and how much of its opening instructions is embedded.
OPTION_CHARS = 220
BODY_CHARS = 300
NONE_OPTION = "none"
NONE_DESCRIPTION = "No skill is needed: a plain answer, a quick lookup, or a small change that needs no procedure."
QUESTION = "Which skill, if any, should guide this request?"


# --------------------------------------------------------------------------- loading


def load_skills(sources: Sequence[str | Path]) -> list[Skill]:
    """Every skill under these sources — a folder of skill folders (`backend/skills`, a project's learned skills) or one
    skill folder — de-duplicated by name, the first source winning. A source that cannot be read is skipped, never
    fatal: a broken learned skill must not take the shipped ones with it."""
    found: dict[str, Skill] = {}
    for source in sources:
        path = Path(source)
        try:
            skills = [Skill.from_file(path)] if (path / "SKILL.md").is_file() else Skill.from_directory(path)
        except (OSError, ValueError):
            logger.warning("skills: could not read %s", path, exc_info=True)
            continue
        for skill in skills:
            found.setdefault(skill.name, skill)
    return list(found.values())


def clip(text: str, limit: int) -> str:
    flat = " ".join((text or "").split())
    return flat if len(flat) <= limit else flat[: limit - 1].rstrip() + "…"


def skill_text(skill: Skill) -> str:
    """What is embedded for a skill: its name as words, its description, and the opening of its instructions (a
    terse description is often made clear by the first lines of the body)."""
    body = " ".join((skill.instructions or "").split())[:BODY_CHARS]
    return f"{skill.name.replace('-', ' ')}. {skill.description} {body}".strip()


# --------------------------------------------------------------------------- the index


@dataclass(frozen=True)
class SkillHit:
    name: str
    score: float
    description: str


_WORD = re.compile(r"[a-z0-9]+")
_STOP = frozenset("the a an and or of to in on for with is are be this that it as at by from use when user asks".split())


def _tokens(text: str) -> set[str]:
    return {t for t in _WORD.findall((text or "").lower()) if len(t) > 2 and t not in _STOP}


_MODEL: Any = None
_MODEL_LOCK = threading.Lock()
_ENCODE_LOCK = threading.Lock()


def _cpu_embedder() -> Any | None:
    """A sentence-transformer of this module's own, **on the CPU**, loaded once per process. None when the library is not
    installed, the model is not cached, or `SIERRALAB_SKILL_EMBED=off`.

    Not the one `agent_memory_tool` holds: that lives on the Apple GPU (MPS), and Metal aborts the whole process
    (`failed assertion _status < MTLCommandBufferStatusCommitted`, SIGABRT) when two threads submit work at once — and
    Laya's weights are warming on another thread at the very time a session's first turn wants a query embedded. A
    one-sentence query on a 6-layer MiniLM costs milliseconds on the CPU, so nothing is gained by sharing and a crash
    is risked by it. The cost is one more copy of ~90 MB of weights in the agent process."""
    global _MODEL
    if os.environ.get("SIERRALAB_SKILL_EMBED", "").strip().lower() in ("off", "0", "false", "no"):
        return None
    with _MODEL_LOCK:
        if _MODEL is None:
            try:
                from sentence_transformers import SentenceTransformer  # noqa: PLC0415 — heavy: only when a session needs it

                try:
                    _MODEL = SentenceTransformer(EMBED_MODEL, device="cpu", local_files_only=True)  # cached: no network, no stall
                except Exception:
                    _MODEL = SentenceTransformer(EMBED_MODEL, device="cpu")
            except Exception:
                logger.info("skills: no sentence-transformer available; ranking by keyword overlap", exc_info=True)
                return None
        return _MODEL


def _cache_path() -> Path | None:
    try:
        import settings_store  # noqa: PLC0415

        return settings_store.user_dir() / "skill_index.json"
    except Exception:
        return None


class SkillIndex:
    """The library, embedded. `search` is a cosine nearest-neighbour; with no embedder it is keyword overlap, which
    ranks worse but still ranks. Embeddings are cached on disk by content hash (mode 0600), so a session start
    embeds only skills that are new or changed."""

    def __init__(
        self,
        skills: Sequence[Skill],
        *,
        embed: Callable[[list[str]], Any] | None = None,
        cache_path: Path | None | str = "default",
    ) -> None:
        self.skills: dict[str, Skill] = {s.name: s for s in skills}
        self._texts = {name: skill_text(skill) for name, skill in self.skills.items()}
        self._embed = embed
        #: An injected embedder (a test) is a different "model": it never reads or writes the shared default cache, and
        #: an explicit cache path it is given is tagged so another model's vectors are never mistaken for its own.
        self._tag = EMBED_MODEL if embed is None else "injected"
        if cache_path == "default":
            self._cache = _cache_path() if embed is None else None
        else:
            self._cache = Path(cache_path) if cache_path else None
        self._vectors: dict[str, list[float]] | None = None
        self._built = False
        self._lock = threading.RLock()
        self._warming = False
        self.semantic = False

    def __len__(self) -> int:
        return len(self.skills)

    def _encoder(self) -> Callable[[list[str]], Any] | None:
        if self._embed is not None:
            return self._embed
        model = _cpu_embedder()
        if model is None:
            return None

        def encode(texts: list[str]) -> Any:
            with _ENCODE_LOCK:  # one encode at a time in this process, whatever thread asks
                return model.encode(texts, normalize_embeddings=True)

        return encode

    def warm(self) -> None:
        """Build in the background so the first request does not wait for a model to load. A request that arrives before
        it finishes is ranked by keywords once and by meaning after. An injected embedder (a test) builds at once."""
        if self._embed is not None:
            self.build()
            return
        with self._lock:
            if self._built or self._warming:
                return
            self._warming = True
        threading.Thread(target=self.build, name="skill-index", daemon=True).start()

    def build(self) -> None:
        """Embed what is not cached. Idempotent; never raises — a failure leaves the index lexical."""
        with self._lock:
            self._build()

    def _build(self) -> None:
        if self._built:
            return
        try:
            encode = self._encoder()
            if encode is None:
                self._built = True
                return
            cached = self._read_cache()
            keys = {name: hashlib.sha256(text.encode("utf-8")).hexdigest() for name, text in self._texts.items()}
            missing = [name for name in self.skills if keys[name] not in cached]
            if missing:
                started = time.perf_counter()
                fresh = encode([self._texts[name] for name in missing])
                for name, vector in zip(missing, fresh):
                    cached[keys[name]] = [round(float(v), 5) for v in vector]
                logger.info("skills: embedded %d new/changed skill(s) in %.0f ms", len(missing), (time.perf_counter() - started) * 1000)
                self._write_cache({keys[name]: cached[keys[name]] for name in self.skills})
            self._vectors = {name: cached[keys[name]] for name in self.skills}
            self._encode = encode
            self.semantic = True  # last: a reader that sees it set sees everything it needs
        except Exception:
            logger.warning("skills: could not build the semantic index; ranking by keyword overlap", exc_info=True)
            self._vectors, self.semantic = None, False
        finally:
            self._built = True

    def _read_cache(self) -> dict[str, list[float]]:
        if self._cache is None:
            return {}
        try:
            data = json.loads(self._cache.read_text(encoding="utf-8"))
            if data.get("model") == self._tag and isinstance(data.get("vectors"), dict):
                return dict(data["vectors"])
        except (OSError, ValueError):
            pass
        return {}

    def _write_cache(self, vectors: dict[str, list[float]]) -> None:
        if self._cache is None:
            return
        try:
            self._cache.parent.mkdir(parents=True, exist_ok=True)
            tmp = self._cache.with_name(self._cache.name + ".tmp")
            fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                json.dump({"model": self._tag, "vectors": vectors}, handle)
            os.replace(tmp, self._cache)
        except OSError:
            logger.debug("skills: could not cache the index", exc_info=True)

    def search(self, query: str, k: int = 5) -> list[SkillHit]:
        """The `k` nearest skills to `query`, best first. Never empty for a non-empty library: a request that matches
        nothing well still gets the least-bad few, because what to *do* with a weak match is Laya's call, not this."""
        if not self.skills or not (query or "").strip():
            return []
        if self._embed is not None:
            self.build()  # an injected embedder is instant
        elif not self._built:
            self.warm()  # the real model loads in the background; this request is ranked by keywords
        scores = self._semantic_scores(query) if self.semantic else self._lexical_scores(query)
        ranked = sorted(scores, key=lambda name: (-scores[name], name))[: max(1, k)]
        return [SkillHit(name, round(scores[name], 4), self.skills[name].description) for name in ranked]

    def _semantic_scores(self, query: str) -> dict[str, float]:
        try:
            q = [float(v) for v in self._encode([query[:2000]])[0]]
        except Exception:
            logger.debug("skills: embedding the query failed", exc_info=True)
            return self._lexical_scores(query)
        norm = math.sqrt(sum(v * v for v in q)) or 1.0
        lexical = self._lexical_scores(query)
        top = max(lexical.values(), default=0.0) or 1.0
        out = {}
        for name, vector in (self._vectors or {}).items():
            denom = (math.sqrt(sum(v * v for v in vector)) or 1.0) * norm
            out[name] = sum(a * b for a, b in zip(q, vector)) / denom + LEXICAL_WEIGHT * lexical.get(name, 0.0) / top
        return out

    def _lexical_scores(self, query: str) -> dict[str, float]:
        wanted = _tokens(query)
        out = {}
        for name, skill in self.skills.items():
            words = _tokens(f"{name.replace('-', ' ')} {skill.description}")
            out[name] = (len(wanted & words) / math.sqrt(len(words) or 1)) if wanted else 0.0
        return out


# --------------------------------------------------------------------------- the decision


@dataclass
class SkillDecision:
    chosen: str | None = None  # the skill Laya names with confidence; None for "none" and for abstaining
    confidence: float = 0.0  # calibrated
    raw: float = 0.0  # weighted, pre-calibration — what outcomes are recorded against
    abstained: bool = True
    reason: str = "no decision"
    probabilities: dict[str, float] = field(default_factory=dict)
    latency_ms: float | None = None


class SkillDecider:
    """Laya's reading of "which skill, if any". The question, the abstention rule and the calibration are the router's
    own (`laya_router.LayaRouter.decide`): one forward pass, a calibrated probability, and no claim when it is unsure."""

    def __init__(
        self,
        predict: Callable[..., dict | None],
        calibrator: Any = None,
        *,
        min_confidence: float = MIN_CONFIDENCE,
        gate: float = GATE,
        ready: Callable[[], bool] | None = None,
    ) -> None:
        self._predict = predict
        self.calibrator = calibrator
        self.min_confidence = min_confidence
        self.gate = gate
        self._ready = ready

    def decide(self, text: str, shortlist: Sequence[SkillHit]) -> SkillDecision:
        """Never raises, never blocks on loading Laya's weights."""
        if not shortlist:
            return SkillDecision(reason="nothing to choose between")
        if self._ready is not None and not self._ready():
            return SkillDecision(reason="Laya is not loaded yet")
        criteria = {hit.name: clip(hit.description, OPTION_CHARS) for hit in shortlist}
        criteria[NONE_OPTION] = NONE_DESCRIPTION
        started = time.perf_counter()
        try:
            result = self._predict(text, {"skill": {"type": "choice", "instructions": QUESTION, "criteria": criteria}}, min_confidence=self.min_confidence)
            answer = (result or {})["answers"]["skill"]
            probabilities = {k: float(v) for k, v in (answer.get("probabilities") or {}).items() if k in criteria}
            raw_choice, raw_conf = answer.get("choice"), float(answer.get("answer_confidence", 0.0))
        except Exception as error:
            return SkillDecision(reason=f"no decision: {type(error).__name__}")
        latency = round((time.perf_counter() - started) * 1000, 1)
        if self.calibrator is not None and probabilities:
            label, weighted = self.calibrator.choose("skill", probabilities)
            calibrated = self.calibrator.calibrated("skill", label, weighted)
        else:
            label, weighted = raw_choice, (probabilities.get(raw_choice, raw_conf) if probabilities else raw_conf)
            calibrated = weighted
        decision = SkillDecision(confidence=float(calibrated), raw=float(weighted), probabilities=probabilities, latency_ms=latency)
        if label not in criteria:
            decision.reason = f"unknown option {label!r}"
        elif answer.get("low_confidence"):
            decision.reason = f"Laya abstained (answer_confidence {raw_conf:.2f} < {self.min_confidence:.2f})"
        elif weighted < self.min_confidence or calibrated < self.gate:
            decision.reason = f"confidence {calibrated:.2f} is under the gate {self.gate:.2f}"
        elif label == NONE_OPTION:
            decision.abstained, decision.reason = False, "Laya is confident no skill is needed"
        else:
            decision.chosen, decision.abstained, decision.reason = label, False, f"Laya is confident ({calibrated:.2f})"
        return decision

    def record(self, decision: SkillDecision, activated: bool) -> None:
        """A skill Laya named was (or was not) activated by the model this turn — the label calibration learns from."""
        if self.calibrator is None or decision.chosen is None:
            return
        try:
            self.calibrator.update("skill", decision.chosen, decision.raw, activated)
        except Exception:
            logger.debug("skills: could not record the outcome", exc_info=True)


# --------------------------------------------------------------------------- the plugin


@dataclass
class _Turn:
    decision: SkillDecision
    listed: list[str]
    shortlist: list[SkillHit]
    activated_before: set[str]


class SemanticSkills(Plugin):
    """Chooses which skills `AgentSkills` lists on each user turn, and gives the agent `search_skills`.

    Registered *before* the `AgentSkills` plugin, so its `BeforeInvocationEvent` hook runs first and the XML block
    `AgentSkills` then injects already describes this turn's selection (docs: the block "is refreshed before each
    invocation")."""

    name = "sierralab:semantic-skills"

    def __init__(
        self,
        index: SkillIndex,
        agent_skills: AgentSkills,
        decider: SkillDecider | None = None,
        *,
        preferences: Callable[[], Any] | None = None,
        notify: Callable[[dict[str, Any]], None] | None = None,
        pinned: Sequence[str] = (),
    ) -> None:
        self.index = index
        self.agent_skills = agent_skills
        self.decider = decider
        #: Listed on every turn, on top of the selection — a candidate skill the promotion gate is measuring
        #: (`SIERRALAB_EXTRA_SKILLS`) must be in front of the model in its arm, or the measurement is of the search.
        self.pinned = [name for name in dict.fromkeys(pinned) if name in index.skills]
        self._preferences = preferences
        self._notify = notify
        self._turns: dict[int, _Turn] = {}
        super().__init__()

    # -- settings --------------------------------------------------------------

    def _get(self, key: str, default: Any) -> Any:
        try:
            value = self._preferences().get(key, default) if self._preferences is not None else default
        except Exception:
            return default
        return default if value is None else value

    def enabled(self) -> bool:
        return self._get("context.skillSearch", True) is not False

    def listed_count(self) -> int:
        try:
            return max(1, min(12, int(float(self._get("context.skillsListed", LISTED)))))
        except (TypeError, ValueError):
            return LISTED

    # -- per turn ---------------------------------------------------------------

    @hook
    def select(self, event: BeforeInvocationEvent) -> None:
        from laya_triage import user_text  # noqa: PLC0415 — keeps this module importable without the router

        text = user_text(event.messages)
        if text is None or text.startswith("[self-heal]"):
            return  # an approval resume or a self-heal continuation: the same turn, with the same listing
        agent = event.agent
        try:
            if not self.enabled():
                self._turns.pop(id(agent), None)
                self.agent_skills.set_available_skills(list(self.index.skills.values()))
                return
            shortlist = self.index.search(text, max(CANDIDATES, self.listed_count()))
            decision = self.decider.decide(text, shortlist) if self.decider is not None else SkillDecision(reason="no decider")
            chosen = [decision.chosen] if decision.chosen and decision.chosen not in self.pinned else []
            rest = [hit.name for hit in shortlist if hit.name not in self.pinned and hit.name not in chosen]
            listed = list(self.pinned) + (chosen + rest)[: self.listed_count()]
            self.agent_skills.set_available_skills([self.index.skills[name] for name in listed])
            self._turns[id(agent)] = _Turn(decision, listed, shortlist, set(self.agent_skills.get_activated_skills(agent)))
            if self._notify is not None:
                self._notify(
                    {
                        "event": "skills",
                        "chosen": decision.chosen,
                        "confidence": round(decision.confidence, 3),
                        "abstained": decision.abstained,
                        "reason": decision.reason,
                        "listed": listed,
                        "semantic": self.index.semantic,
                        "latency_ms": decision.latency_ms,
                    }
                )
        except Exception:
            # Fail open to the whole library: slower to read, never a skill the agent cannot reach.
            logger.warning("skills: selection failed; listing every skill", exc_info=True)
            try:
                self.agent_skills.set_available_skills(list(self.index.skills.values()))
            except Exception:
                pass

    @hook
    def learn(self, event: AfterInvocationEvent) -> None:
        stop_reason = getattr(event.result, "stop_reason", None)
        if stop_reason in ("interrupt", "checkpoint") or event.resume is not None:
            return  # the same turn continues
        turn = self._turns.pop(id(event.agent), None)
        if turn is None or self.decider is None or turn.decision.chosen is None:
            return
        try:
            now = set(self.agent_skills.get_activated_skills(event.agent))
            self.decider.record(turn.decision, turn.decision.chosen in (now - turn.activated_before))
        except Exception:
            logger.debug("skills: could not score the decision", exc_info=True)

    def hint(self, agent: Any) -> str | None:
        """The one line the model is told, for this call only, when Laya is confident which skill fits. Built from
        the skill's name — a controlled string, not text a request or a tool wrote."""
        turn = self._turns.get(id(agent))
        if turn is None or turn.decision.chosen is None or turn.decision.chosen not in self.index.skills:
            return None
        name = turn.decision.chosen
        return f'<skill_hint>The "{name}" skill fits this request. Activate it with the skills tool before you start, and follow it.</skill_hint>'

    # -- the tool ---------------------------------------------------------------

    @tool(context=True)
    def search_skills(self, query: str, tool_context: ToolContext, limit: int = 5) -> str:
        """Search the whole skill library by meaning, when the skills listed in your prompt do not cover the task.

        Returns the best matches with a one-line description each, and makes them available: activate one by name
        with the skills tool.

        Args:
            query: What you need to do, in a sentence — not keywords.
            limit: How many matches to return (1-10).
        """
        agent = tool_context.agent
        limit = max(1, min(10, int(limit)))
        hits = self.index.search(query, limit)
        if not hits:
            return "No skills matched."
        turn = self._turns.get(id(agent))
        listed = list(turn.listed) if turn is not None else []
        for hit in hits:
            if hit.name not in listed:
                listed.append(hit.name)
        self.agent_skills.set_available_skills([self.index.skills[name] for name in listed])
        if turn is not None:
            turn.listed = listed
        lines = [f"- {hit.name}: {clip(hit.description, 160)}" for hit in hits]
        return "Best matches (activate one with the skills tool):\n" + "\n".join(lines)


# --------------------------------------------------------------------------- assembly


def build_skill_plugins(
    sources: Sequence[str | Path],
    *,
    decider: SkillDecider | None = None,
    preferences: Callable[[], Any] | None = None,
    notify: Callable[[dict[str, Any]], None] | None = None,
    index: SkillIndex | None = None,
    pinned_sources: Sequence[str | Path] = (),
) -> list[Plugin]:
    """The three plugins, in the order that matters: selection, then Strands' `AgentSkills` (which injects the block
    and owns the `skills` activation tool), then the hint injector."""
    skills = load_skills([*sources, *pinned_sources])
    index = index or SkillIndex(skills)
    index.warm()
    pinned = [skill.name for skill in load_skills(pinned_sources)]
    agent_skills = AgentSkills(skills=list(index.skills.values()))
    semantic = SemanticSkills(index, agent_skills, decider, preferences=preferences, notify=notify, pinned=pinned)
    injector = ContextInjector(lambda context: semantic.hint(context.agent), name="sierralab:skill-hint", trigger="userTurn")
    return [semantic, agent_skills, injector]
