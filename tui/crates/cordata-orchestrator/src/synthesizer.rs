//! Isolated parallel waves and the SYNTHESIZE pass that folds them back together.
//!
//! **Why isolate.** Steps of one wave have pairwise-disjoint scopes, but they still share one working tree: a worker's
//! changed-files list sees its siblings' writes, a rollback has to be narrowed to the step's scope, and a build a worker
//! runs reads its siblings' half-written files. `git worktree add HEAD` is not the answer (it drops uncommitted edits, and
//! checkpoints work outside git too), so each step of a wave of several runs in a **sandbox**: a copy of the project as it
//! is right now, dirty files included.
//!
//! **How it is cheap.** On APFS a file is copied with `clonefile(2)` — copy-on-write, no data blocks duplicated until
//! someone writes. Elsewhere it falls back to an ordinary copy. Dependency and build directories are not copied but
//! *symlinked*, so a test run in the sandbox finds its toolchain; `.git` and `.cordata` are left out.
//!
//! **Harvest.** When a step completes, the files it changed *inside its scope* are copied back into the primary tree. A
//! file the primary tree changed since the sandbox was made is a conflict: reported, never overwritten.
//!
//! **Synthesize.** After the last wave, before HARDEN, [`check`] reads the combined change: syntax of every changed file
//! (Python, JSON, TOML, YAML) and, for Python, imports between project modules — a `from pkg.mod import name` whose module
//! exists in the project but no longer defines `name` is exactly the drift two agents writing two sides of one interface
//! produce.
//!
//! Syntax differs from CPython's `compile()` in two ways: Python is parsed with tree-sitter (grammar errors only — not the
//! semantic ones `compile` also raises, like `return` outside a function) and the messages are the parsers' own.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use sha1::{Digest, Sha1};
use tree_sitter::{Node, Parser, Tree};

use crate::executor::{Harvest, Isolation, SandboxHandle};
use crate::machine::Synthesis;
use crate::scope;

/// Left out of a sandbox entirely (not linked either): a sandbox is not a repository, and the shadow store is the run's.
pub const SKIP: [&str; 2] = [".git", ".cordata"];
pub const MAX_FILES: usize = 60_000;
/// Directories that are rebuilt or vendored (`checkpoints.DEPENDENCY_DIRS`).
pub use cordata_checkpoints::DEPENDENCY_DIRS;

pub(crate) fn digest(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(Sha1::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect())
}

/// Copies one file, copy-on-write when the filesystem can. `true` when it was a clone.
pub(crate) fn copy_file(src: &Path, dst: &Path) -> std::io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let (s, d) = (CString::new(src.as_os_str().as_bytes())?, CString::new(dst.as_os_str().as_bytes())?);
        // SAFETY: both pointers are valid NUL-terminated strings for the call; CLONE_NOFOLLOW (1) never follows a symlink.
        if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 1) } == 0 {
            return Ok(true);
        }
    }
    let _ = (CString::new(""), OsStrExt::as_bytes(src.as_os_str())); // keep the imports used off macOS
    std::fs::copy(src, dst)?;
    Ok(false)
}

/// One step's private copy of the project.
#[derive(Debug)]
pub struct Sandbox {
    pub root: PathBuf,
    pub source: PathBuf,
    pub cloned: usize,
    pub copied: usize,
    pub linked: Vec<String>,
    /// relative path → digest of the primary tree's file when the step started (`None`: it did not exist) — what harvest
    /// compares against to tell the step's change from someone else's.
    pub base: HashMap<String, Option<String>>,
}

impl Sandbox {
    /// Every file was cloned copy-on-write.
    pub fn cow(&self) -> bool {
        self.cloned > 0 && self.copied == 0
    }

    pub fn discard(&self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Removes sandboxes a crashed run left behind (`cordata-*` under the temp dir, untouched for `older_than`).
pub fn sweep(older_than: Duration, parent: Option<&Path>) -> usize {
    let base = parent.map_or_else(std::env::temp_dir, Path::to_path_buf);
    let mut removed = 0;
    for entry in std::fs::read_dir(base).into_iter().flatten().flatten() {
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir && entry.file_name().to_string_lossy().starts_with("cordata-") {
            let old = entry.metadata().and_then(|m| m.modified()).ok().and_then(|m| SystemTime::now().duration_since(m).ok()).is_some_and(|age| age > older_than);
            if old && std::fs::remove_dir_all(entry.path()).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

fn temp_dir(parent: &Path, prefix: &str) -> std::io::Result<PathBuf> {
    use std::hash::{BuildHasher, Hasher};
    for _ in 0..16 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        let path = parent.join(format!("{prefix}{:08x}", hasher.finish() & 0xffff_ffff));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("could not create a sandbox directory"))
}

/// A sandbox of `project` as it is now (uncommitted and untracked files included). With `scope`, the primary tree's
/// in-scope files are remembered as the harvest base. `Err` is "too large" or an I/O failure — run the step shared.
pub fn create(project: &Path, parent: Option<&Path>, label: &str, scope: &[String], max_files: usize) -> Result<Sandbox, String> {
    let source = std::fs::canonicalize(project).map_err(|e| e.to_string())?;
    let parent = parent.map_or_else(std::env::temp_dir, Path::to_path_buf);
    let root = temp_dir(&parent, &format!("cordata-{label}-")).map_err(|e| e.to_string())?;
    let mut sandbox = Sandbox { root: root.clone(), source: source.clone(), cloned: 0, copied: 0, linked: Vec::new(), base: HashMap::new() };
    let mut count = 0usize;
    let mut stack = vec![PathBuf::new()]; // relative directories still to copy
    while let Some(rel_dir) = stack.pop() {
        let (src_dir, dst_dir) = (source.join(&rel_dir), root.join(&rel_dir));
        for entry in std::fs::read_dir(&src_dir).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let Ok(kind) = entry.file_type() else { continue };
            let (src, dst) = (entry.path(), dst_dir.join(&name));
            if kind.is_dir() || kind.is_symlink() && std::fs::metadata(&src).is_ok_and(|m| m.is_dir()) {
                let name_str = name.to_string_lossy();
                if SKIP.contains(&name_str.as_ref()) {
                    continue;
                }
                if kind.is_symlink() || DEPENDENCY_DIRS.contains(&name_str.as_ref()) {
                    let target = if kind.is_symlink() { std::fs::read_link(&src).unwrap_or_else(|_| src.clone()) } else { src.clone() };
                    if std::os::unix::fs::symlink(target, &dst).is_ok() {
                        sandbox.linked.push(rel_dir.join(&name).to_string_lossy().into_owned());
                    }
                    continue;
                }
                if std::fs::create_dir(&dst).is_ok() {
                    stack.push(rel_dir.join(&name));
                }
                continue;
            }
            count += 1;
            if count > max_files {
                sandbox.discard();
                return Err(format!("the project has more than {} files", group(max_files)));
            }
            if kind.is_symlink() {
                if let Ok(target) = std::fs::read_link(&src) {
                    let _ = std::os::unix::fs::symlink(target, &dst);
                }
                continue;
            }
            match copy_file(&src, &dst) {
                Ok(true) => sandbox.cloned += 1,
                Ok(false) => sandbox.copied += 1,
                Err(_) => continue,
            }
        }
    }
    if !scope.is_empty() {
        let files = scoped_files(&sandbox, scope);
        remember(&mut sandbox, &files);
    }
    Ok(sandbox)
}

fn group(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Records the primary tree's content of `paths` as the sandbox's base, before the step runs.
pub fn remember(sandbox: &mut Sandbox, paths: &[String]) {
    for rel in paths {
        sandbox.base.insert(rel.clone(), digest(&sandbox.source.join(rel)));
    }
}

/// Files in the sandbox (and in the source) that a scope covers — the base a step's harvest is checked against.
pub fn scoped_files(sandbox: &Sandbox, scope: &[String]) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    for tree in [&sandbox.root, &sandbox.source] {
        let mut stack = vec![tree.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let Ok(kind) = entry.file_type() else { continue };
                let name = entry.file_name().to_string_lossy().into_owned();
                if kind.is_dir() {
                    if !SKIP.contains(&name.as_str()) && !DEPENDENCY_DIRS.contains(&name.as_str()) {
                        stack.push(entry.path());
                    }
                } else if let Ok(rel) = entry.path().strip_prefix(tree) {
                    let rel = rel.to_string_lossy().into_owned();
                    if scope::matches(&rel, scope, &sandbox.source.to_string_lossy()) {
                        out.insert(rel);
                    }
                }
            }
        }
    }
    out.into_iter().collect()
}

/// `os.path.normpath` on a relative path.
fn normpath(rel: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|p| *p != "..") => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() { ".".into() } else { parts.join("/") }
}

/// Copies a completed step's in-scope changes from its sandbox into the primary tree.
pub fn harvest(sandbox: &Sandbox, changed: &[String], scope: &[String]) -> Harvest {
    let mut out = Harvest::default();
    let mut paths: Vec<&String> = changed.iter().collect();
    paths.sort();
    paths.dedup();
    let source = sandbox.source.to_string_lossy().into_owned();
    let real_root = std::fs::canonicalize(&sandbox.root).unwrap_or_else(|_| sandbox.root.clone());
    for raw in paths {
        let rel = normpath(raw);
        if rel.starts_with("..") || rel.starts_with('/') || !scope::matches(&rel, scope, &source) {
            out.skipped.push(rel);
            continue;
        }
        let (src, dst) = (sandbox.root.join(&rel), sandbox.source.join(&rel));
        // A worker that wrote through a symlink (a linked dependency dir, a planted link) wrote outside its sandbox:
        // never follow it back into the primary tree.
        let exists = std::fs::symlink_metadata(&src).is_ok();
        if exists && (std::fs::symlink_metadata(&src).is_ok_and(|m| m.file_type().is_symlink()) || !std::fs::canonicalize(&src).is_ok_and(|r| r.starts_with(&real_root))) {
            out.skipped.push(rel);
            continue;
        }
        if digest(&dst) != sandbox.base.get(&rel).cloned().flatten() {
            // The primary tree's file is not what the sandbox started from (edited, created or deleted meanwhile).
            out.conflicts.push(rel);
            continue;
        }
        if exists {
            if let Some(dir) = dst.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = PathBuf::from(format!("{}.cordata-harvest", dst.display()));
            if std::fs::copy(&src, &tmp).and_then(|_| std::fs::rename(&tmp, &dst)).is_ok() {
                out.written.push(rel);
            }
        } else if dst.exists() && std::fs::remove_file(&dst).is_ok() {
            out.deleted.push(rel);
        }
    }
    out
}

/// The executor's [`Isolation`]: one copy-on-write sandbox per step.
pub struct CowIsolation {
    parent: Option<PathBuf>,
    max_files: usize,
    boxes: Mutex<HashMap<String, Sandbox>>,
}

impl Default for CowIsolation {
    fn default() -> Self {
        Self { parent: None, max_files: MAX_FILES, boxes: Mutex::new(HashMap::new()) }
    }
}

impl CowIsolation {
    pub fn in_dir(parent: &Path) -> Self {
        Self { parent: Some(parent.to_path_buf()), ..Self::default() }
    }

    pub fn max_files(mut self, n: usize) -> Self {
        self.max_files = n;
        self
    }
}

impl Isolation for CowIsolation {
    fn create(&self, project: &Path, label: &str, scope: &[String]) -> Result<SandboxHandle, String> {
        let sandbox = create(project, self.parent.as_deref(), label, scope, self.max_files)?;
        let handle = SandboxHandle { root: sandbox.root.clone(), id: sandbox.root.to_string_lossy().into_owned() };
        self.boxes.lock().unwrap().insert(handle.id.clone(), sandbox);
        Ok(handle)
    }

    fn harvest(&self, handle: &SandboxHandle, changed: &[String], scope: &[String]) -> Harvest {
        self.boxes.lock().unwrap().get(&handle.id).map(|b| harvest(b, changed, scope)).unwrap_or_default()
    }

    fn discard(&self, handle: &SandboxHandle) {
        if let Some(sandbox) = self.boxes.lock().unwrap().remove(&handle.id) {
            sandbox.discard();
        }
    }
}

// ---------------------------------------------------------------------------------------------- the SYNTHESIZE checks

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// `syntax` | `import`
    pub check: &'static str,
    pub path: String,
    pub line: Option<usize>,
    pub message: String,
}

impl Problem {
    pub fn render(&self) -> String {
        match self.line {
            Some(line) if line > 0 => format!("{}:{line}: [{}] {}", self.path, self.check, self.message),
            _ => format!("{}: [{}] {}", self.path, self.check, self.message),
        }
    }
}

fn parse_python(text: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_python::LANGUAGE.into()).ok()?;
    parser.parse(text, None)
}

/// The first syntax error in the tree: `(line, message)`.
fn first_error(tree: &Tree, text: &str) -> Option<(usize, String)> {
    if !tree.root_node().has_error() {
        return None;
    }
    fn find<'t>(node: Node<'t>) -> Option<Node<'t>> {
        if node.is_error() || node.is_missing() {
            return Some(node);
        }
        let mut cursor = node.walk();
        node.children(&mut cursor).filter(|c| c.has_error() || c.is_error() || c.is_missing()).find_map(find)
    }
    let node = find(tree.root_node())?;
    let line = node.start_position().row + 1;
    let shown = text.lines().nth(node.start_position().row).map(|l| l.trim().chars().take(80).collect::<String>()).unwrap_or_default();
    let what = if node.is_missing() { format!("missing {}", node.kind()) } else { "invalid syntax".to_owned() };
    Some((line, if shown.is_empty() { what } else { format!("{what}: {shown}") }))
}

fn syntax_error(path: &str, text: &str) -> Option<(Option<usize>, String)> {
    let suffix = Path::new(path).extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())).unwrap_or_default();
    match suffix.as_str() {
        ".py" | ".pyi" => parse_python(text).and_then(|t| first_error(&t, text)).map(|(l, m)| (Some(l), m)),
        ".json" => serde_json::from_str::<serde::de::IgnoredAny>(text).err().map(|e| (Some(e.line()), e.to_string())),
        ".toml" => text.parse::<toml::Table>().err().map(|e| {
            let line = e.span().map(|s| text[..s.start.min(text.len())].matches('\n').count() + 1);
            (line, e.message().lines().next().unwrap_or("").chars().take(200).collect())
        }),
        ".yaml" | ".yml" => {
            use serde::Deserialize;
            for document in serde_yaml::Deserializer::from_str(text) {
                if let Err(e) = serde::de::IgnoredAny::deserialize(document) {
                    let line = e.location().map(|l| l.line());
                    return Some((line, e.to_string().lines().next().unwrap_or("").chars().take(200).collect()));
                }
            }
            None
        }
        _ => None,
    }
}

/// The project file a dotted module name refers to, if it is one of the project's own (`None` for a library).
fn module_file(project: &Path, module: &str, importer: &str) -> Option<PathBuf> {
    let parts: Vec<&str> = module.split('.').collect();
    let importer_dir = project.join(importer).parent().map(Path::to_path_buf).unwrap_or_else(|| project.to_path_buf());
    for base in [project.to_path_buf(), importer_dir] {
        let joined = parts.iter().fold(base, |p, part| p.join(part));
        let (file, package) = (PathBuf::from(format!("{}.py", joined.display())), joined.join("__init__.py"));
        for candidate in [file, package] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn text_of<'a>(node: Node<'_>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

/// Names bound at the top of a module, or `None` when it is too clever to judge statically (conditional definitions, a
/// star import) or does not parse.
fn defined_names(path: &Path) -> Option<std::collections::HashSet<String>> {
    let text = String::from_utf8_lossy(&std::fs::read(path).ok()?).into_owned();
    let tree = parse_python(&text)?;
    if tree.root_node().has_error() {
        return None;
    }
    let mut names = std::collections::HashSet::new();
    let mut star = false;
    let mut cursor = tree.root_node().walk();
    for node in tree.root_node().named_children(&mut cursor) {
        collect_binding(node, &text, &mut names, &mut star)?;
    }
    (!star).then_some(names)
}

/// Adds the names a top-level statement binds. `None` bails out (a conditional statement).
fn collect_binding(node: Node<'_>, src: &str, names: &mut std::collections::HashSet<String>, star: &mut bool) -> Option<()> {
    match node.kind() {
        "function_definition" | "class_definition" => {
            names.insert(text_of(node.child_by_field_name("name")?, src).to_owned());
        }
        "decorated_definition" => {
            if let Some(def) = node.child_by_field_name("definition") {
                collect_binding(def, src, names, star)?;
            }
        }
        "expression_statement" => {
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                if child.kind() == "assignment" {
                    assignment_targets(child, src, names);
                }
            }
        }
        "import_statement" => {
            let mut c = node.walk();
            for alias in node.children_by_field_name("name", &mut c) {
                let bound = match alias.kind() {
                    "aliased_import" => alias.child_by_field_name("alias").map(|a| text_of(a, src).to_owned()),
                    _ => Some(text_of(alias, src).to_owned()),
                };
                if let Some(b) = bound {
                    names.insert(b.split('.').next().unwrap_or("").to_owned());
                }
            }
        }
        "import_from_statement" => {
            let mut c = node.walk();
            if node.children(&mut c).any(|ch| ch.kind() == "wildcard_import") {
                *star = true;
            }
            let mut c = node.walk();
            for alias in node.children_by_field_name("name", &mut c) {
                let bound = match alias.kind() {
                    "aliased_import" => alias.child_by_field_name("alias").map(|a| text_of(a, src).to_owned()),
                    _ => Some(text_of(alias, src).to_owned()),
                };
                if let Some(b) = bound {
                    names.insert(b.split('.').next().unwrap_or("").to_owned());
                }
            }
        }
        "if_statement" | "try_statement" => return None, // conditional definitions: too clever to judge statically
        _ => {}
    }
    Some(())
}

/// `ast.Assign` / `ast.AnnAssign` targets: every `Name` in the left side (an attribute contributes its object only).
fn assignment_targets(assignment: Node<'_>, src: &str, names: &mut std::collections::HashSet<String>) {
    fn walk(node: Node<'_>, src: &str, names: &mut std::collections::HashSet<String>) {
        match node.kind() {
            "identifier" => {
                names.insert(text_of(node, src).to_owned());
            }
            "attribute" => {
                if let Some(object) = node.child_by_field_name("object") {
                    walk(object, src, names);
                }
            }
            _ => {
                let mut c = node.walk();
                for child in node.named_children(&mut c) {
                    walk(child, src, names);
                }
            }
        }
    }
    if let Some(left) = assignment.child_by_field_name("left") {
        walk(left, src, names);
    }
    // `a = b = 1` nests the second assignment on the right.
    if let Some(right) = assignment.child_by_field_name("right").filter(|r| r.kind() == "assignment") {
        assignment_targets(right, src, names);
    }
}

struct FromImport {
    line: usize,
    level: usize,
    module: Option<String>,
    names: Vec<String>,
}

fn from_imports(tree: &Tree, src: &str) -> Vec<FromImport> {
    fn walk(node: Node<'_>, src: &str, out: &mut Vec<FromImport>) {
        if node.kind() == "import_from_statement" {
            let (mut level, mut module) = (0, None);
            if let Some(m) = node.child_by_field_name("module_name") {
                if m.kind() == "relative_import" {
                    let text = text_of(m, src);
                    level = text.chars().take_while(|c| *c == '.').count();
                    let rest = text[level..].trim();
                    module = (!rest.is_empty()).then(|| rest.to_owned());
                } else {
                    module = Some(text_of(m, src).to_owned());
                }
            }
            let mut c = node.walk();
            let mut names = Vec::new();
            for alias in node.children_by_field_name("name", &mut c) {
                let original = if alias.kind() == "aliased_import" { alias.child_by_field_name("name").map(|n| text_of(n, src)) } else { Some(text_of(alias, src)) };
                names.extend(original.map(str::to_owned));
            }
            let mut c = node.walk();
            if node.children(&mut c).any(|ch| ch.kind() == "wildcard_import") {
                names.push("*".into());
            }
            out.push(FromImport { line: node.start_position().row + 1, level, module, names });
        }
        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            walk(child, src, out);
        }
    }
    let mut out = Vec::new();
    walk(tree.root_node(), src, &mut out);
    out
}

/// What is wrong with the combined change, deterministically: syntax, and imports between the project's own modules.
pub fn check(project: &Path, files: &[String]) -> Vec<Problem> {
    let mut problems: Vec<Problem> = Vec::new();
    let mut sorted: Vec<&String> = files.iter().collect();
    sorted.sort();
    sorted.dedup();
    for rel in sorted {
        let path = project.join(rel);
        if !path.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if let Some((line, message)) = syntax_error(rel, &text) {
            problems.push(Problem { check: "syntax", path: rel.clone(), line, message });
        }
        if !rel.ends_with(".py") || problems.iter().any(|p| &p.path == rel && p.check == "syntax") {
            continue;
        }
        let Some(tree) = parse_python(&text) else { continue };
        for import in from_imports(&tree, &text) {
            let module = if import.level > 0 {
                // `from .add import add` in pkg/__init__.py: resolve against the importer's package, `level` dirs up.
                let mut base = Path::new(rel).parent().map(Path::to_path_buf).unwrap_or_default();
                for _ in 1..import.level {
                    base = base.parent().map(Path::to_path_buf).unwrap_or_default();
                }
                let mut parts: Vec<String> = base.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).filter(|p| !p.is_empty()).collect();
                parts.extend(import.module.clone());
                parts.join(".")
            } else {
                match &import.module {
                    Some(m) => m.clone(),
                    None => continue,
                }
            };
            if module.is_empty() {
                continue;
            }
            let target = if import.level == 0 {
                module_file(project, &module, rel)
            } else {
                let joined = module.split('.').fold(project.to_path_buf(), |p, part| p.join(part));
                [PathBuf::from(format!("{}.py", joined.display())), joined.join("__init__.py")].into_iter().find(|c| c.is_file())
            };
            let Some(target) = target else { continue };
            let Some(names) = defined_names(&target) else { continue };
            let shown = format!("{}{}", ".".repeat(import.level), import.module.clone().unwrap_or_default());
            for name in &import.names {
                if name == "*" || names.contains(name) || module_file(project, &format!("{module}.{name}"), rel).is_some() {
                    continue; // defined, a star import, or a submodule rather than a name
                }
                let target_rel = target.strip_prefix(project).unwrap_or(&target).to_string_lossy().into_owned();
                problems.push(Problem {
                    check: "import",
                    path: rel.clone(),
                    line: Some(import.line),
                    message: format!("`from {shown} import {name}`: {target_rel} defines no `{name}`"),
                });
            }
        }
    }
    problems
}

/// The machine's [`Synthesis`]: each problem rendered as one line.
pub struct FileSynthesis;

impl Synthesis for FileSynthesis {
    fn check(&self, project: &str, files: &[String]) -> Vec<String> {
        check(Path::new(project), files).iter().map(Problem::render).collect()
    }
}
