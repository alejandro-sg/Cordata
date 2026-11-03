//! The dispatch policy: which worker (and model, tier, effort) takes
//! each role, cheapest competent first.
//!
//! ```text
//! Score(w) = Q̂(w, role, task_class) − λ·NormalizedCost(w) − μ·QuotaScarcity(w)
//! ```
//!
//! Hard gates filter (role served, not past the pool's hard line, harden excludes the executing model families) and
//! scores only *sort* what passed — nothing learned can add a candidate a gate removed. In `shadow` mode the learned
//! ranking is computed and recorded but the role's default order is returned; `live` returns the learned order with
//! ε-exploration on the top slot.
//!
//! Persistence of decisions and outcomes to `state.db`'s `events` table is the caller's: the dispatcher queues them
//! ([`LearnedDispatcher::take_events`]) rather than owning a second database layer.

use std::collections::{BTreeMap, HashMap, HashSet};

use cordata_ledger::Ledger;
use regex::Regex;
use serde_json::{Value, json};

use crate::bandit::Bandit;

pub const TIER_RANK: [(&str, i32); 3] = [("fast", 0), ("standard", 1), ("strong", 2)];
const MIN_TOLERANCE: f64 = 0.01;
const SHRINK: f64 = 20.0;

pub fn tier_rank(tier: &str) -> i32 {
    TIER_RANK.iter().find(|(t, _)| *t == tier).map_or(1, |(_, r)| *r)
}

fn tier_prior_q(tier: &str) -> f64 {
    match tier {
        "fast" => 0.55,
        "standard" => 0.7,
        "strong" => 0.8,
        _ => 0.6,
    }
}

fn tier_price(tier: &str) -> f64 {
    match tier {
        "fast" => 0.08,
        "standard" => 0.3,
        "strong" => 1.0,
        _ => 0.3,
    }
}

fn role_prior(role: &str) -> [&'static str; 3] {
    match role {
        "plan" | "research" | "conformance" => ["standard", "fast", "strong"],
        "audit" => ["standard", "strong", "fast"],
        _ => ["strong", "standard", "fast"], // execute, harden, and the default
    }
}

pub fn point_for_role(role: &str) -> &'static str {
    match role {
        "plan" | "research" | "conformance" => "planner_pool",
        "harden" | "audit" => "reviewer_pool",
        _ => "executor_pool",
    }
}

pub fn ctx(role: &str, task_class: &str, point: &str) -> String {
    format!("orch|{role}|{task_class}|{point}")
}

/// The task class a goal belongs to: a keyword reading, with `/large` for long multi-part goals. `complexity`
/// (a model's reading, when one is loaded) only adds the size suffix, so the class stays stable across model updates.
pub fn classify(goal: &str, complexity: Option<f64>) -> String {
    const CLASSES: [(&str, &str); 5] = [
        ("docs", r"\b(readme|docs?|documentation|docstring|comment|changelog|typo)\b"),
        ("test", r"\b(tests?|coverage|unit test|spec)\b"),
        ("bugfix", r"\b(fix|bug|error|crash|fail(s|ing|ure)?|broken|regression|exception)\b"),
        ("refactor", r"\b(refactor|rename|extract|move|split|clean ?up|simplify|dedupe)\b"),
        ("feature", r"\b(add|implement|create|support|build|introduce|new)\b"),
    ];
    let text = goal.to_lowercase();
    let kind = CLASSES.iter().find(|(_, p)| Regex::new(p).is_ok_and(|re| re.is_match(&text))).map_or("general", |(name, _)| name);
    let joiners = Regex::new(r"\b(and then|also|plus)\b").expect("static").find_iter(&text).count();
    let large = text.chars().count() > 600 || text.matches("\n- ").count() >= 4 || joiners >= 3 || complexity.is_some_and(|c| c >= 0.7);
    if large { format!("{kind}/large") } else { kind.to_owned() }
}

/// A runnable worker/model pair, as the policy sees it. (The worker itself lives with the executor.)
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub worker_id: String,
    /// Provider family (`claude_code`, `antigravity`, `ollama`) — what the quota pools match on.
    pub family: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The vendor behind the model; the harden role compares these.
    pub model_family: String,
}

impl Candidate {
    pub fn key(&self) -> String {
        // Effort is part of the arm: `opus@high` earns its own record apart from `opus@medium`.
        let effort = self.effort.as_deref().filter(|e| !e.is_empty()).map_or(String::new(), |e| format!("@{e}"));
        format!("{}:{}{effort}", self.worker_id, self.model.as_deref().unwrap_or("default"))
    }
}

#[derive(Debug, Clone)]
pub struct Spec {
    pub candidate: Candidate,
    pub roles: HashSet<String>,
    pub tier: String,
    /// Relative cost per task (strong = 1); defaults to the tier's.
    pub price: Option<f64>,
}

impl Spec {
    pub fn new(candidate: Candidate, tier: &str) -> Self {
        let roles = ["plan", "execute", "harden", "audit"].into_iter().map(String::from).collect();
        Self { candidate, roles, tier: tier.into(), price: None }
    }
    pub fn roles(mut self, roles: &[&str]) -> Self {
        self.roles = roles.iter().map(|r| (*r).to_owned()).collect();
        self
    }
    pub fn key(&self) -> String {
        self.candidate.key()
    }
    pub fn unit_price(&self) -> f64 {
        self.price.unwrap_or_else(|| tier_price(&self.tier))
    }
}

/// The one thing a state machine needs from a dispatcher.
pub trait Dispatcher {
    fn rank(&mut self, role: &str, task_class: &str, exclude_model_families: &HashSet<String>) -> Vec<Candidate>;
    fn observe(&mut self, role: &str, task_class: &str, candidate: &Candidate, outcome: &Outcome);
}

/// Fixed per-role order: for tests, and the baseline the learned policy is judged against.
#[derive(Default)]
pub struct StaticDispatcher {
    pub roles: HashMap<String, Vec<Candidate>>,
    pub observed: Vec<(String, String, String, Outcome)>,
}

impl Dispatcher for StaticDispatcher {
    fn rank(&mut self, role: &str, _task_class: &str, exclude: &HashSet<String>) -> Vec<Candidate> {
        self.roles.get(role).into_iter().flatten().filter(|c| !exclude.contains(&c.model_family)).cloned().collect()
    }
    fn observe(&mut self, role: &str, task_class: &str, candidate: &Candidate, outcome: &Outcome) {
        self.observed.push((role.into(), task_class.into(), candidate.key(), outcome.clone()));
    }
}

/// One run's verdict on a candidate. `reward` is the continuous per-step reward in [-1, 1] (`rlaif.py`).
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub ok: bool,
    pub norm_cost: Option<f64>,
    pub reward: Option<f64>,
    pub final_: bool,
    pub step_id: Option<String>,
    pub reward_version: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Shadow,
    Live,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Live => "live",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub point: &'static str,
    pub context: String,
    pub default: String,
    pub arm: String,
    pub acted: bool,
    pub mode: &'static str,
    pub propensity: f64,
    pub scores: BTreeMap<String, f64>,
}

impl Decision {
    fn to_json(&self) -> Value {
        json!({"point": self.point, "context": self.context, "default": self.default, "arm": self.arm, "acted": self.acted,
               "mode": self.mode, "propensity": self.propensity, "scores": self.scores})
    }
}

/// An event for the caller to append to `state.db`'s `events` table.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub kind: String,
    pub payload: Value,
    pub source: &'static str,
}

fn round(x: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (x * scale).round() / scale
}

/// A small non-cryptographic generator (splitmix64) for the exploration slot; tests inject their own.
fn default_rng() -> Box<dyn FnMut() -> f64> {
    let mut state = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0x9e37_79b9, |d| d.as_nanos() as u64);
    Box::new(move || {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    })
}

pub struct LearnedDispatcher<'a> {
    pub specs: Vec<Spec>,
    pub ledger: Option<&'a Ledger>,
    pub mode: Mode,
    pub lam: f64,
    pub mu: f64,
    pub explore: f64,
    pub min_pulls: u64,
    pub table: Bandit,
    /// Interior-mutable so ranking can draw while it holds borrowed specs.
    pub rng: std::cell::RefCell<Box<dyn FnMut() -> f64>>,
    pub available: Box<dyn Fn(&Spec) -> bool>,
    /// Extra utility a subclass adds to a candidate (the Auto Mode router). Zero by default.
    #[allow(clippy::type_complexity)]
    pub adjust: Box<dyn Fn(&Spec, &str, &str) -> f64>,
    pub decisions: Vec<Decision>,
    /// Write outcomes to the table's file and queue ledger events (off for offline training).
    pub log_outcomes: bool,
    /// How many standard errors below the best arm still counts as "as good".
    pub z: f64,
    last: HashMap<(String, String), Decision>,
    events: Vec<Event>,
}

impl<'a> LearnedDispatcher<'a> {
    pub fn new(specs: Vec<Spec>, table: Bandit) -> Self {
        Self {
            specs,
            ledger: None,
            mode: Mode::Shadow,
            lam: 0.3,
            mu: 0.5,
            explore: 0.1,
            min_pulls: 3,
            table,
            rng: std::cell::RefCell::new(default_rng()),
            available: Box::new(|_| true),
            adjust: Box::new(|_, _, _| 0.0),
            decisions: Vec::new(),
            log_outcomes: true,
            z: 1.5,
            last: HashMap::new(),
            events: Vec::new(),
        }
    }

    /// Events queued since the last call, for the caller to append to the ledger.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    // ------------------------------------------------------------------ gates and the default

    pub fn eligible(&self, role: &str, exclude: &HashSet<String>) -> Vec<&Spec> {
        let gate_role = if matches!(role, "research" | "conformance") { "plan" } else { role };
        self.specs
            .iter()
            .filter(|s| {
                (s.roles.contains(gate_role) || s.roles.contains(role))
                    && !exclude.contains(&s.candidate.model_family)
                    && (self.available)(s)
                    && self.ledger.is_none_or(|l| !matches!(l.level(&s.candidate.family, s.candidate.model.as_deref()), "hard" | "exhausted"))
            })
            .collect()
    }

    /// Strongest first; among equals, the cheaper one.
    pub fn default_order<'s>(specs: &[&'s Spec]) -> Vec<&'s Spec> {
        let mut out = specs.to_vec();
        out.sort_by(|a, b| tier_rank(&b.tier).cmp(&tier_rank(&a.tier)).then(a.unit_price().total_cmp(&b.unit_price())));
        out
    }

    /// Role-prior default order: cheapest competent tier by role, ties broken by unit price then key.
    pub fn role_order<'s>(specs: &[&'s Spec], role: &str) -> Vec<&'s Spec> {
        let prior = role_prior(role);
        let rank = |tier: &str| prior.iter().position(|t| *t == tier).unwrap_or(prior.len());
        let mut out = specs.to_vec();
        out.sort_by(|a, b| rank(&a.tier).cmp(&rank(&b.tier)).then(a.unit_price().total_cmp(&b.unit_price())).then(a.key().cmp(&b.key())));
        out
    }

    // ------------------------------------------------------------------ scoring

    /// What a candidate's tier (and effort, when it names one) has earned in this context, else the tier's prior.
    pub fn prior(&self, spec: &Spec, role: &str, task_class: &str) -> f64 {
        let mut points: Vec<(&str, &str)> = vec![("tier", &spec.tier)];
        if let Some(effort) = spec.candidate.effort.as_deref().filter(|e| !e.is_empty()) {
            points.push(("effort", effort));
        }
        let borrowed: Vec<f64> = points
            .into_iter()
            .filter_map(|(point, arm)| {
                let stat = self.table.stat(&ctx(role, task_class, point), arm);
                (stat.n >= self.min_pulls).then(|| (stat.wins + 1.0) / (stat.n as f64 + 2.0))
            })
            .collect();
        if borrowed.is_empty() { tier_prior_q(&spec.tier) } else { borrowed.iter().sum::<f64>() / borrowed.len() as f64 }
    }

    fn own(&self, spec: &Spec, role: &str, task_class: &str) -> crate::bandit::Stat {
        self.table.stat(&ctx(role, task_class, point_for_role(role)), &spec.key())
    }

    /// Q̂: the candidate's own win rate shrunk toward its tier's (`SHRINK` pseudo-outcomes).
    pub fn quality(&self, spec: &Spec, role: &str, task_class: &str) -> f64 {
        let own = self.own(spec, role, task_class);
        (own.wins + SHRINK * self.prior(spec, role, task_class)) / (own.n as f64 + SHRINK)
    }

    pub fn cost(&self, spec: &Spec, role: &str, task_class: &str) -> f64 {
        let own = self.own(spec, role, task_class);
        let prior = spec.unit_price().min(1.0);
        if own.n == 0 {
            return prior;
        }
        (own.cost * own.n as f64 + prior * 2.0) / (own.n as f64 + 2.0)
    }

    pub fn scarcity(&self, spec: &Spec) -> f64 {
        let Some(ledger) = self.ledger else { return 0.0 };
        let pool = ledger.pool_for(&spec.candidate.family, spec.candidate.model.as_deref());
        ledger.state(&pool).pct.clamp(0.0, 1.0)
    }

    pub fn score(&self, spec: &Spec, role: &str, task_class: &str) -> f64 {
        self.quality(spec, role, task_class) - self.lam * self.cost(spec, role, task_class) - self.mu * self.scarcity(spec) + (self.adjust)(spec, role, task_class)
    }

    fn tier_stat(&self, tier: &str, role: &str, task_class: &str) -> (f64, u64) {
        let stat = self.table.stat(&ctx(role, task_class, "tier"), tier);
        if stat.n < self.min_pulls { (tier_prior_q(tier), 0) } else { ((stat.wins + 1.0) / (stat.n as f64 + 2.0), stat.n) }
    }

    /// The plan's rule — *the cheapest worker whose expected quality satisfies the threshold* — decided where the
    /// evidence is: first between tiers (every arm pooled), then between the arms of the competent tiers.
    pub fn cheapest_competent<'s>(&self, specs: &[&'s Spec], role: &str, task_class: &str, scored: &BTreeMap<String, f64>) -> Vec<&'s Spec> {
        let mut tiers: Vec<&str> = specs.iter().map(|s| s.tier.as_str()).collect::<HashSet<_>>().into_iter().collect();
        tiers.sort_by_key(|t| std::cmp::Reverse(tier_rank(t)));
        let stats: HashMap<&str, (f64, u64)> = tiers.iter().map(|t| (*t, self.tier_stat(t, role, task_class))).collect();
        let best = tiers.iter().copied().max_by(|a, b| stats[a].0.total_cmp(&stats[b].0).then(tier_rank(a).cmp(&tier_rank(b)))).expect("specs is non-empty");
        let (q_best, n_best) = stats[best];
        let mut competent_tiers: HashSet<&str> = HashSet::new();
        for tier in &tiers {
            let (q, n) = stats[tier];
            let tolerance = if n_best < self.min_pulls || n < self.min_pulls {
                MIN_TOLERANCE
            } else {
                let se = (q_best * (1.0 - q_best) / (n_best as f64 + 2.0) + q * (1.0 - q) / (n as f64 + 2.0)).sqrt();
                MIN_TOLERANCE.max(self.z * se)
            };
            if q >= q_best - tolerance {
                competent_tiers.insert(tier);
            }
        }
        let mut competent: Vec<&Spec> = Vec::new();
        for spec in specs {
            if !competent_tiers.contains(spec.tier.as_str()) {
                continue;
            }
            let own = self.own(spec, role, task_class);
            let q_tier = stats[spec.tier.as_str()].0;
            if own.n >= self.min_pulls {
                let q_own = (own.wins + 1.0) / (own.n as f64 + 2.0);
                if q_own < q_tier - MIN_TOLERANCE.max(2.0 * (q_tier * (1.0 - q_tier) / (own.n as f64 + 2.0)).sqrt()) {
                    continue; // significantly worse than its tier
                }
            }
            competent.push(spec);
        }
        let chosen: HashSet<String> = competent.iter().map(|s| s.key()).collect();
        let mut rest: Vec<&Spec> = specs.iter().copied().filter(|s| !chosen.contains(&s.key())).collect();
        let key = |s: &Spec| (self.cost(s, role, task_class) + self.mu * self.scarcity(s) - (self.adjust)(s, role, task_class), -self.quality(s, role, task_class));
        competent.sort_by(|a, b| {
            let (ka, kb) = (key(a), key(b));
            ka.0.total_cmp(&kb.0).then(ka.1.total_cmp(&kb.1))
        });
        rest.sort_by(|a, b| scored[&b.key()].total_cmp(&scored[&a.key()]).then(tier_rank(&b.tier).cmp(&tier_rank(&a.tier))));
        competent.extend(rest);
        competent
    }

    // ------------------------------------------------------------------ the policy

    pub fn rank(&mut self, role: &str, task_class: &str, exclude: &HashSet<String>) -> Vec<Candidate> {
        let eligible = self.eligible(role, exclude);
        let default = Self::role_order(&eligible, role);
        if !matches!(self.mode, Mode::Shadow | Mode::Live) || eligible.is_empty() {
            return default.iter().map(|s| s.candidate.clone()).collect();
        }
        let scored: BTreeMap<String, f64> = eligible.iter().map(|s| (s.key(), round(self.score(s, role, task_class), 4))).collect();
        let mut learned = self.cheapest_competent(&eligible, role, task_class, &scored);
        let acted = self.mode == Mode::Live;
        let n = eligible.len();
        let default_key = default[0].key();
        if acted && learned.len() > 1 && self.explore > 0.0 && (self.rng.borrow_mut())() < self.explore {
            let pick = 1 + ((self.rng.borrow_mut())() * (learned.len() - 1) as f64) as usize;
            let chosen = learned.remove(pick.min(learned.len() - 1));
            learned.insert(0, chosen);
        }
        let learned_key = learned[0].key();
        let chosen: Vec<Candidate> = if acted { learned.iter().map(|s| s.candidate.clone()).collect() } else { default.iter().map(|s| s.candidate.clone()).collect() };
        let point = point_for_role(role);
        let decision = Decision {
            point,
            context: ctx(role, task_class, point),
            default: default_key.clone(),
            arm: learned_key.clone(),
            acted: acted && learned_key != default_key,
            mode: self.mode.as_str(),
            propensity: if acted { round((1.0 - self.explore) + self.explore / n.max(1) as f64, 4) } else { 1.0 },
            scores: scored,
        };
        if self.log_outcomes {
            let mut payload = decision.to_json();
            payload["action"] = json!("decide");
            payload["turn"] = json!(format!("orch:{role}:{task_class}"));
            self.events.push(Event { kind: "bandit".into(), payload, source: "bandit" });
        }
        self.last.insert((role.into(), task_class.into()), decision.clone());
        self.decisions.push(decision);
        chosen
    }

    /// Folds one outcome in. A `final` outcome (the run's verdict) is also queued as an `orch_outcome` event — the
    /// rows [`replay`] judges a policy on.
    pub fn observe(&mut self, role: &str, task_class: &str, candidate: &Candidate, outcome: &Outcome) {
        let Some(spec) = self.specs.iter().find(|s| s.key() == candidate.key()).cloned() else { return };
        let cost = outcome.norm_cost.unwrap_or_else(|| spec.unit_price());
        // A continuous reward in [-1, 1] maps onto the [0, 1] scale the binary outcome used; `win` stays binary.
        let quality = outcome.reward.map_or(if outcome.ok { 1.0 } else { 0.0 }, |r| (r.clamp(-1.0, 1.0) + 1.0) / 2.0);
        let value = quality - self.lam * cost;
        let effort = candidate.effort.clone().unwrap_or_else(|| "default".into());
        for (point, arm) in [(point_for_role(role), spec.key()), ("tier", spec.tier.clone()), ("effort", effort)] {
            self.table.observe_arm(&ctx(role, task_class, point), &arm, value, outcome.ok, cost);
        }
        if self.log_outcomes {
            let _ = self.table.save(); // offline training keeps its table in memory
        }
        if outcome.final_ && self.log_outcomes {
            let decision = self.last.get(&(role.to_owned(), task_class.to_owned()));
            let mut payload = json!({
                "role": role, "task_class": task_class, "arm": spec.key(), "tier": spec.tier, "effort": candidate.effort,
                "ok": outcome.ok, "cost": round(cost, 4), "mode": self.mode.as_str(),
                "propensity": decision.map(|d| d.propensity), "default": decision.map(|d| d.default.clone()),
            });
            if let Some(reward) = outcome.reward {
                payload["reward"] = json!(round(reward, 4));
                payload["step_id"] = json!(outcome.step_id);
                payload["reward_version"] = outcome.reward_version.clone().unwrap_or(Value::Null);
            }
            self.events.push(Event { kind: "orch_outcome".into(), payload, source: "orchestrator" });
        }
    }
}

impl Dispatcher for LearnedDispatcher<'_> {
    fn rank(&mut self, role: &str, task_class: &str, exclude: &HashSet<String>) -> Vec<Candidate> {
        LearnedDispatcher::rank(self, role, task_class, exclude)
    }
    fn observe(&mut self, role: &str, task_class: &str, candidate: &Candidate, outcome: &Outcome) {
        LearnedDispatcher::observe(self, role, task_class, candidate, outcome);
    }
}

// ---------------------------------------------------------------------- offline judging

/// One logged decision with its outcome: what the replay estimator reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Logged {
    pub role: String,
    pub task_class: String,
    pub arm: String,
    pub ok: bool,
    pub cost: f64,
}

/// Rejection-sampling replay: the value of `choose(role, task_class, arms) -> arm` on uniformly explored logs.
/// Returns `{"matched", "pass_rate", "cost_per_task", "cost_per_success"}` (null where undefined).
pub fn replay(logs: &[Logged], choose: &mut dyn FnMut(&str, &str, &[String]) -> String, arms: &dyn Fn(&str) -> Vec<String>) -> Value {
    let (mut matched, mut wins, mut cost) = (0u64, 0u64, 0.0);
    for row in logs {
        if choose(&row.role, &row.task_class, &arms(&row.role)) != row.arm {
            continue;
        }
        matched += 1;
        wins += u64::from(row.ok);
        cost += row.cost;
    }
    json!({
        "matched": matched,
        "pass_rate": (matched > 0).then(|| round(wins as f64 / matched as f64, 4)),
        "cost_per_task": (matched > 0).then(|| round(cost / matched as f64, 4)),
        "cost_per_success": (wins > 0).then(|| round(cost / wins as f64, 4)),
    })
}

/// Trains a learned policy on the first part of `logs` and judges it, and the always-strongest default, by replay on
/// the rest. Offline: an in-memory table, nothing queued for the ledger.
pub fn evaluate(logs: &[Logged], specs: &[Spec], train_fraction: f64, lam: f64, min_pulls: u64, z: f64) -> Value {
    let cut = (logs.len() as f64 * train_fraction) as usize;
    let (train, test) = logs.split_at(cut);
    let mut learner = LearnedDispatcher::new(specs.to_vec(), Bandit::in_memory());
    (learner.mode, learner.lam, learner.mu, learner.explore, learner.min_pulls, learner.log_outcomes, learner.z) = (Mode::Live, lam, 0.0, 0.0, min_pulls, false, z);
    for row in train {
        if let Some(spec) = specs.iter().find(|s| s.key() == row.arm) {
            learner.observe(&row.role, &row.task_class, &spec.candidate.clone(), &Outcome { ok: row.ok, norm_cost: Some(row.cost), ..Default::default() });
        }
    }
    let arms = |role: &str| specs.iter().filter(|s| s.roles.contains(role)).map(Spec::key).collect::<Vec<_>>();
    let none = HashSet::new();
    let mut learned = |role: &str, task_class: &str, _: &[String]| learner.rank(role, task_class, &none).first().map(|c| c.key()).unwrap_or_default();
    let mut strongest = |role: &str, _: &str, _: &[String]| {
        let pool: Vec<&Spec> = specs.iter().filter(|s| s.roles.contains(role)).collect();
        LearnedDispatcher::default_order(&pool).first().map(|s| s.key()).unwrap_or_default()
    };
    json!({"train": train.len(), "test": test.len(), "learned": replay(test, &mut learned, &arms), "strongest": replay(test, &mut strongest, &arms)})
}

/// Outcomes the `auto` dispatch mode wants before it judges the learned policy at all.
pub const GRADUATE_AFTER: usize = 30;

/// The `auto` dispatch mode: `Live` once the learned policy, trained on the older 70% of `logs` (oldest first) and
/// replayed on the rest, passes as often as the always-strongest default (within 2 points) at no more cost per
/// success, on at least 5 matched decisions; `Shadow` (still learning, nothing changes) otherwise. The note says why.
pub fn graduate(logs: &[Logged], specs: &[Spec], lam: f64) -> (Mode, String) {
    if logs.len() < GRADUATE_AFTER {
        return (Mode::Shadow, format!("routing is learning ({}/{GRADUATE_AFTER} outcomes); the default order decides", logs.len()));
    }
    let report = evaluate(logs, specs, 0.7, lam, 3, 1.5);
    let num = |side: &str, key: &str| report[side][key].as_f64();
    let matched = report["learned"]["matched"].as_u64().unwrap_or(0);
    let (lp, sp) = (num("learned", "pass_rate"), num("strongest", "pass_rate"));
    let (lc, sc) = (num("learned", "cost_per_success"), num("strongest", "cost_per_success"));
    let good = matched >= 5
        && match (lp, sp) {
            (Some(l), Some(s)) => l + 0.02 >= s && lc.zip(sc).is_none_or(|(l, s)| l <= s),
            (Some(_), None) => true,
            _ => false,
        };
    let pct = |p: Option<f64>| p.map_or("—".to_owned(), |p| format!("{:.0}%", p * 100.0));
    let money = |c: Option<f64>| c.map_or("—".to_owned(), |c| format!("{c:.2}"));
    let summary = format!("learned {} pass at {} per success vs default {} at {}, {matched} matched", pct(lp), money(lc), pct(sp), money(sc));
    if good { (Mode::Live, format!("routing learned from {} outcomes and now decides ({summary})", logs.len())) } else { (Mode::Shadow, format!("routing has not yet beaten the default order ({summary}); it keeps learning")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_mode_goes_live_only_when_the_learned_policy_holds_up_on_replay() {
        let few: Vec<Logged> = (0..10).map(|_| Logged { role: "execute".into(), task_class: "g".into(), arm: "cc:opus".into(), ok: true, cost: 1.0 }).collect();
        assert_eq!(graduate(&few, &specs(), 0.3).0, Mode::Shadow, "too little evidence");
        // haiku passes as often as opus at a tenth of the cost: the learner should take over.
        let mut logs = Vec::new();
        for i in 0..120 {
            let (arm, cost) = [("cc:opus", 1.0), ("cc:haiku", 0.1), ("agy:gemini", 0.3)][i % 3];
            logs.push(Logged { role: "execute".into(), task_class: "g".into(), arm: arm.into(), ok: arm != "agy:gemini" || i % 2 == 0, cost });
        }
        let (mode, note) = graduate(&logs, &specs(), 0.3);
        assert_eq!(mode, Mode::Live, "{note}");
        // Drift: haiku did fine in the older outcomes and fails in the recent ones. Replay on the recent ones catches
        // it, and the default keeps deciding.
        let cut = logs.len() * 7 / 10;
        let drifted: Vec<Logged> = logs.iter().cloned().enumerate().map(|(i, mut r)| { if i >= cut && r.arm == "cc:haiku" { r.ok = false } r }).collect();
        let (mode, note) = graduate(&drifted, &specs(), 0.3);
        assert_eq!(mode, Mode::Shadow, "{note}");
    }

    fn cand(worker: &str, model: &str, family: &str, vendor: &str) -> Candidate {
        Candidate { worker_id: worker.into(), family: family.into(), model: Some(model.into()), effort: None, model_family: vendor.into() }
    }

    fn specs() -> Vec<Spec> {
        vec![
            Spec::new(cand("cc", "opus", "claude_code", "anthropic"), "strong"),
            Spec::new(cand("cc", "haiku", "claude_code", "anthropic"), "fast"),
            Spec::new(cand("agy", "gemini", "antigravity", "google"), "standard"),
        ]
    }

    fn keys(cs: &[Candidate]) -> Vec<String> {
        cs.iter().map(Candidate::key).collect()
    }

    fn live(table: Bandit) -> LearnedDispatcher<'static> {
        let mut d = LearnedDispatcher::new(specs(), table);
        (d.mode, d.explore, d.log_outcomes) = (Mode::Live, 0.0, false);
        d
    }

    #[test]
    fn classify_reads_keywords_and_size() {
        assert_eq!(classify("Fix the crash in the parser", None), "bugfix");
        assert_eq!(classify("update the README", None), "docs");
        assert_eq!(classify("add unit tests", None), "test");
        assert_eq!(classify("rename the module", None), "refactor");
        assert_eq!(classify("implement caching", None), "feature");
        assert_eq!(classify("hello", None), "general");
        assert_eq!(classify("fix it", Some(0.8)), "bugfix/large");
        assert_eq!(classify("fix a, and then b, also c, plus d", None), "bugfix/large");
        assert_eq!(classify(&"x".repeat(601), None), "general/large");
        assert_eq!(classify("fix\n- a\n- b\n- c\n- d", None), "bugfix/large");
    }

    #[test]
    fn role_order_follows_the_role_prior() {
        let all = specs();
        let refs: Vec<&Spec> = all.iter().collect();
        let plan: Vec<String> = LearnedDispatcher::role_order(&refs, "plan").iter().map(|s| s.key()).collect();
        assert_eq!(plan, ["agy:gemini", "cc:haiku", "cc:opus"]);
        let exec: Vec<String> = LearnedDispatcher::role_order(&refs, "execute").iter().map(|s| s.key()).collect();
        assert_eq!(exec, ["cc:opus", "agy:gemini", "cc:haiku"]);
        let strongest: Vec<String> = LearnedDispatcher::default_order(&refs).iter().map(|s| s.key()).collect();
        assert_eq!(strongest[0], "cc:opus");
    }

    #[test]
    fn gates_filter_roles_families_and_availability() {
        let mut d = live(Bandit::in_memory());
        d.specs[1] = d.specs[1].clone().roles(&["plan"]);
        let none = HashSet::new();
        assert_eq!(d.eligible("execute", &none).len(), 2);
        assert_eq!(d.eligible("research", &none).len(), 3, "research is gated as plan");
        let anthropic: HashSet<String> = ["anthropic".to_string()].into();
        assert_eq!(keys(&d.rank("harden", "general", &anthropic)), ["agy:gemini"], "harden excludes the executing family");
        d.available = Box::new(|s| s.candidate.worker_id != "agy");
        assert_eq!(d.eligible("execute", &none).len(), 1);
    }

    #[test]
    fn a_ledger_past_its_hard_line_removes_the_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let pools = vec![cordata_ledger::Pool::new("cc", "claude_code").limit(100.0)];
        let ledger = Ledger::open(&dir.path().join("s.db"), pools).unwrap();
        let spent = cordata_ledger::Outcome { tokens_in: 96, ..Default::default() };
        ledger.record_result("r", None, "claude_code", Some("opus"), &spent, None, None);
        let mut d = live(Bandit::in_memory());
        d.ledger = Some(&ledger);
        assert_eq!(keys(&d.rank("execute", "general", &HashSet::new())), ["agy:gemini"]);
    }

    #[test]
    fn with_no_evidence_the_strongest_tier_wins_and_shadow_returns_the_default() {
        let none = HashSet::new();
        assert_eq!(keys(&live(Bandit::in_memory()).rank("execute", "general", &none))[0], "cc:opus");
        let mut shadow = live(Bandit::in_memory());
        shadow.mode = Mode::Shadow;
        let out = shadow.rank("execute", "general", &none);
        assert_eq!(keys(&out), ["cc:opus", "agy:gemini", "cc:haiku"]);
        assert_eq!(shadow.decisions.len(), 1);
        assert!(!shadow.decisions[0].acted && shadow.decisions[0].propensity == 1.0);
    }

    #[test]
    fn evidence_that_a_cheap_tier_is_as_good_moves_it_first() {
        let mut d = live(Bandit::in_memory());
        let none = HashSet::new();
        let fast = d.specs[1].candidate.clone();
        let strong = d.specs[0].candidate.clone();
        for _ in 0..20 {
            d.observe("execute", "bugfix", &fast, &Outcome { ok: true, ..Default::default() });
            d.observe("execute", "bugfix", &strong, &Outcome { ok: true, ..Default::default() });
        }
        assert_eq!(keys(&d.rank("execute", "bugfix", &none))[0], "cc:haiku", "equally good and cheaper goes first");
        // …and a cheap tier that keeps failing does not.
        let mut d = live(Bandit::in_memory());
        for _ in 0..20 {
            d.observe("execute", "bugfix", &fast, &Outcome { ok: false, ..Default::default() });
            d.observe("execute", "bugfix", &strong, &Outcome { ok: true, ..Default::default() });
        }
        assert_eq!(keys(&d.rank("execute", "bugfix", &none))[0], "cc:opus");
    }

    #[test]
    fn exploration_moves_a_lower_slot_to_the_front() {
        let mut d = live(Bandit::in_memory());
        d.explore = 1.0;
        let mut calls = [0.0, 0.0].into_iter();
        d.rng = std::cell::RefCell::new(Box::new(move || calls.next().unwrap_or(0.0)));
        let out = d.rank("execute", "general", &HashSet::new());
        assert_ne!(out[0].key(), "cc:opus", "explored away from the learned top");
        assert_eq!(out.len(), 3);
        assert!(d.decisions[0].acted);
        assert_eq!(d.decisions[0].propensity, round(0.0 + 1.0 / 3.0, 4));
    }

    #[test]
    fn final_outcomes_queue_ledger_events_and_save_the_table() {
        let dir = tempfile::tempdir().unwrap();
        let table_path = dir.path().join("t.json");
        let mut d = LearnedDispatcher::new(specs(), Bandit::open(&table_path));
        d.mode = Mode::Live;
        d.explore = 0.0;
        let c = d.rank("execute", "bugfix", &HashSet::new())[0].clone();
        d.observe("execute", "bugfix", &c, &Outcome { ok: true, reward: Some(0.5), final_: true, step_id: Some("s1".into()), ..Default::default() });
        let events = d.take_events();
        assert_eq!(events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), ["bandit", "orch_outcome"]);
        let outcome = &events[1].payload;
        assert_eq!((outcome["ok"].clone(), outcome["reward"].clone(), outcome["step_id"].clone()), (json!(true), json!(0.5), json!("s1")));
        assert_eq!(outcome["default"], "cc:opus");
        assert!(d.take_events().is_empty());
        assert_eq!(Bandit::open(&table_path).stat("orch|execute|bugfix|executor_pool", "cc:opus").n, 1);
    }

    #[test]
    fn a_continuous_reward_scales_into_the_table_but_win_stays_binary() {
        let mut d = live(Bandit::in_memory());
        let c = d.specs[0].candidate.clone();
        d.observe("execute", "g", &c, &Outcome { ok: false, reward: Some(1.0), norm_cost: Some(0.0), ..Default::default() });
        let s = d.table.stat("orch|execute|g|executor_pool", "cc:opus");
        assert_eq!((s.wins, s.mean), (0.0, 1.0));
    }

    #[test]
    fn replay_counts_only_matching_decisions() {
        let logs = vec![
            Logged { role: "execute".into(), task_class: "g".into(), arm: "a".into(), ok: true, cost: 1.0 },
            Logged { role: "execute".into(), task_class: "g".into(), arm: "b".into(), ok: false, cost: 0.1 },
            Logged { role: "execute".into(), task_class: "g".into(), arm: "a".into(), ok: false, cost: 3.0 },
        ];
        let arms = |_: &str| vec!["a".to_string(), "b".to_string()];
        let r = replay(&logs, &mut |_, _, _| "a".into(), &arms);
        assert_eq!(r, json!({"matched": 2, "pass_rate": 0.5, "cost_per_task": 2.0, "cost_per_success": 4.0}));
        let none = replay(&logs, &mut |_, _, _| "zzz".into(), &arms);
        assert_eq!(none, json!({"matched": 0, "pass_rate": null, "cost_per_task": null, "cost_per_success": null}));
    }

    #[test]
    fn evaluate_trains_on_the_first_half() {
        let mut logs = Vec::new();
        for i in 0..40 {
            for (arm, ok, cost) in [("cc:opus", true, 1.0), ("cc:haiku", true, 0.08), ("agy:gemini", i % 2 == 0, 0.3)] {
                logs.push(Logged { role: "execute".into(), task_class: "bugfix".into(), arm: arm.into(), ok, cost });
            }
        }
        let out = evaluate(&logs, &specs(), 0.5, 0.3, 3, 1.5);
        assert_eq!((out["train"].clone(), out["test"].clone()), (json!(60), json!(60)));
        assert!(out["learned"]["cost_per_task"].as_f64().unwrap() < out["strongest"]["cost_per_task"].as_f64().unwrap(), "{out}");
    }
}
