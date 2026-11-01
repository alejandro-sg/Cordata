//! The learned dispatcher as the machine's [`Roster`]: ranks per role, learns from outcomes, and writes the decisions and
//! outcomes the dispatcher queues into the ledger's `events` table (what `dispatch.replay` judges a policy on).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cordata_ledger::Ledger;
use cordata_orchestrator::bandit::Bandit;
use cordata_orchestrator::dispatch::{Candidate, Dispatcher, LearnedDispatcher, Mode, Outcome, Spec, tier_rank};
use cordata_orchestrator::executor::Runnable;
use cordata_orchestrator::machine::Roster;
use cordata_orchestrator::worker::Worker;
use sha1::{Digest, Sha1};

/// `<home>/bandit/<sha1(realpath(project))[:16]>.json` — the table the Python dispatcher shares.
pub fn bandit_path(home: &Path, project: &Path) -> PathBuf {
    let real = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
    let digest = Sha1::digest(real.to_string_lossy().as_bytes());
    let key: String = digest.iter().map(|b| format!("{b:02x}")).collect::<String>().chars().take(16).collect();
    home.join("bandit").join(format!("{key}.json"))
}

pub struct LearnedRoster<'a> {
    pub dispatcher: LearnedDispatcher<'a>,
    workers: HashMap<String, Arc<dyn Worker>>,
    ledger: &'a Ledger,
}

impl<'a> LearnedRoster<'a> {
    /// `available` filters specs whose CLI is missing; `mode` is `off`, `shadow` or `live`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(specs: Vec<Spec>, table: Bandit, ledger: &'a Ledger, workers: HashMap<String, Arc<dyn Worker>>, mode: Mode, lam: f64, mu: f64, available: impl Fn(&Spec) -> bool + 'static) -> Self {
        let mut dispatcher = LearnedDispatcher::new(specs, table);
        dispatcher.ledger = Some(ledger);
        dispatcher.mode = mode;
        dispatcher.lam = lam;
        dispatcher.mu = mu;
        dispatcher.available = Box::new(available);
        Self { dispatcher, workers, ledger }
    }

    fn flush_events(&mut self) {
        for event in self.dispatcher.take_events() {
            let _ = self.ledger.append_event(&event.kind, &event.payload, None, Some(event.source));
        }
    }
}

impl Roster for LearnedRoster<'_> {
    fn rank(&mut self, role: &str, task_class: &str, exclude: &HashSet<String>) -> Vec<Runnable> {
        let ranked = Dispatcher::rank(&mut self.dispatcher, role, task_class, exclude);
        self.flush_events();
        ranked.into_iter().filter_map(|c| self.workers.get(&c.worker_id).cloned().map(|worker| Runnable { candidate: c, worker })).collect()
    }

    fn observe(&mut self, role: &str, task_class: &str, candidate: &Candidate, outcome: &Outcome) {
        Dispatcher::observe(&mut self.dispatcher, role, task_class, candidate, outcome);
        self.flush_events();
    }

    /// Candidate key → (tier rank, unit price): routes a step by its complexity.
    fn profile(&self) -> HashMap<String, (i64, f64)> {
        self.dispatcher.specs.iter().map(|s| (s.key(), (i64::from(tier_rank(&s.tier)), s.unit_price()))).collect()
    }
}
