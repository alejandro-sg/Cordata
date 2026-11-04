//! A ranked map of a repository's definitions, cut to a token budget: what an agent needs to know *where* things are
//! before it reads anything. Instead of grepping and opening whole files, the model gets the signatures that matter
//! most for the question, a line each, with their line numbers.
//!
//! How: tree-sitter's own `tags` queries find the definitions (Rust, Python, TypeScript/JavaScript, Go); every
//! identifier a file uses is a reference. Files form a graph — an edge from the file that uses a name to the file that
//! defines it — ranked by PageRank personalised towards the question's terms and the files it names, then each
//! definition takes its share of that rank, boosted by how well its name and signature match the question (BM25).
//!
//! Offline, deterministic, no model. A [`RepoMap`] keeps what it parsed and re-reads only files whose size or mtime
//! changed, so a refresh of an unchanged tree is a directory walk.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Instant, SystemTime};

use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator as _};

/// Files larger than this are generated or vendored more often than written: skipped.
const MAX_FILE_BYTES: u64 = 512 * 1024;
/// A walk stops here; a map of more files would not fit any budget anyway.
const MAX_FILES: usize = 20_000;
/// A signature line is cut to this many characters.
const MAX_SIG: usize = 120;
/// How a question ranks a definition when a model is loaded: graph, BM25 words (with expansion), embedding meaning.
/// Tuned on this repository's 16 bench questions (scripts/bench.sh): meaning-heavy lifted paraphrased questions from MRR
/// 0.09 to 0.28 with code-word questions unchanged (0.77). Sixteen questions are few; re-run the bench before changing.
const W_GRAPH: f64 = 0.1;
const W_WORDS: f64 = 0.3;
const W_MEANING: f64 = 0.6;
/// Query expansion: the code's words at least this close to a question word, at most this many per word, weighed
/// this much (times their similarity) against the question's own.
const EXPAND_MIN_SIM: f32 = 0.5;
/// How much a definition in test code counts, against one in the code.
const TEST_FACTOR: f64 = 0.4;
const EXPAND_PER_TERM: usize = 4;
const EXPAND_WEIGHT: f64 = 0.6;
/// Characters per token, for the budget (code averages a little under four).
const CHARS_PER_TOKEN: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Lang {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
}

impl Lang {
    fn of(path: &Path) -> Option<Self> {
        Some(match path.extension()?.to_str()? {
            "rs" => Lang::Rust,
            "py" => Lang::Python,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
            "go" => Lang::Go,
            _ => return None,
        })
    }

    fn language(self) -> Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }

    /// The grammar's own tags query; TypeScript's extends JavaScript's, as the grammars ship them.
    fn tags(self) -> String {
        match self {
            Lang::Rust => tree_sitter_rust::TAGS_QUERY.into(),
            Lang::Python => tree_sitter_python::TAGS_QUERY.into(),
            Lang::TypeScript | Lang::Tsx => format!("{}\n{}", tree_sitter_javascript::TAGS_QUERY, tree_sitter_typescript::TAGS_QUERY),
            Lang::JavaScript => tree_sitter_javascript::TAGS_QUERY.into(),
            Lang::Go => tree_sitter_go::TAGS_QUERY.into(),
        }
    }
}

/// Compiled once per language and shared: compiling a tags query costs more than parsing most files.
static QUERIES: LazyLock<HashMap<Lang, Option<Query>>> = LazyLock::new(|| {
    [Lang::Rust, Lang::Python, Lang::TypeScript, Lang::Tsx, Lang::JavaScript, Lang::Go].into_iter().map(|l| (l, Query::new(&l.language(), &l.tags()).ok())).collect()
});

/// One definition: a function, type, trait, class, method, constant…
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Def {
    pub name: String,
    /// 1-based.
    pub line: usize,
    /// `function`, `class`, `method`, `interface`, `module`, … as the tags query names it.
    pub kind: String,
    /// The definition's first line, trimmed.
    pub sig: String,
    /// The comment right above it (doc comment, `#`/`//` lines), joined: searched, not drawn.
    pub doc: String,
    /// Defined in test code (a `#[cfg(test)]` module, `test_*`, `*_test.go`, `*.test.ts`…): ranked below the code it
    /// tests, since a question is usually about the code.
    pub test: bool,
}

#[derive(Debug, Clone, Default)]
struct FileTags {
    stamp: Stamp,
    defs: Vec<Def>,
    /// Identifier → how often this file uses it (definitions excluded).
    refs: HashMap<String, u32>,
    /// One unit vector per definition, when an embedder is loaded.
    vecs: Vec<Vec<f32>>,
}

/// What a refresh did.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Refresh {
    pub files: usize,
    pub parsed: usize,
    pub defs: usize,
    pub ms: f64,
}

/// A hit from [`RepoMap::search`].
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// Relative to the root.
    pub path: PathBuf,
    pub def: Def,
    pub score: f64,
}

pub struct RepoMap {
    root: PathBuf,
    files: BTreeMap<PathBuf, FileTags>,
    embedder: Option<std::sync::Arc<Embedder>>,
    /// Every word the code uses in names and comments, embedded once: what a question's words are matched against
    /// when they are not the code's own.
    vocab: HashMap<String, Vec<f32>>,
}

/// A local static embedding model (Model2Vec): a token-table lookup and a mean, no network, no GPU — microseconds per
/// definition. With the `embed` feature off, or no model on disk, the map ranks by graph and words alone.
pub struct Embedder {
    #[cfg(feature = "embed")]
    model: model2vec_rs::model::StaticModel,
}

impl Embedder {
    /// `CORDATA_EMBED_MODEL`, else `~/.cordata/models/potion-code-16M` if it is there. Never downloads.
    pub fn from_env() -> Option<Self> {
        let dir = std::env::var_os("CORDATA_EMBED_MODEL").map(PathBuf::from).or_else(|| {
            let home = std::env::var_os("CORDATA_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cordata")))?;
            Some(home.join("models/potion-code-16M"))
        })?;
        dir.join("model.safetensors").is_file().then(|| Self::load(&dir)).flatten()
    }

    #[cfg(feature = "embed")]
    pub fn load(dir: &Path) -> Option<Self> {
        model2vec_rs::model::StaticModel::from_pretrained(dir, None, Some(true), None).ok().map(|model| Self { model })
    }

    #[cfg(not(feature = "embed"))]
    pub fn load(_dir: &Path) -> Option<Self> {
        None
    }

    fn encode(&self, texts: &[String]) -> Vec<Vec<f32>> {
        #[cfg(feature = "embed")]
        {
            self.model.encode(texts).into_iter().map(unit).collect()
        }
        #[cfg(not(feature = "embed"))]
        {
            texts.iter().map(|_| Vec::new()).collect()
        }
    }
}

#[cfg_attr(not(feature = "embed"), allow(dead_code))]
fn unit(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// What a definition is about, for the embedder: its name in words, its signature and its comment.
fn def_text(path: &Path, d: &Def) -> String {
    format!("{} {} {} {}", words(&d.name).join(" "), d.sig, d.doc, path.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default())
}

impl RepoMap {
    pub fn new(root: &Path) -> Self {
        Self { root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()), files: BTreeMap::new(), embedder: None, vocab: HashMap::new() }
    }

    /// Ranks with `embedder` too (definitions are embedded as they are parsed; set it before the first refresh).
    pub fn with_embedder(mut self, embedder: Option<std::sync::Arc<Embedder>>) -> Self {
        self.embedder = embedder;
        self
    }

    /// Walks the tree (gitignore-aware) and re-parses only files that are new or changed; forgets deleted ones.
    pub fn refresh(&mut self) -> Refresh {
        let start = Instant::now();
        let mut seen: Vec<(PathBuf, Lang, Stamp)> = Vec::new();
        let walk = ignore::WalkBuilder::new(&self.root).hidden(true).git_ignore(true).require_git(false).build();
        for entry in walk.flatten() {
            if seen.len() >= MAX_FILES {
                break;
            }
            let path = entry.path();
            let Some(lang) = Lang::of(path) else { continue };
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
                continue;
            }
            let Ok(rel) = path.strip_prefix(&self.root) else { continue };
            seen.push((rel.to_path_buf(), lang, (meta.len(), meta.modified().ok())));
        }
        let stale: Vec<(PathBuf, Lang, Stamp)> =
            seen.iter().filter(|(rel, _, stamp)| self.files.get(rel).is_none_or(|f| f.stamp != *stamp)).cloned().collect();
        let parsed = parse_all(&self.root, &stale);
        let keep: HashSet<&PathBuf> = seen.iter().map(|(rel, _, _)| rel).collect();
        self.files.retain(|rel, _| keep.contains(rel));
        let count = parsed.len();
        let mut parsed = parsed;
        if let Some(e) = &self.embedder {
            for (rel, f) in &mut parsed {
                let texts: Vec<String> = f.defs.iter().map(|d| def_text(rel, d)).collect();
                f.vecs = if texts.is_empty() { Vec::new() } else { e.encode(&texts) };
            }
        }
        self.files.extend(parsed);
        if let Some(e) = &self.embedder {
            let fresh: Vec<String> = self
                .files
                .values()
                .flat_map(|f| f.defs.iter().flat_map(|d| words(&d.name).into_iter().chain(words(&d.doc))))
                .filter(|w| !self.vocab.contains_key(w) && !w.chars().all(|c| c.is_ascii_digit()))
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            if !fresh.is_empty() {
                let vecs = e.encode(&fresh);
                self.vocab.extend(fresh.into_iter().zip(vecs));
            }
        }
        Refresh { files: self.files.len(), parsed: count, defs: self.files.values().map(|f| f.defs.len()).sum(), ms: start.elapsed().as_secs_f64() * 1e3 }
    }

    /// The question's words, plus — with a model — the code's own words nearest each one in meaning, so "persist the
    /// conversation" also searches `save`, `session`, `history`.
    fn expand(&self, terms: &HashSet<String>) -> HashMap<String, f64> {
        let mut out: HashMap<String, f64> = terms.iter().map(|t| (t.clone(), 1.0)).collect();
        // Morphological stemming & plural variants: e.g. need <-> needs, rule <-> rules
        for t in terms {
            let st = stem(t);
            if st != *t && st.len() >= 3 {
                out.entry(st).or_insert(0.9);
            }
        }
        let Some(e) = &self.embedder else { return out };
        // Every question word, even one the code uses: "persist" is in this repository, but the code that persists says
        // "save".
        let foreign: Vec<String> = terms.iter().cloned().collect();
        if foreign.is_empty() || self.vocab.is_empty() {
            return out;
        }
        for (term, q) in foreign.iter().zip(e.encode(&foreign)) {
            let mut near: Vec<(&String, f32)> = self.vocab.iter().map(|(w, v)| (w, dot(&q, v))).filter(|(_, s)| *s >= EXPAND_MIN_SIM).collect();
            near.sort_by(|a, b| b.1.total_cmp(&a.1));
            for (w, sim) in near.into_iter().take(EXPAND_PER_TERM) {
                let weight = EXPAND_WEIGHT * f64::from(sim);
                let slot = out.entry(w.clone()).or_default();
                *slot = slot.max(weight);
            }
            let _ = term;
        }
        out
    }

    /// Definitions ranked for `query`, best first. `mentioned` are paths (relative or absolute) the conversation names.
    pub fn search(&self, query: &str, mentioned: &[&str], limit: usize) -> Vec<Hit> {
        let mut hits = self.rank(query, mentioned);
        hits.truncate(limit);
        hits
    }

    /// The map itself: the best-ranked definitions grouped by file, cut to `budget_tokens`.
    ///
    /// ```text
    /// crates/cordata-guard/src/redact.rs
    ///   98│ pub fn scrub(text: &str) -> (String, usize) {
    /// ```
    pub fn render(&self, query: &str, mentioned: &[&str], budget_tokens: usize) -> String {
        let budget = budget_tokens * CHARS_PER_TOKEN;
        let mut by_file: Vec<(PathBuf, Vec<Def>)> = Vec::new();
        let mut used = 0;
        for hit in self.rank(query, mentioned) {
            let line = format!("  {:>4}│ {}\n", hit.def.line, hit.def.sig);
            let header = if by_file.iter().any(|(p, _)| *p == hit.path) { 0 } else { hit.path.as_os_str().len() + 1 };
            if used + header + line.len() > budget {
                if used + 40 > budget {
                    break;
                }
                continue;
            }
            used += header + line.len();
            match by_file.iter_mut().find(|(p, _)| *p == hit.path) {
                Some((_, defs)) => defs.push(hit.def),
                None => by_file.push((hit.path, vec![hit.def])),
            }
        }
        let mut out = String::with_capacity(used);
        for (path, mut defs) in by_file {
            defs.sort_by_key(|d| d.line);
            out.push_str(&path.to_string_lossy());
            out.push('\n');
            for def in defs {
                out.push_str(&format!("  {:>4}│ {}\n", def.line, def.sig));
            }
        }
        out
    }

    fn rank(&self, query: &str, mentioned: &[&str]) -> Vec<Hit> {
        let files: Vec<(&PathBuf, &FileTags)> = self.files.iter().collect();
        let n = files.len();
        if n == 0 {
            return Vec::new();
        }
        let terms: HashSet<String> = words(query).into_iter().collect();
        let index: HashMap<&Path, usize> = files.iter().enumerate().map(|(i, (p, _))| (p.as_path(), i)).collect();

        // Who defines each name.
        let mut definers: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, (_, f)) in files.iter().enumerate() {
            for d in &f.defs {
                let list = definers.entry(d.name.as_str()).or_default();
                if list.last() != Some(&i) {
                    list.push(i);
                }
            }
        }

        // Personalisation: the files the conversation names and those whose path or definitions share its words.
        let mut personal = vec![0.0f64; n];
        for m in mentioned {
            let p = Path::new(m);
            let rel = p.strip_prefix(&self.root).unwrap_or(p);
            if let Some(&i) = index.get(rel) {
                personal[i] += 10.0;
            }
        }
        if !terms.is_empty() {
            for (i, (path, f)) in files.iter().enumerate() {
                let path_hits = words(&path.to_string_lossy()).iter().filter(|w| terms.contains(*w)).count();
                let def_hits = f.defs.iter().filter(|d| words(&d.name).iter().any(|w| terms.contains(w))).count();
                personal[i] += path_hits as f64 + (def_hits as f64).sqrt();
            }
        }
        let total: f64 = personal.iter().sum();
        if total == 0.0 {
            personal.iter_mut().for_each(|p| *p = 1.0 / n as f64);
        } else {
            personal.iter_mut().for_each(|p| *p /= total);
        }

        // Edges: user file → definer file, weighted as Aider weighs them.
        let mut edges: Vec<Vec<(usize, f64, &str)>> = vec![Vec::new(); n];
        for (i, (_, f)) in files.iter().enumerate() {
            for (name, &count) in &f.refs {
                let Some(defs) = definers.get(name.as_str()) else { continue };
                let mut mul = 1.0;
                if words(name).iter().any(|w| terms.contains(w)) {
                    mul *= 10.0;
                }
                if name.len() >= 8 && (name.contains('_') || name.chars().any(char::is_uppercase)) {
                    mul *= 10.0;
                }
                if name.starts_with('_') {
                    mul *= 0.1;
                }
                if defs.len() > 5 {
                    mul *= 0.1; // `new`, `run`, `from`: defined everywhere, says nothing
                }
                let w = mul * f64::from(count).sqrt();
                for &d in defs {
                    if d != i {
                        edges[i].push((d, w, name.as_str()));
                    }
                }
            }
        }
        let out_weight: Vec<f64> = edges.iter().map(|e| e.iter().map(|(_, w, _)| w).sum()).collect();

        // Personalised PageRank.
        let mut rank = personal.clone();
        for _ in 0..30 {
            let mut next: Vec<f64> = personal.iter().map(|p| 0.15 * p).collect();
            let mut dangling = 0.0;
            for i in 0..n {
                if out_weight[i] == 0.0 {
                    dangling += rank[i];
                    continue;
                }
                for &(d, w, _) in &edges[i] {
                    next[d] += 0.85 * rank[i] * w / out_weight[i];
                }
            }
            for (x, p) in next.iter_mut().zip(&personal) {
                *x += 0.85 * dangling * p;
            }
            rank = next;
        }

        // Each definition takes the rank flowing to it through its name, plus a floor from its own file.
        let mut def_rank: HashMap<(usize, &str), f64> = HashMap::new();
        for i in 0..n {
            for &(d, w, name) in &edges[i] {
                *def_rank.entry((d, name)).or_default() += rank[i] * w / out_weight[i];
            }
        }
        let bm25 = Bm25::new(&files, &self.expand(&terms));
        // Graph rank spans orders of magnitude (and is 0 for a file nothing reaches): compare it on a log scale, 0..1.
        // With a question, its words decide most of the order and the graph breaks ties towards what the code actually
        // uses; without one, the graph alone.
        let graph: Vec<Vec<f64>> = files
            .iter()
            .enumerate()
            .map(|(i, (_, f))| f.defs.iter().map(|d| (def_rank.get(&(i, d.name.as_str())).copied().unwrap_or(0.0) + 0.01 * rank[i] + 1e-12).ln()).collect())
            .collect();
        let (lo, hi) = graph.iter().flatten().fold((f64::MAX, f64::MIN), |(lo, hi), g| (lo.min(*g), hi.max(*g)));
        // Meaning, when a model is loaded: cosine to the question, scaled 0..1 by the best match.
        let query_vec = self.embedder.as_ref().filter(|_| !terms.is_empty()).and_then(|e| e.encode(&[query.to_owned()]).pop()).filter(|v| !v.is_empty());
        let sims: Option<Vec<Vec<f64>>> = query_vec.map(|q| files.iter().map(|(_, f)| f.vecs.iter().map(|v| f64::from(dot(&q, v)).max(0.0)).collect()).collect());
        let best_sim = sims.as_ref().map_or(1.0, |s| s.iter().flatten().fold(0.0f64, |m, x| m.max(*x)).max(1e-9));
        let mut hits: Vec<Hit> = Vec::new();
        let mut parts: Vec<(f64, f64, Option<f64>)> = Vec::new();
        for (i, (path, f)) in files.iter().enumerate() {
            for (j, d) in f.defs.iter().enumerate() {
                let g = if hi > lo { (graph[i][j] - lo) / (hi - lo) } else { 1.0 };
                let sim = sims.as_ref().and_then(|s| s[i].get(j)).map(|x| x / best_sim);
                // Test code answers fewer questions than the code it tests: it ranks lower, never disappears.
                let t = if d.test { TEST_FACTOR } else { 1.0 };
                parts.push((g * t, bm25.score(i, j) * t, sim.map(|m| m * t)));
                hits.push(Hit { path: (*path).clone(), def: d.clone(), score: 0.0 });
            }
        }
        if terms.is_empty() {
            for (h, (g, _, _)) in hits.iter_mut().zip(&parts) {
                h.score = *g;
            }
        } else if sims.is_none() {
            for (h, (g, w, _)) in hits.iter_mut().zip(&parts) {
                h.score = 0.25 * g + 0.75 * w;
            }
        } else {
            // A linear blend: on this repository's questions it beat reciprocal-rank fusion and is simpler.
            for (h, (g, w, m)) in hits.iter_mut().zip(&parts) {
                h.score = W_GRAPH * g + W_WORDS * w + W_MEANING * m.unwrap_or(0.0);
            }
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.path.cmp(&b.path)).then_with(|| a.def.line.cmp(&b.def.line)));
        hits
    }
}

/// BM25 over each definition's name, signature and path words, normalised to 0..1 across the repository.
struct Bm25 {
    scores: HashMap<(usize, usize), f64>,
}

impl Bm25 {
    /// `terms` with a weight each: the question's own words count 1, words added by [`RepoMap::expand`] less.
    fn new(files: &[(&PathBuf, &FileTags)], terms: &HashMap<String, f64>) -> Self {
        let mut scores = HashMap::new();
        if terms.is_empty() {
            return Self { scores };
        }
        let docs: Vec<((usize, usize), Vec<String>)> = files
            .iter()
            .enumerate()
            .flat_map(|(i, (path, f))| {
                let path_words = words(&path.to_string_lossy());
                f.defs.iter().enumerate().map(move |(j, d)| {
                    let mut w = words(&d.name);
                    // The name counts 3x: it is the most deliberate word in the definition.
                    w.extend(words(&d.name));
                    w.extend(words(&d.name));
                    w.extend(words(&d.sig));
                    w.extend(words(&d.doc));
                    w.extend(path_words.iter().cloned());
                    let stems: Vec<String> = w.iter().map(|x| stem(x)).filter(|s| s.len() >= 3 && !w.contains(s)).collect();
                    w.extend(stems);
                    ((i, j), w)
                })
            })
            .collect();
        let n = docs.len() as f64;
        let avg = docs.iter().map(|(_, w)| w.len()).sum::<usize>() as f64 / n.max(1.0);
        let mut df: HashMap<&str, f64> = HashMap::new();
        for (_, w) in &docs {
            for t in w.iter().filter(|t| terms.contains_key(*t)).collect::<HashSet<_>>() {
                *df.entry(t.as_str()).or_default() += 1.0;
            }
        }
        let (k1, b) = (1.2, 0.75);
        let mut best: f64 = 0.0;
        for (key, w) in &docs {
            let len = w.len() as f64;
            let mut s = 0.0;
            for (t, weight) in terms {
                let tf = w.iter().filter(|x| *x == t).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let idf = ((n - df[t.as_str()] + 0.5) / (df[t.as_str()] + 0.5) + 1.0).ln();
                s += weight * idf * tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * len / avg));
            }
            if s > 0.0 {
                // Boost definitions where the name directly contains query terms or stems
                let def = &files[key.0].1.defs[key.1];
                let name_words: Vec<String> = words(&def.name);
                let name_matches = name_words.iter().filter(|nw| terms.contains_key(*nw) || terms.contains_key(&stem(nw))).count();
                if name_matches > 0 {
                    s *= 1.0 + 0.35 * name_matches as f64;
                }
                best = best.max(s);
                scores.insert(*key, s);
            }
        }
        scores.values_mut().for_each(|s| *s /= best);
        Self { scores }
    }

    fn score(&self, file: usize, def: usize) -> f64 {
        self.scores.get(&(file, def)).copied().unwrap_or(0.0)
    }
}

/// Lower-case words of an identifier, path or sentence: `parseMcpConfig` and `parse_mcp_config` both give
/// `parse mcp config`. Words under three letters and a few stop words carry nothing.
pub fn words(text: &str) -> Vec<String> {
    const STOP: &[&str] = &["the", "and", "for", "with", "from", "that", "this", "where", "what", "how", "does", "into", "fn", "pub", "let", "mut", "self", "src", "crates", "impl"];
    let mut out = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        let mut word = String::new();
        let mut prev_lower = false;
        for c in raw.chars() {
            if c.is_uppercase() && prev_lower {
                out.push(std::mem::take(&mut word));
            }
            prev_lower = c.is_lowercase() || c.is_ascii_digit();
            word.extend(c.to_lowercase());
        }
        out.push(word);
    }
    out.retain(|w| w.len() >= 3 && !STOP.contains(&w.as_str()));
    out
}

/// Canonical stem / morphological base for English identifiers and query terms.
/// Fast, deterministic, handles plurals, -ed, -ing, and common derivations.
pub fn stem(word: &str) -> String {
    let mut s = word.to_lowercase();
    if s.len() <= 3 || !s.is_ascii() {
        return s;
    }
    // 1. Plurals and 3rd person singular forms
    if s.ends_with("ies") && s.len() > 4 {
        s.truncate(s.len() - 3);
        s.push('y');
    } else if s.ends_with("es") && s.len() > 4 && (s.ends_with("shes") || s.ends_with("ches") || s.ends_with("sses") || s.ends_with("xes") || s.ends_with("zes")) {
        s.truncate(s.len() - 2);
    } else if s.ends_with('s') && !s.ends_with("ss") && !s.ends_with("us") && !s.ends_with("is") && s.len() > 3 {
        s.truncate(s.len() - 1);
    }

    // 2. Past tense and participles (-ed)
    if s.ends_with("eed") && s.len() > 4 {
        s.truncate(s.len() - 1);
    } else if s.ends_with("ed") && s.len() > 4 {
        s.truncate(s.len() - 2);
        if s.len() > 3 {
            let bytes = s.as_bytes();
            if bytes[bytes.len() - 1] == bytes[bytes.len() - 2] && !matches!(bytes[bytes.len() - 1], b'l' | b's' | b'z') {
                s.truncate(s.len() - 1);
            }
        }
    }

    // 3. Progressive participle (-ing)
    if s.ends_with("ing") && s.len() > 5 {
        s.truncate(s.len() - 3);
        if s.len() > 3 {
            let bytes = s.as_bytes();
            if bytes[bytes.len() - 1] == bytes[bytes.len() - 2] && !matches!(bytes[bytes.len() - 1], b'l' | b's' | b'z') {
                s.truncate(s.len() - 1);
            }
        }
    }

    // 4. Nominal suffixes (-tion)
    if s.ends_with("tion") && s.len() > 5 {
        s.truncate(s.len() - 3);
    }

    s
}

/// A file's size and modification time: what tells a refresh it changed.
type Stamp = (u64, Option<SystemTime>);

/// Parses `stale` across the machine's cores; each thread keeps its own parser.
fn parse_all(root: &Path, stale: &[(PathBuf, Lang, Stamp)]) -> Vec<(PathBuf, FileTags)> {
    if stale.is_empty() {
        return Vec::new();
    }
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(stale.len()).max(1);
    let chunk = stale.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let jobs: Vec<_> = stale
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    let mut parser = Parser::new();
                    // A file that cannot be read or parsed is kept empty with its stamp, so it is not retried every refresh.
                    part.iter().map(|(rel, lang, stamp)| {
                        let mut parsed = std::fs::read_to_string(root.join(rel)).ok().and_then(|text| tags(&mut parser, *lang, &text));
                        if is_test_path(rel) && let Some(f) = parsed.as_mut() {
                            f.defs.iter_mut().for_each(|d| d.test = true);
                        }
                        (rel.clone(), FileTags { stamp: *stamp, ..parsed.unwrap_or_default() })
                    }).collect::<Vec<_>>()
                })
            })
            .collect();
        jobs.into_iter().flat_map(|j| j.join().unwrap_or_default()).collect()
    })
}

/// Up to six comment lines directly above `row` (skipping attributes and decorators), or a Python docstring's first
/// lines just inside the definition.
fn doc_above(lines: &[&str], row: usize, node: tree_sitter::Node, text: &str) -> String {
    let mut doc: Vec<&str> = Vec::new();
    let mut r = row;
    while r > 0 && doc.len() < 6 {
        r -= 1;
        let l = lines.get(r).map_or("", |l| l.trim());
        if let Some(c) = l.strip_prefix("///").or_else(|| l.strip_prefix("//!")).or_else(|| l.strip_prefix("//")).or_else(|| l.strip_prefix('#').filter(|_| !l.starts_with("#["))).or_else(|| l.strip_prefix("* ")).or_else(|| l.strip_prefix("/**")) {
            doc.push(c.trim());
        } else if l.starts_with("#[") || l.starts_with('@') || l == "*/" {
            continue;
        } else {
            break;
        }
    }
    doc.reverse();
    if doc.is_empty() {
        let mut end = node.end_byte().min(node.start_byte() + 600);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let body = &text[node.start_byte()..end];
        if let Some(i) = body.find("\"\"\"") {
            return body[i + 3..].split("\"\"\"").next().unwrap_or("").lines().take(3).collect::<Vec<_>>().join(" ");
        }
    }
    doc.join(" ")
}

/// A file that holds only tests: `tests/`, `__tests__/`, `test_*.py`, `*_test.go`, `*.test.ts`, `*.spec.js`…
fn is_test_path(rel: &Path) -> bool {
    let s = rel.to_string_lossy();
    let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    s.split('/').any(|part| part == "tests" || part == "__tests__" || part == "test")
        || name.starts_with("test_")
        || name.ends_with("_test.go")
        || name.ends_with("_test.py")
        || [".test.", ".spec."].iter().any(|m| name.contains(m))
}

/// The kinds of leaf node that name something a file uses.
const IDENT_KINDS: &[&str] = &["identifier", "type_identifier", "field_identifier", "property_identifier", "shorthand_property_identifier", "package_identifier"];

fn tags(parser: &mut Parser, lang: Lang, text: &str) -> Option<FileTags> {
    let query = QUERIES.get(&lang)?.as_ref()?;
    parser.set_language(&lang.language()).ok()?;
    let tree = parser.parse(text, None)?;
    let names = query.capture_names();
    let lines: Vec<&str> = text.lines().collect();
    let mut defs = Vec::new();
    let mut def_spans = HashSet::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), text.as_bytes());
    while let Some(m) = matches.next() {
        let mut name = None;
        let mut kind = None;
        for c in m.captures {
            let cap = names[c.index as usize];
            if cap == "name" {
                name = Some(c.node);
            } else if let Some(k) = cap.strip_prefix("definition.") {
                kind = Some((k, c.node));
            }
        }
        let (Some(name), Some((kind, node))) = (name, kind) else { continue };
        let Ok(name_text) = name.utf8_text(text.as_bytes()) else { continue };
        def_spans.insert(name.byte_range());
        let first = text[node.start_byte()..].lines().next().unwrap_or("").trim();
        let sig: String = first.chars().take(MAX_SIG).collect();
        let row = node.start_position().row;
        defs.push(Def { name: name_text.to_owned(), line: row + 1, kind: kind.to_owned(), sig, doc: doc_above(&lines, row, node, text), test: false });
    }
    defs.sort_by_key(|d| d.line);
    defs.dedup_by(|a, b| a.line == b.line && a.name == b.name);
    // Rust keeps tests at the bottom of the file in `#[cfg(test)]` modules; other languages name them.
    let test_from = (lang == Lang::Rust).then(|| lines.iter().position(|l| l.trim_start().starts_with("#[cfg(test)]")).map(|r| r + 1)).flatten();
    for d in &mut defs {
        d.test = test_from.is_some_and(|from| d.line > from) || d.name.starts_with("test_") || d.name.starts_with("Test");
    }

    let mut refs: HashMap<String, u32> = HashMap::new();
    let mut walk = tree.walk();
    let mut descend = true;
    loop {
        let node = walk.node();
        if descend && node.child_count() == 0 && IDENT_KINDS.contains(&node.kind()) && !def_spans.contains(&node.byte_range())
            && let Ok(t) = node.utf8_text(text.as_bytes())
            && t.len() > 1
        {
            *refs.entry(t.to_owned()).or_default() += 1;
        }
        if descend && walk.goto_first_child() {
            continue;
        }
        if walk.goto_next_sibling() {
            descend = true;
            continue;
        }
        if !walk.goto_parent() {
            break;
        }
        descend = false;
    }
    Some(FileTags { stamp: (0, None), defs, refs, vecs: Vec::new() })
}

/// One map per project for a process, refreshed on each use (a stat walk when nothing changed).
static MAPS: LazyLock<Mutex<HashMap<PathBuf, RepoMap>>> = LazyLock::new(Mutex::default);

/// [`RepoMap::render`] for `root`, from a map kept warm across calls.
pub fn render_cached(root: &Path, query: &str, mentioned: &[&str], budget_tokens: usize) -> String {
    let key = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let Ok(mut maps) = MAPS.lock() else { return String::new() };
    static EMBEDDER: LazyLock<Option<std::sync::Arc<Embedder>>> = LazyLock::new(|| Embedder::from_env().map(std::sync::Arc::new));
    let map = maps.entry(key.clone()).or_insert_with(|| RepoMap::new(&key).with_embedder(EMBEDDER.clone()));
    map.refresh();
    map.render(query, mentioned, budget_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, body) in files {
            let p = dir.path().join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        dir
    }

    #[test]
    fn every_grammar_query_compiles() {
        for (lang, q) in QUERIES.iter() {
            assert!(q.is_some(), "{lang:?}");
        }
    }

    #[test]
    fn words_split_identifiers_and_drop_noise() {
        assert_eq!(words("parseMcpConfig"), ["parse", "mcp", "config"]);
        assert_eq!(words("parse_mcp_config(x)"), ["parse", "mcp", "config"]);
        assert_eq!(words("where is the HTTPServer"), ["httpserver"]);
    }

    #[test]
    fn finds_definitions_in_each_language() {
        let dir = repo(&[
            ("a.rs", "pub struct Ledger;\nimpl Ledger { pub fn charge(&self) {} }\npub trait Pool {}\n"),
            ("b.py", "class Vault:\n    def unlock(self):\n        pass\n"),
            ("c.ts", "export function route(x: number) { return x }\nexport class Router {}\ninterface Opts {}\n"),
            ("d.go", "package d\nfunc Serve() {}\ntype Server struct{}\n"),
            ("e.js", "function draw() {}\n"),
        ]);
        let mut map = RepoMap::new(dir.path());
        let r = map.refresh();
        assert_eq!(r.files, 5);
        let names: HashSet<String> = map.files.values().flat_map(|f| f.defs.iter().map(|d| d.name.clone())).collect();
        for want in ["Ledger", "charge", "Pool", "Vault", "unlock", "route", "Router", "Serve", "Server", "draw"] {
            assert!(names.contains(want), "{want} missing from {names:?}");
        }
    }

    #[test]
    fn the_used_definition_outranks_the_unused_and_the_question_steers() {
        let dir = repo(&[
            ("core/redact.rs", "pub fn scrub_secrets(t: &str) -> String { t.into() }\npub fn unused_helper() {}\n"),
            ("ui/a.rs", "fn a() { scrub_secrets(\"x\"); }\n"),
            ("ui/b.rs", "fn b() { scrub_secrets(\"y\"); }\n"),
            ("net/socket.rs", "pub fn open_socket_listener() {}\n"),
        ]);
        let mut map = RepoMap::new(dir.path());
        map.refresh();
        let top = map.search("", &[], 1);
        assert_eq!(top[0].def.name, "scrub_secrets", "referenced from two files");
        let steered = map.search("open a socket listener", &[], 1);
        assert_eq!(steered[0].def.name, "open_socket_listener");
    }

    #[test]
    fn the_render_keeps_to_its_budget_and_refresh_reparses_only_changes() {
        let body: String = (0..400).map(|i| format!("pub fn function_number_{i}() {{}}\n")).collect();
        let dir = repo(&[("big.rs", &body), ("small.rs", "pub fn tiny() {}\n")]);
        let mut map = RepoMap::new(dir.path());
        assert_eq!(map.refresh().parsed, 2);
        let out = map.render("", &[], 200);
        assert!(out.len() <= 200 * CHARS_PER_TOKEN, "{}", out.len());
        assert!(out.contains("│ pub fn"), "{out}");
        assert_eq!(map.refresh().parsed, 0, "nothing changed");
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(dir.path().join("small.rs"), "pub fn tiny() {}\npub fn tinier() {}\n").unwrap();
        assert_eq!(map.refresh().parsed, 1);
        std::fs::remove_file(dir.path().join("big.rs")).unwrap();
        assert_eq!(map.refresh().files, 1);
    }

    #[test]
    fn multibyte_text_near_a_definition_does_not_lose_the_file() {
        let body = format!("def f():\n    x = '{}'\n", "é".repeat(400));
        let dir = repo(&[("a.py", &body), ("b.py", "def g():\n    pass\n")]);
        let mut map = RepoMap::new(dir.path());
        let r = map.refresh();
        assert_eq!((r.files, r.defs), (2, 2));
        assert_eq!(map.refresh().parsed, 0);
    }

    #[test]
    fn the_code_outranks_its_tests() {
        let dir = repo(&[
            ("src/store.rs", "/// Writes the session to disk.\npub fn save_session() {}\n#[cfg(test)]\nmod tests {\n    fn save_session_round_trips() { super::save_session(); }\n}\n"),
            ("tests/store_test.rs", "fn save_session_twice() {}\n"),
        ]);
        let mut map = RepoMap::new(dir.path());
        map.refresh();
        let hits = map.search("save session", &[], 3);
        assert_eq!(hits[0].def.name, "save_session");
        assert!(hits[1..].iter().all(|h| h.def.test), "{hits:?}");
        assert!(is_test_path(Path::new("web/src/a.test.ts")) && is_test_path(Path::new("pkg/x_test.go")) && !is_test_path(Path::new("src/testing.rs")));
    }

    #[test]
    fn ignored_files_stay_out() {
        let dir = repo(&[(".gitignore", "target/\n"), ("target/gen.rs", "pub fn generated() {}\n"), ("src/lib.rs", "pub fn real() {}\n")]);
        let mut map = RepoMap::new(dir.path());
        map.refresh();
        assert!(map.render("", &[], 500).contains("real") && !map.render("", &[], 500).contains("generated"));
    }
}

/// On this repository: `cargo test --release -p cordata-repomap bench_ -- --ignored --nocapture` (scripts/bench.sh).
#[cfg(test)]
mod bench {
    use super::*;

    /// (question, a definition that answers it, the file it lives in)
    const QUESTIONS: &[(&str, &str, &str)] = &[
        ("where are credentials masked before drawing text", "shown", "cordata-view/src/lib.rs"),
        ("agy worker sandbox rules for its home state", "agy_home_rules", "scope.rs"),
        ("which mcp servers start in an untrusted project folder", "configured", "native/mcp.rs"),
        ("orchestrator posts a system message to the run bus", "post_system", "cordata-bus/src/lib.rs"),
        ("save chat session history to disk", "save_in", "acp-core/src/history.rs"),
        ("current git worktree detection", "current_worktree", "git_ops.rs"),
        ("does this tool call need approval", "needs_approval", "native/tools.rs"),
        ("kill the process group of a worker", "kill_group", "workers/cli.rs"),
    ];

    /// What an agent does without a map: `code_search` for the question's words (200 matching lines at most, as the
    /// native tool caps it), then reads the three files with the most matches in full.
    fn grep_and_read(map: &RepoMap, query: &str) -> (usize, Vec<PathBuf>) {
        let terms = words(query);
        let mut lines = 0usize;
        let mut chars = 0usize;
        let mut per_file: Vec<(usize, PathBuf, usize)> = Vec::new();
        for rel in map.files.keys() {
            let Ok(text) = std::fs::read_to_string(map.root.join(rel)) else { continue };
            let mut hits = 0;
            for line in text.lines() {
                let lower = line.to_lowercase();
                if terms.iter().any(|t| lower.contains(t.as_str())) {
                    hits += 1;
                    if lines < 200 {
                        lines += 1;
                        chars += rel.as_os_str().len() + 8 + line.len().min(200);
                    }
                }
            }
            if hits > 0 {
                per_file.push((hits, rel.clone(), text.len()));
            }
        }
        per_file.sort_by_key(|f| std::cmp::Reverse(f.0));
        let read: Vec<PathBuf> = per_file.iter().take(3).map(|(_, p, _)| p.clone()).collect();
        chars += per_file.iter().take(3).map(|(_, _, len)| len).sum::<usize>();
        (chars / CHARS_PER_TOKEN, read)
    }

    /// The same answers asked the way a person would, with few of the code's own words.
    const PARAPHRASED: &[(&str, &str, &str)] = &[
        ("stop secrets from appearing on screen", "shown", "cordata-view/src/lib.rs"),
        ("keep a confined coding agent from overwriting its own executable", "agy_home_rules", "scope.rs"),
        ("decide which external tool servers to launch for a repository", "configured", "native/mcp.rs"),
        ("broadcast what a finished step found to the other agents", "post_system", "cordata-bus/src/lib.rs"),
        ("persist the conversation so it can be resumed later", "save_in", "acp-core/src/history.rs"),
        ("figure out which checkout of the repository the user is in", "current_worktree", "git_ops.rs"),
        ("require a person to say yes before a tool changes anything", "needs_approval", "native/tools.rs"),
        ("terminate a child program together with everything it spawned", "kill_group", "workers/cli.rs"),
    ];

    fn quality(map: &RepoMap, set: &[(&str, &str, &str)]) -> (Vec<usize>, f64, usize, usize) {
        let ranks: Vec<usize> = set
            .iter()
            .map(|(q, def, file)| map.search(q, &[], 500).iter().position(|h| h.def.name == *def && h.path.to_string_lossy().ends_with(file)).map_or(501, |p| p + 1))
            .collect();
        let mrr = ranks.iter().map(|r| 1.0 / *r as f64).sum::<f64>() / set.len() as f64;
        let in_map = |budget: usize| set.iter().filter(|(q, def, file)| map.search(q, &[], 500).iter().take_while({
            let mut used = 0;
            move |h| { used += h.def.sig.len() + 8; used <= budget * CHARS_PER_TOKEN }
        }).any(|h| h.def.name == *def && h.path.to_string_lossy().ends_with(file))).count();
        (ranks.clone(), mrr, in_map(256), in_map(1024))
    }

    #[test]
    #[ignore]
    fn bench_ranking_words_versus_meaning() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let mut plain = RepoMap::new(&root);
        plain.refresh();
        let mut rows = vec![("graph+BM25", plain)];
        for model in ["potion-base-8M", "potion-code-16M"] {
            let dir = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".cordata/models").join(model);
            let start = Instant::now();
            let Some(e) = Embedder::load(&dir) else {
                println!("BENCH ranking: {model} not loaded (build with --features embed and put the model in {})", dir.display());
                continue;
            };
            let load = start.elapsed().as_secs_f64() * 1e3;
            let mut map = RepoMap::new(&root).with_embedder(Some(std::sync::Arc::new(e)));
            let r = map.refresh();
            println!("BENCH ranking: {model} load {load:.0}ms, index with embeddings {:.0}ms ({} definitions)", r.ms, r.defs);
            rows.push((model, map));
        }
        for (name, map) in &rows {
            let start = Instant::now();
            let (r1, m1, s1, b1) = quality(map, QUESTIONS);
            let (r2, m2, s2, b2) = quality(map, PARAPHRASED);
            let per_q = start.elapsed().as_secs_f64() * 1e3 / (2 * QUESTIONS.len()) as f64;
            println!("BENCH ranking {name:<16} code words: MRR {m1:.2} ranks {r1:?} in 256/1024 tok {s1}/{b1} | paraphrased: MRR {m2:.2} ranks {r2:?} in 256/1024 tok {s2}/{b2} | {per_q:.1}ms/query");
        }
    }

    #[test]
    #[ignore]
    fn bench_repomap_on_this_repository() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let mut map = RepoMap::new(&root);
        let cold = map.refresh();
        let warm = map.refresh();
        println!("BENCH repomap index: {} files, {} definitions, cold {:.0}ms, warm refresh {:.1}ms", cold.files, cold.defs, cold.ms, warm.ms);
        let (mut map_hits, mut grep_hits, mut map_tokens, mut grep_tokens, mut ms) = (0, 0, 0, 0, 0.0);
        for (q, def, file) in QUESTIONS {
            let start = Instant::now();
            let out = map.render(q, &[], 1024);
            ms += start.elapsed().as_secs_f64() * 1e3;
            let found = out.split('\n').scan(String::new(), |cur, line| {
                if !line.starts_with(' ') {
                    *cur = line.to_owned();
                }
                Some((cur.clone(), line.to_owned()))
            }).any(|(path, line)| path.ends_with(file) && line.contains(&format!(" {def}")));
            let (tokens, read) = grep_and_read(&map, q);
            let grep_found = read.iter().any(|p| p.to_string_lossy().ends_with(file));
            map_hits += usize::from(found);
            grep_hits += usize::from(grep_found);
            map_tokens += out.len() / CHARS_PER_TOKEN;
            grep_tokens += tokens;
            println!("BENCH   {:<58} map {} {:>4} tok | grep+read {} {:>6} tok", q, if found { "✓" } else { "✗" }, out.len() / CHARS_PER_TOKEN, if grep_found { "✓" } else { "✗" }, tokens);
        }
        let n = QUESTIONS.len();
        let ranks: Vec<usize> = QUESTIONS
            .iter()
            .map(|(q, def, file)| map.search(q, &[], 500).iter().position(|h| h.def.name == *def && h.path.to_string_lossy().ends_with(file)).map_or(501, |p| p + 1))
            .collect();
        let mrr = ranks.iter().map(|r| 1.0 / *r as f64).sum::<f64>() / n as f64;
        let small = QUESTIONS.iter().filter(|(q, def, file)| {
            let out = map.render(q, &[], 256);
            out.contains(&format!(" {def}")) && out.contains(file)
        }).count();
        println!("BENCH repomap: rank of the answer {ranks:?} (MRR {mrr:.2}); in a 256-token map {small}/{n}");
        println!(
            "BENCH repomap: answer in context {map_hits}/{n} with {} tokens/question ({:.1}ms/query) | grep+read {grep_hits}/{n} with {} tokens/question",
            map_tokens / n,
            ms / n as f64,
            grep_tokens / n
        );
    }
}

