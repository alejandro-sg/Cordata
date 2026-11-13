//! The quota ledger: how much of each provider pool is left,
//! metered locally and corrected by the provider's own readings. Same `~/.cordata/state.db`, same tables, same
//! arithmetic — the Python backend and this crate read and write one ledger, which `tests/python_parity.rs` checks.
//!
//! Two envelopes: the per-plan spend cap ([`Ledger::envelope`]) and rolling pool windows ([`Ledger::state`]) with a
//! soft line (finish the step, then move on) and a hard line (stop the step where it is).
//!
//! Like the original it never panics into a run: a ledger that cannot be read reports its pool as `exhausted`,
//! which a dispatcher treats as "do not prefer".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const FIVE_HOURS: i64 = 5 * 3600;
pub const WEEK: f64 = 7.0 * 24.0 * 3600.0;
const BUSY_TIMEOUT_MS: u64 = 5000;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ledger database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("ledger file: {0}")]
    Io(#[from] std::io::Error),
    #[error("pool {0:?}: unit must be usd, tokens or requests")]
    Unit(String),
    #[error("pool {pool:?}: bad model regex: {source}")]
    Regex { pool: String, source: regex::Error },
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Unit {
    Usd,
    Tokens,
    Requests,
}

impl Unit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usd => "usd",
            Self::Tokens => "tokens",
            Self::Requests => "requests",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pool {
    pub pool_id: String,
    pub family: String,
    #[serde(default = "any")]
    pub model_regex: String,
    #[serde(default = "tokens")]
    pub unit: Unit,
    /// `None` is uncapped.
    #[serde(default)]
    pub window_limit: Option<f64>,
    #[serde(default = "five_hours")]
    pub window_seconds: i64,
    #[serde(default = "soft")]
    pub soft_threshold_pct: f64,
    #[serde(default = "hard")]
    pub hard_threshold_pct: f64,
    #[serde(default)]
    pub weekly_limit: Option<f64>,
}

fn any() -> String {
    ".*".into()
}
fn tokens() -> Unit {
    Unit::Tokens
}
fn five_hours() -> i64 {
    FIVE_HOURS
}
fn soft() -> f64 {
    0.80
}
fn hard() -> f64 {
    0.95
}

impl Pool {
    pub fn new(pool_id: &str, family: &str) -> Self {
        Self {
            pool_id: pool_id.into(),
            family: family.into(),
            model_regex: any(),
            unit: Unit::Tokens,
            window_limit: None,
            window_seconds: FIVE_HOURS,
            soft_threshold_pct: 0.80,
            hard_threshold_pct: 0.95,
            weekly_limit: None,
        }
    }

    pub fn model(mut self, regex: &str) -> Self {
        self.model_regex = regex.into();
        self
    }
    pub fn unit(mut self, unit: Unit) -> Self {
        self.unit = unit;
        self
    }
    pub fn limit(mut self, limit: f64) -> Self {
        self.window_limit = Some(limit);
        self
    }
    pub fn window(mut self, seconds: i64) -> Self {
        self.window_seconds = seconds;
        self
    }
    pub fn weekly(mut self, limit: f64) -> Self {
        self.weekly_limit = Some(limit);
        self
    }

    /// Family equal and the model regex matching the *whole* model name (Python's `re.fullmatch`).
    pub fn matches(&self, family: &str, model: Option<&str>) -> bool {
        self.family == family
            && Regex::new(&format!("^(?:{})$", self.model_regex)).is_ok_and(|re| re.is_match(model.unwrap_or("")))
    }
}

/// Conservative defaults, identical to the Python module's.
pub fn default_pools() -> Vec<Pool> {
    vec![
        Pool::new("claude_code", "claude_code").unit(Unit::Usd).limit(20.0).weekly(150.0),
        Pool::new("antigravity_claude", "antigravity").model("claude-.*").limit(3_000_000.0),
        Pool::new("antigravity", "antigravity").limit(8_000_000.0),
        Pool::new("ollama_local", "ollama").model(r".*:local|local/.*").unit(Unit::Requests),
        Pool::new("ollama_cloud", "ollama").unit(Unit::Requests).limit(600.0).window(3600),
    ]
}

/// `orchestrator.pools` from settings (a JSON list, or a string holding one). A malformed entry is skipped — never
/// a reason to run with *no* caps.
pub fn pools_from_settings(raw: Option<&Value>) -> Vec<Pool> {
    let parsed;
    let list = match raw {
        Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
            Ok(value) => {
                parsed = value;
                &parsed
            }
            Err(_) => return default_pools(),
        },
        Some(value) => value,
        None => return default_pools(),
    };
    let pools: Vec<Pool> = list
        .as_array()
        // A pool whose model regex does not compile would match nothing: drop it, so its models keep a default cap.
        .map(|items| items.iter().filter_map(|item| serde_json::from_value::<Pool>(item.clone()).ok()).filter(|p| Regex::new(&p.model_regex).is_ok()).collect())
        .unwrap_or_default();
    if pools.is_empty() { default_pools() } else { pools }
}

/// The `orchestrator.pools` entries [`pools_from_settings`] drops because their `model_regex` does not compile.
impl Drop for Ledger {
    /// One last try at the spend the database refused, so it is not lost with the process.
    fn drop(&mut self) {
        let rows = std::mem::take(&mut *self.unrecorded.lock().unwrap_or_else(|e| e.into_inner()));
        for row in rows {
            let _ = self.record(&row);
        }
    }
}

pub fn bad_pool_regexes(raw: Option<&Value>) -> Vec<String> {
    let parsed = match raw {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text).unwrap_or(Value::Null),
        Some(v) => v.clone(),
        None => Value::Null,
    };
    parsed.as_array().into_iter().flatten().filter_map(|item| serde_json::from_value::<Pool>(item.clone()).ok())
        .filter(|p| Regex::new(&p.model_regex).is_err()).map(|p| format!("{} (model_regex `{}`)", p.pool_id, p.model_regex)).collect()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PoolState {
    pub pool_id: String,
    pub family: String,
    pub unit: Unit,
    pub used: f64,
    pub limit: Option<f64>,
    pub pct: f64,
    /// `ok` | `soft` | `hard` | `exhausted`
    pub level: &'static str,
    pub window_seconds: i64,
    pub weekly_used: f64,
    pub weekly_limit: Option<f64>,
    pub limited_until: Option<f64>,
    /// How much of `unit` is left before the hard line.
    pub to_hard: Option<f64>,
    pub to_soft: Option<f64>,
    pub provider_pct: Option<f64>,
    /// When the pool next frees up (epoch seconds): a limit's end, the provider's reset, or the oldest metered use
    /// leaving the window. `None` with nothing to free.
    pub resets_at: Option<f64>,
    /// `meter` | `provider`
    pub source: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EnvelopeState {
    pub session_id: String,
    pub spent_usd: f64,
    pub spent_tokens: i64,
    pub max_usd: Option<f64>,
    pub max_tokens: Option<i64>,
    pub exhausted: bool,
    pub remaining_usd: Option<f64>,
    pub remaining_tokens: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageRow {
    pub session_id: String,
    pub pool_id: String,
    pub model: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cost_usd: f64,
    pub requests: i64,
    pub step_id: Option<String>,
    pub is_error_limit: bool,
    pub reset_ts: Option<f64>,
    pub ts: f64,
    pub role: Option<String>,
    pub worker: Option<String>,
}

impl UsageRow {
    pub fn new(session_id: &str, pool_id: &str, model: &str, ts: f64) -> Self {
        Self {
            session_id: session_id.into(),
            pool_id: pool_id.into(),
            model: model.into(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            requests: 1,
            step_id: None,
            is_error_limit: false,
            reset_ts: None,
            ts,
            role: None,
            worker: None,
        }
    }
}

/// What one worker run reported (the fields of `WorkerResult` the ledger reads).
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub model: Option<String>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cost_usd: f64,
    pub requests: Option<i64>,
    pub rate_limited: bool,
    /// `{"status": …, "resets_at": …, "windows": {name: {"utilization": f, "resets_at": f}}}`
    pub provider_quota: Value,
}

/// `Ledger::headroom`: room for one more typical step on a pool, or how short it is.
#[derive(Debug, Clone, PartialEq)]
pub enum Headroom {
    Fits,
    Short { pool_id: String, unit: Unit, left: f64, typical: f64, resets_at: Option<f64> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Allowance {
    Unbounded,
    Usd(f64),
    Tokens(i64),
}

impl Allowance {
    pub fn to_json(&self) -> Value {
        match self {
            Self::Unbounded => json!({}),
            Self::Usd(v) => json!({"budget_usd": v}),
            Self::Tokens(v) => json!({"budget_tokens": v}),
        }
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS quota_pools (
    pool_id            TEXT PRIMARY KEY,
    family             TEXT NOT NULL,
    model_regex        TEXT NOT NULL,
    unit               TEXT NOT NULL,
    soft_threshold_pct REAL NOT NULL DEFAULT 0.80,
    hard_threshold_pct REAL NOT NULL DEFAULT 0.95,
    window_seconds     INTEGER NOT NULL DEFAULT 18000,
    window_limit       REAL,
    weekly_limit       REAL
);
CREATE TABLE IF NOT EXISTS quota_usage (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    ts             REAL NOT NULL,
    session_id     TEXT NOT NULL,
    step_id        TEXT,
    pool_id        TEXT NOT NULL,
    model          TEXT NOT NULL,
    tokens_in      INTEGER NOT NULL DEFAULT 0,
    tokens_out     INTEGER NOT NULL DEFAULT 0,
    cost_usd       REAL NOT NULL DEFAULT 0.0,
    is_error_limit INTEGER NOT NULL DEFAULT 0,
    requests       INTEGER NOT NULL DEFAULT 1,
    reset_ts       REAL,
    role           TEXT,
    worker         TEXT
);
CREATE INDEX IF NOT EXISTS quota_usage_pool_ts ON quota_usage(pool_id, ts);
CREATE INDEX IF NOT EXISTS quota_usage_session ON quota_usage(session_id);
CREATE TABLE IF NOT EXISTS quota_signals (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          REAL NOT NULL,
    pool_id     TEXT NOT NULL,
    window      TEXT NOT NULL,
    utilization REAL NOT NULL,
    resets_at   REAL,
    status      TEXT
);
CREATE INDEX IF NOT EXISTS quota_signals_pool_ts ON quota_signals(pool_id, ts);
CREATE TABLE IF NOT EXISTS handoff_packets (
    handoff_id  TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL,
    ts          REAL NOT NULL,
    step_id     TEXT NOT NULL,
    from_worker TEXT NOT NULL,
    to_worker   TEXT,
    reason      TEXT NOT NULL,
    packet_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS handoff_packets_session ON handoff_packets(session_id, ts);
CREATE TABLE IF NOT EXISTS orch_runs (
    run_id      TEXT PRIMARY KEY,
    created_ts  REAL NOT NULL,
    updated_ts  REAL NOT NULL,
    cwd         TEXT NOT NULL,
    goal        TEXT NOT NULL,
    state       TEXT NOT NULL,
    record_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS orch_runs_updated ON orch_runs(updated_ts);
CREATE TABLE IF NOT EXISTS events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts         REAL NOT NULL,
    session_id TEXT,
    kind       TEXT NOT NULL,
    payload    TEXT NOT NULL,
    source     TEXT
);
CREATE INDEX IF NOT EXISTS events_session_ts ON events(session_id, ts);
CREATE INDEX IF NOT EXISTS events_kind_ts    ON events(kind, ts);
CREATE TRIGGER IF NOT EXISTS events_no_update
BEFORE UPDATE ON events BEGIN
    SELECT RAISE(ABORT, 'events is append-only');
END;
";

/// `$CORDATA_STATE_DB`, else `$CORDATA_HOME/state.db`, else `~/.cordata/state.db` — the Python rule.
pub fn default_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CORDATA_STATE_DB") {
        return PathBuf::from(path);
    }
    let home = std::env::var_os("CORDATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cordata"));
    home.join("state.db")
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

fn round(value: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (value * scale).round() / scale
}

pub struct Ledger {
    conn: Connection,
    pub pools: Vec<Pool>,
    clock: Box<dyn Fn() -> f64 + Send>,
    /// Spend the database refused to store (twice). Kept here and counted by pool states and envelopes, so a busy or
    /// broken database never makes a run look cheaper than it was.
    unrecorded: std::sync::Mutex<Vec<UsageRow>>,
}

impl Ledger {
    pub fn open_default(pools: Vec<Pool>) -> Result<Self> {
        Self::open(&default_path(), pools)
    }

    pub fn open(path: &Path, pools: Vec<Pool>) -> Result<Self> {
        make_private(path)?;
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        add_columns(&conn)?;
        // Default pools the list does not name go after it: first match wins, so they cap only what the list leaves out.
        let mut pools = pools;
        let missing: Vec<Pool> = default_pools().into_iter().filter(|d| !pools.iter().any(|p| p.pool_id == d.pool_id)).collect();
        pools.extend(missing);
        let ledger = Self { conn, pools, clock: Box::new(now), unrecorded: std::sync::Mutex::default() };
        ledger.sync_pools()?;
        Ok(ledger)
    }

    /// A test rolls a window forward by injecting a clock.
    pub fn with_clock(mut self, clock: impl Fn() -> f64 + Send + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    fn sync_pools(&self) -> Result<()> {
        for p in &self.pools {
            self.conn.execute(
                "INSERT OR REPLACE INTO quota_pools (pool_id, family, model_regex, unit, soft_threshold_pct, hard_threshold_pct, window_seconds, window_limit, weekly_limit) VALUES (?,?,?,?,?,?,?,?,?)",
                params![p.pool_id, p.family, p.model_regex, p.unit.as_str(), p.soft_threshold_pct, p.hard_threshold_pct, p.window_seconds, p.window_limit, p.weekly_limit],
            )?;
        }
        Ok(())
    }

    /// First match wins. An unconfigured family/model is its own uncapped pool: metered, and a provider limit
    /// still marks it exhausted.
    pub fn pool_for(&self, family: &str, model: Option<&str>) -> Pool {
        self.pools.iter().find(|p| p.matches(family, model)).cloned().unwrap_or_else(|| Pool::new(family, family))
    }

    pub fn record(&self, row: &UsageRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO quota_usage (ts, session_id, step_id, pool_id, model, tokens_in, tokens_out, cost_usd, is_error_limit, requests, reset_ts, role, worker) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![row.ts, row.session_id, row.step_id, row.pool_id, row.model, row.tokens_in, row.tokens_out, row.cost_usd, row.is_error_limit as i64, row.requests, row.reset_ts, row.role, row.worker],
        )?;
        Ok(())
    }

    /// The provider's own window readings → `quota_signals` rows.
    pub fn record_signal(&self, pool_id: &str, quota: &Value) -> Result<()> {
        let Some(windows) = quota.get("windows").and_then(Value::as_object) else { return Ok(()) };
        let status = quota.get("status").and_then(Value::as_str).unwrap_or("");
        let ts = (self.clock)();
        for (name, window) in windows {
            let Some(utilization) = window.get("utilization").and_then(Value::as_f64) else { continue };
            self.conn.execute(
                "INSERT INTO quota_signals (ts, pool_id, window, utilization, resets_at, status) VALUES (?,?,?,?,?,?)",
                params![ts, pool_id, name, utilization, window.get("resets_at").and_then(Value::as_f64), status],
            )?;
        }
        Ok(())
    }

    /// The highest *fresh* utilisation the provider reported: per window, the latest reading whose reset time has
    /// not passed (or, with no reset time, that is younger than the pool's window).
    pub fn provider_reading(&self, pool: &Pool) -> Option<f64> {
        let now = (self.clock)();
        let mut stmt = self
            .conn
            .prepare("SELECT window, utilization, resets_at, ts FROM quota_signals WHERE pool_id = ? AND ts > ? ORDER BY ts DESC, id DESC")
            .ok()?;
        let rows = stmt
            .query_map(params![pool.pool_id, now - WEEK], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, Option<f64>>(2)?, r.get::<_, f64>(3)?))
            })
            .ok()?;
        let mut latest: BTreeMap<String, f64> = BTreeMap::new();
        let mut seen = std::collections::HashSet::new();
        for (window, utilization, resets_at, ts) in rows.flatten() {
            if !seen.insert(window.clone()) {
                continue;
            }
            let fresh = match resets_at {
                Some(reset) => reset > now,
                None => ts > now - pool.window_seconds as f64,
            };
            if fresh {
                latest.insert(window, utilization);
            }
        }
        latest.values().copied().reduce(f64::max)
    }

    /// The soonest reset the provider announced that is still ahead.
    fn provider_reset(&self, pool: &Pool, now: f64) -> Option<f64> {
        self.conn
            .query_row("SELECT MIN(resets_at) FROM quota_signals WHERE pool_id = ? AND resets_at > ?", params![pool.pool_id, now], |r| r.get::<_, Option<f64>>(0))
            .ok()
            .flatten()
    }

    /// One worker result → one usage row in the pool it belongs to (and the provider's window readings).
    #[allow(clippy::too_many_arguments)]
    pub fn record_result(
        &self,
        session_id: &str,
        step_id: Option<&str>,
        family: &str,
        model: Option<&str>,
        outcome: &Outcome,
        role: Option<&str>,
        worker: Option<&str>,
    ) -> UsageRow {
        let used_model = model.or(outcome.model.as_deref());
        let pool = self.pool_for(family, used_model);
        if outcome.provider_quota.is_object() {
            let _ = self.record_signal(&pool.pool_id, &outcome.provider_quota);
        }
        let row = UsageRow {
            session_id: session_id.into(),
            pool_id: pool.pool_id,
            model: used_model.unwrap_or("").into(),
            tokens_in: outcome.tokens_in,
            tokens_out: outcome.tokens_out,
            cost_usd: outcome.cost_usd,
            requests: outcome.requests.unwrap_or(1),
            step_id: step_id.map(Into::into),
            is_error_limit: outcome.rate_limited,
            reset_ts: if outcome.rate_limited { outcome.provider_quota.get("resets_at").and_then(Value::as_f64) } else { None },
            ts: (self.clock)(),
            role: role.map(Into::into),
            worker: worker.map(Into::into),
        };
        // Never raises into a run: a write that fails twice is kept in memory and still counted.
        if self.record(&row).is_err() && self.record(&row).is_err() {
            self.unrecorded.lock().unwrap_or_else(|e| e.into_inner()).push(row.clone());
        }
        row
    }

    pub fn state_of(&self, pool_id: &str) -> PoolState {
        let pool = self.pools.iter().find(|p| p.pool_id == pool_id).cloned().unwrap_or_else(|| Pool::new(pool_id, pool_id));
        self.state(&pool)
    }

    /// About how many typical steps fit before this pool's hard line; `None` without a cap or a history.
    pub fn steps_left(&self, pool_id: &str) -> Option<i64> {
        let pool = self.pools.iter().find(|p| p.pool_id == pool_id)?;
        let left = self.state(pool).to_hard?;
        let typical = self.typical_step(pool)?;
        Some((left / typical).floor() as i64)
    }

    pub fn state(&self, pool: &Pool) -> PoolState {
        let now = (self.clock)();
        let horizon = (pool.window_seconds as f64).max(if pool.weekly_limit.is_some() { WEEK } else { 0.0 });
        let rows = self.usage_rows(&pool.pool_id, now - horizon);
        let Ok(rows) = rows else {
            return PoolState {
                pool_id: pool.pool_id.clone(),
                family: pool.family.clone(),
                unit: pool.unit,
                used: 0.0,
                limit: None,
                pct: 1.0,
                level: "exhausted",
                window_seconds: pool.window_seconds,
                weekly_used: 0.0,
                weekly_limit: None,
                limited_until: None,
                to_hard: None,
                to_soft: None,
                provider_pct: None,
                resets_at: None,
                source: "meter",
            };
        };
        let window_start = now - pool.window_seconds as f64;
        let amount = |r: &Row| match pool.unit {
            Unit::Usd => r.cost_usd,
            Unit::Tokens => (r.tokens_in + r.tokens_out) as f64,
            Unit::Requests => r.requests as f64,
        };
        // `+ 0.0`: an empty f64 sum is -0.0, which would print as "-0%".
        let used: f64 = rows.iter().filter(|r| r.ts > window_start).map(&amount).sum::<f64>() + 0.0;
        let weekly: f64 = if pool.weekly_limit.is_some() { rows.iter().map(&amount).sum::<f64>() + 0.0 } else { 0.0 };
        let mut limited_until: Option<f64> = None;
        for r in rows.iter().filter(|r| r.is_error_limit) {
            let until = r.reset_ts.filter(|t| *t != 0.0).unwrap_or(r.ts + pool.window_seconds as f64);
            if until > now {
                limited_until = Some(limited_until.unwrap_or(0.0).max(until));
            }
        }
        let limit = pool.window_limit;
        let mut metered = limit.filter(|l| *l != 0.0).map_or(0.0, |l| used / l);
        if let Some(weekly_limit) = pool.weekly_limit {
            metered = metered.max(weekly / weekly_limit);
        }
        let provider = self.provider_reading(pool);
        let mut pct = provider.map_or(metered, |p| metered.max(p));
        let source = if provider.is_some_and(|p| p >= metered) { "provider" } else { "meter" };
        let level = if limited_until.is_some() {
            pct = pct.max(1.0);
            "exhausted"
        } else if pct >= 1.0 {
            "exhausted"
        } else if pct >= pool.hard_threshold_pct {
            "hard"
        } else if pct >= pool.soft_threshold_pct {
            "soft"
        } else {
            "ok"
        };
        let (mut to_hard, mut to_soft) = (None, None);
        if let Some(l) = limit {
            to_hard = Some((l * pool.hard_threshold_pct - used).max(0.0));
            to_soft = Some((l * pool.soft_threshold_pct - used).max(0.0));
        }
        if let Some(weekly_limit) = pool.weekly_limit {
            let left = (weekly_limit * pool.hard_threshold_pct - weekly).max(0.0);
            to_hard = Some(to_hard.map_or(left, |h: f64| h.min(left)));
        }
        if let (Some(p), Some(l)) = (provider, limit)
            && p > metered {
                // The provider says more is used than we metered (other sessions, the web app, another machine).
                to_hard = Some(to_hard.unwrap_or(f64::INFINITY).min(((pool.hard_threshold_pct - p) * l).max(0.0)));
                to_soft = Some(((pool.soft_threshold_pct - p) * l).max(0.0));
            }
        if level == "exhausted" {
            to_hard = Some(0.0);
        }
        let oldest_in_window = rows.iter().filter(|r| r.ts > window_start && amount(r) > 0.0).map(|r| r.ts).reduce(f64::min);
        let resets_at = limited_until.or_else(|| self.provider_reset(pool, now)).or(oldest_in_window.map(|ts| ts + pool.window_seconds as f64));
        PoolState {
            pool_id: pool.pool_id.clone(),
            family: pool.family.clone(),
            unit: pool.unit,
            used: round(used, 6),
            limit,
            pct: round(pct, 4),
            level,
            window_seconds: pool.window_seconds,
            weekly_used: round(weekly, 6),
            weekly_limit: pool.weekly_limit,
            limited_until,
            to_hard,
            to_soft,
            provider_pct: provider,
            resets_at,
            source,
        }
    }

    pub fn states(&self) -> Vec<PoolState> {
        self.pools.iter().map(|p| self.state(p)).collect()
    }

    pub fn level(&self, family: &str, model: Option<&str>) -> &'static str {
        self.state(&self.pool_for(family, model)).level
    }

    /// What one step on this pool may spend before the hard line; `Unbounded` when the pool is uncapped or counted
    /// in requests (which a worker cannot cut off mid-step).
    pub fn step_allowance(&self, family: &str, model: Option<&str>) -> Allowance {
        let pool = self.pool_for(family, model);
        let state = self.state(&pool);
        match (state.to_hard, pool.unit) {
            (None, _) | (_, Unit::Requests) => Allowance::Unbounded,
            (Some(h), Unit::Usd) => Allowance::Usd(round(h, 6)),
            (Some(h), Unit::Tokens) => Allowance::Tokens(h as i64),
        }
    }

    /// What one step usually spends on this pool, in its unit: the 75th percentile of the last 20 steps metered here
    /// (every attempt at a step summed). `None` under 3 such steps, or for a pool counted in requests.
    pub fn typical_step(&self, pool: &Pool) -> Option<f64> {
        let amount = match pool.unit {
            Unit::Usd => "SUM(cost_usd)",
            Unit::Tokens => "CAST(SUM(tokens_in + tokens_out) AS REAL)",
            Unit::Requests => return None,
        };
        let sql = format!(
            "SELECT {amount} FROM quota_usage WHERE pool_id = ? AND step_id IS NOT NULL AND is_error_limit = 0 \
             AND (role IS NULL OR role IN ('execute', 'harden')) GROUP BY session_id, step_id ORDER BY MAX(ts) DESC LIMIT 20"
        );
        let mut stmt = self.conn.prepare(&sql).ok()?;
        let mut spends: Vec<f64> = stmt.query_map(params![pool.pool_id], |r| r.get::<_, f64>(0)).ok()?.filter_map(|r| r.ok()).filter(|v| *v > 0.0).collect();
        if spends.len() < 3 {
            return None;
        }
        spends.sort_by(f64::total_cmp);
        Some(spends[(spends.len() * 3).div_ceil(4) - 1])
    }

    /// Whether a typical step still fits under this pool's hard line. A run checks before it starts a step, so it can
    /// stop at a step boundary instead of being cut off halfway through one.
    pub fn headroom(&self, family: &str, model: Option<&str>) -> Headroom {
        let pool = self.pool_for(family, model);
        let state = self.state(&pool);
        match (state.to_hard, self.typical_step(&pool)) {
            (Some(left), Some(typical)) if left < typical => Headroom::Short { pool_id: pool.pool_id, unit: pool.unit, left, typical, resets_at: state.resets_at },
            _ => Headroom::Fits,
        }
    }

    /// `budget` less what one more request may cost, so the request that crosses the line still lands inside it. A CLI
    /// checks its dollar budget only after a request, and cost is known only then: holding back the costliest
    /// request this run has made (before its first, the costliest the ledger has seen; never more than half the
    /// budget) is what makes the cap hold. A ledger that cannot be read holds back half.
    pub fn hold_back(&self, session_id: &str, budget: Option<f64>) -> Option<f64> {
        let costliest = |sql: &str, args: &[&dyn rusqlite::ToSql]| self.conn.query_row(sql, args, |r| r.get::<_, Option<f64>>(0));
        let per_request = "MAX(cost_usd / MAX(requests, 1))";
        let in_run = costliest(&format!("SELECT {per_request} FROM quota_usage WHERE session_id = ?"), &[&session_id]);
        let largest = match in_run {
            Ok(Some(v)) => Ok(v),
            Ok(None) => costliest(&format!("SELECT {per_request} FROM quota_usage"), &[]).map(|v| v.unwrap_or(0.0)),
            Err(e) => Err(e),
        };
        let unrecorded = self.unrecorded.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|r| r.cost_usd / r.requests.max(1) as f64).fold(0.0, f64::max);
        budget.map(|b| b - largest.map_or(b / 2.0, |l| l.max(unrecorded)).min(b / 2.0))
    }

    pub fn envelope(&self, session_id: &str, max_usd: Option<f64>, max_tokens: Option<i64>) -> EnvelopeState {
        let sums = self.conn.query_row(
            "SELECT COALESCE(SUM(cost_usd),0), COALESCE(SUM(tokens_in+tokens_out),0) FROM quota_usage WHERE session_id = ?",
            params![session_id],
            |r| Ok((r.get::<_, f64>(0)?, r.get::<_, i64>(1)?)),
        );
        let lost = self.unrecorded.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|u| u.session_id == session_id).fold((0.0, 0), |(u, t), r| (u + r.cost_usd, t + r.tokens_in + r.tokens_out));
        let sums = sums.map(|(u, t)| (u + lost.0, t + lost.1));
        let Ok((usd, tok)) = sums else {
            return EnvelopeState { session_id: session_id.into(), spent_usd: 0.0, spent_tokens: 0, max_usd, max_tokens, exhausted: true, remaining_usd: None, remaining_tokens: None };
        };
        let exhausted = max_usd.is_some_and(|m| usd >= m) || max_tokens.is_some_and(|m| tok >= m);
        EnvelopeState {
            session_id: session_id.into(),
            spent_usd: round(usd, 6),
            spent_tokens: tok,
            max_usd,
            max_tokens,
            exhausted,
            remaining_usd: max_usd.map(|m| round((m - usd).max(0.0), 6)),
            remaining_tokens: max_tokens.map(|m| (m - tok).max(0)),
        }
    }

    /// One row of the `events` table (append-only). Like the Python writer it never fails a run: errors are returned
    /// for the caller to ignore.
    pub fn append_event(&self, kind: &str, payload: &Value, session_id: Option<&str>, source: Option<&str>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (ts, session_id, kind, payload, source) VALUES (?,?,?,?,?)",
            params![(self.clock)(), session_id, kind, payload.to_string(), source],
        )?;
        Ok(())
    }

    /// Events of one kind, newest first (`recent_events` in Python).
    pub fn recent_events(&self, kind: &str, limit: usize) -> Vec<Value> {
        let Ok(mut stmt) = self.conn.prepare("SELECT payload FROM events WHERE kind = ? ORDER BY ts DESC, id DESC LIMIT ?") else { return Vec::new() };
        let Ok(rows) = stmt.query_map(params![kind, limit as i64], |r| r.get::<_, String>(0)) else { return Vec::new() };
        rows.flatten().filter_map(|text| serde_json::from_str(&text).ok()).collect()
    }

    /// Persists a run record (`record` is its JSON; the indexed columns are read from it).
    pub fn save_run(&self, record: &Value) -> Result<()> {
        let text = |k: &str| record.get(k).and_then(Value::as_str).map(str::to_owned);
        self.conn.execute(
            "INSERT OR REPLACE INTO orch_runs (run_id, created_ts, updated_ts, cwd, goal, state, record_json) VALUES (?,?,?,?,?,?,?)",
            params![text("run_id"), record.get("created").and_then(Value::as_f64), record.get("updated").and_then(Value::as_f64), text("cwd"), text("goal"), text("state"), record.to_string()],
        )?;
        Ok(())
    }

    pub fn load_run(&self, run_id: &str) -> Option<Value> {
        let text: String = self.conn.query_row("SELECT record_json FROM orch_runs WHERE run_id = ?", params![run_id], |r| r.get(0)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Run records, most recently updated first.
    pub fn recent_runs(&self, limit: usize) -> Vec<Value> {
        let Ok(mut stmt) = self.conn.prepare("SELECT record_json FROM orch_runs ORDER BY updated_ts DESC LIMIT ?") else { return Vec::new() };
        let Ok(rows) = stmt.query_map(params![limit as i64], |r| r.get::<_, String>(0)) else { return Vec::new() };
        rows.flatten().filter_map(|t| serde_json::from_str(&t).ok()).collect()
    }

    /// Persists a handoff packet (`packet` is the packet's JSON; the indexed columns are read from it).
    pub fn save_handoff(&self, packet: &Value) -> Result<()> {
        let field = |k: &str| packet.get(k).and_then(Value::as_str).map(str::to_owned);
        self.conn.execute(
            "INSERT OR REPLACE INTO handoff_packets (handoff_id, session_id, ts, step_id, from_worker, to_worker, reason, packet_json) VALUES (?,?,?,?,?,?,?,?)",
            params![field("handoff_id"), field("session_id"), packet.get("timestamp").and_then(Value::as_f64), field("step_id"), field("from_worker"), field("to_worker"), field("reason"), packet.to_string()],
        )?;
        Ok(())
    }

    pub fn load_handoff(&self, handoff_id: &str) -> Option<Value> {
        let text: String = self.conn.query_row("SELECT packet_json FROM handoff_packets WHERE handoff_id = ?", params![handoff_id], |r| r.get(0)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Handoff packets, newest first.
    pub fn recent_handoffs(&self, limit: usize) -> Vec<Value> {
        let Ok(mut stmt) = self.conn.prepare("SELECT packet_json FROM handoff_packets ORDER BY ts DESC LIMIT ?") else { return Vec::new() };
        let Ok(rows) = stmt.query_map(params![limit as i64], |r| r.get::<_, String>(0)) else { return Vec::new() };
        rows.flatten().filter_map(|t| serde_json::from_str(&t).ok()).collect()
    }

    pub fn handoffs_for_session(&self, session_id: &str) -> Vec<Value> {
        let Ok(mut stmt) = self.conn.prepare("SELECT packet_json FROM handoff_packets WHERE session_id = ? ORDER BY ts") else { return Vec::new() };
        let Ok(rows) = stmt.query_map(params![session_id], |r| r.get::<_, String>(0)) else { return Vec::new() };
        rows.flatten().filter_map(|text| serde_json::from_str(&text).ok()).collect()
    }

    /// One run's spend grouped by (role, worker), sorted by that key.
    pub fn run_usage(&self, run_id: &str) -> Vec<Value> {
        let Ok(mut stmt) = self.conn.prepare("SELECT step_id, pool_id, model, role, worker, requests, tokens_in, tokens_out, cost_usd FROM quota_usage WHERE session_id = ? ORDER BY id") else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map(params![run_id], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, Option<i64>>(7)?,
                r.get::<_, Option<f64>>(8)?,
            ))
        }) else {
            return Vec::new();
        };
        let mut groups: BTreeMap<(String, String), (i64, i64, i64, f64)> = BTreeMap::new();
        for (step, pool, model, role, worker, requests, tin, tout, cost) in rows.flatten() {
            let role = role.filter(|r| !r.is_empty()).unwrap_or_else(|| match step.as_deref().unwrap_or("") {
                s @ ("plan" | "research" | "conformance" | "harden" | "audit") => s.to_owned(),
                _ => "execute".to_owned(),
            });
            let worker = worker.filter(|w| !w.is_empty()).unwrap_or_else(|| format!("{}:{}", pool.unwrap_or_default(), model.unwrap_or_default()));
            let entry = groups.entry((role, worker)).or_default();
            entry.0 += requests.unwrap_or(1);
            entry.1 += tin.unwrap_or(0);
            entry.2 += tout.unwrap_or(0);
            entry.3 += cost.unwrap_or(0.0);
        }
        groups
            .into_iter()
            .map(|((role, worker), (calls, tin, tout, cost))| {
                json!({"role": role, "worker": worker, "calls": calls, "tokens_in": tin, "tokens_out": tout, "cost_usd": round(cost, 6)})
            })
            .collect()
    }

    fn usage_rows(&self, pool_id: &str, since: f64) -> Result<Vec<Row>> {
        let mut stmt = self.conn.prepare("SELECT ts, tokens_in, tokens_out, cost_usd, requests, is_error_limit, reset_ts FROM quota_usage WHERE pool_id = ? AND ts > ?")?;
        let rows = stmt
            .query_map(params![pool_id, since], |r| {
                Ok(Row {
                    ts: r.get(0)?,
                    tokens_in: r.get(1)?,
                    tokens_out: r.get(2)?,
                    cost_usd: r.get(3)?,
                    requests: r.get(4)?,
                    is_error_limit: r.get::<_, i64>(5)? != 0,
                    reset_ts: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let unrecorded = self.unrecorded.lock().unwrap_or_else(|e| e.into_inner());
        let mut rows = rows;
        rows.extend(unrecorded.iter().filter(|u| u.pool_id == pool_id && u.ts > since).map(|u| Row {
            ts: u.ts, tokens_in: u.tokens_in, tokens_out: u.tokens_out, cost_usd: u.cost_usd, requests: u.requests,
            is_error_limit: u.is_error_limit, reset_ts: u.reset_ts,
        }));
        Ok(rows)
    }
}

struct Row {
    ts: f64,
    tokens_in: i64,
    tokens_out: i64,
    cost_usd: f64,
    requests: i64,
    is_error_limit: bool,
    reset_ts: Option<f64>,
}

/// Old databases lack `quota_usage.role`/`.worker`.
fn add_columns(conn: &Connection) -> Result<()> {
    let have: Vec<String> = conn
        .prepare("PRAGMA table_info(quota_usage)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<_, _>>()?;
    for column in ["role", "worker"] {
        if !have.iter().any(|c| c == column) {
            // Another process may add it between the check and the ALTER.
            let _ = conn.execute(&format!("ALTER TABLE quota_usage ADD COLUMN {column} TEXT"), []);
        }
    }
    Ok(())
}

/// The ledger records prompts and tool inputs: keep it readable by this user only (0600), as `state_store` does.
fn make_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(path)?;
    }
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let candidate = PathBuf::from(name);
        if candidate.exists() {
            std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}
