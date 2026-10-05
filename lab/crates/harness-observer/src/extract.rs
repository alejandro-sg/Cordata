//! tree-sitter: a source file → the functions, classes and types in it.
//!
//! The point is to send the daemon *what changed*, so the unit is the symbol — a function, a
//! method, a class, an `impl` — not the file. Each [`Symbol`] carries its qualified name
//! (`Type::method`, `Class.method`), its line range, and its **own text**: the node's source with
//! the bodies of the symbols nested inside it cut out. That last part is what keeps a one-line
//! change to a method from also reporting its class, its `impl` block and its module as modified:
//! the class's own text does not contain the method.
//!
//! Languages: Rust, Python, TypeScript (+ TSX), JavaScript. Parsing is tolerant — tree-sitter
//! recovers from syntax errors, so a half-typed file still yields the symbols that are intact.

use std::collections::HashMap;
use std::path::Path;

use sha2::{Digest, Sha256};
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
}

impl Lang {
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::JavaScript => "javascript",
        }
    }

    fn language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        }
    }

    fn separator(self) -> &'static str {
        match self {
            Self::Rust => "::",
            _ => ".",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// The grammar's node kind: `function_item`, `class_definition`, `method_definition`, …
    pub kind: String,
    /// `Type::method` / `Class.method` / plain name.
    pub name: String,
    /// 1-based, inclusive.
    pub start_line: u32,
    pub end_line: u32,
    /// The symbol's source minus the symbols nested in it (replaced by `…`).
    pub own_text: String,
    /// `sha256(own_text)`, hex, 32 chars: what "changed" means.
    pub hash: String,
    parent: Option<usize>,
}

pub fn hash_text(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

struct Found {
    kind: String,
    name: String,
    parent: Option<usize>,
}

/// What a node is, for the walk: does it name a symbol worth reporting, and does it qualify the
/// names inside it?
struct Role<'t> {
    name: String,
    /// Report it as a symbol.
    symbol: bool,
    /// Prefix the names of everything inside it with this name.
    scope: bool,
    /// The node whose range is the symbol's (a decorated definition's range includes its decorators).
    range: Node<'t>,
}

fn field_text(node: Node, field: &str, source: &[u8]) -> Option<String> {
    node.child_by_field_name(field).and_then(|n| n.utf8_text(source).ok()).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

fn classify<'t>(lang: Lang, node: Node<'t>, source: &[u8]) -> Option<Role<'t>> {
    let kind = node.kind();
    let plain = |name: Option<String>, scope: bool| name.map(|name| Role { name, symbol: true, scope, range: node });
    match lang {
        Lang::Rust => match kind {
            "function_item" => plain(field_text(node, "name", source), true),
            "struct_item" | "enum_item" | "union_item" | "type_item" | "const_item" | "static_item" | "macro_definition" => plain(field_text(node, "name", source), false),
            "trait_item" | "mod_item" => {
                // A bodiless `mod x;` is a declaration, not a thing with code in it.
                if kind == "mod_item" && node.child_by_field_name("body").is_none() {
                    return None;
                }
                plain(field_text(node, "name", source), true)
            }
            "impl_item" => {
                let ty = field_text(node, "type", source)?;
                let name = match field_text(node, "trait", source) {
                    Some(tr) => format!("<{ty} as {tr}>"),
                    None => ty,
                };
                plain(Some(name), true)
            }
            _ => None,
        },
        Lang::Python => match kind {
            "function_definition" | "class_definition" => {
                // Include the decorators in the symbol's range by using the wrapping node.
                let name = field_text(node, "name", source)?;
                let range = match node.parent() {
                    Some(parent) if parent.kind() == "decorated_definition" => parent,
                    _ => node,
                };
                Some(Role { name, symbol: true, scope: true, range })
            }
            _ => None,
        },
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => match kind {
            "function_declaration" | "generator_function_declaration" => plain(field_text(node, "name", source), true),
            "class_declaration" | "abstract_class_declaration" => plain(field_text(node, "name", source), true),
            "method_definition" | "method_signature" | "abstract_method_signature" => plain(field_text(node, "name", source), true),
            "interface_declaration" | "type_alias_declaration" | "enum_declaration" => plain(field_text(node, "name", source), false),
            "variable_declarator" => {
                // `const handler = (…) => …` / `const f = function …`: a function in everything but syntax.
                let value = node.child_by_field_name("value")?;
                if !matches!(value.kind(), "arrow_function" | "function_expression" | "function" | "generator_function") {
                    return None;
                }
                let name = field_text(node, "name", source)?;
                // The whole statement (`const …;` / `export const …;`) is the symbol's range.
                let mut range = node;
                while let Some(parent) = range.parent() {
                    if matches!(parent.kind(), "lexical_declaration" | "variable_declaration" | "export_statement") {
                        range = parent;
                    } else {
                        break;
                    }
                }
                Some(Role { name, symbol: true, scope: true, range })
            }
            _ => None,
        },
    }
}

/// Every symbol in `source`, in document order.
pub fn symbols(lang: Lang, source: &str) -> Vec<Symbol> {
    let mut parser = Parser::new();
    if parser.set_language(&lang.language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else { return Vec::new() };
    let bytes = source.as_bytes();
    let root = tree.root_node();

    // Walk, remembering each symbol's parent symbol and the qualified-name prefix.
    let mut found: Vec<Found> = Vec::new();
    let mut ranges: Vec<Node> = Vec::new();
    fn walk<'t>(lang: Lang, node: Node<'t>, source: &[u8], prefix: &str, parent: Option<usize>, found: &mut Vec<Found>, ranges: &mut Vec<Node<'t>>) {
        let mut child_prefix = prefix.to_owned();
        let mut child_parent = parent;
        if let Some(role) = classify(lang, node, source) {
            let qualified = if prefix.is_empty() { role.name.clone() } else { format!("{prefix}{}{}", lang.separator(), role.name) };
            if role.symbol {
                found.push(Found { kind: node.kind().to_owned(), name: qualified.clone(), parent });
                ranges.push(role.range);
                child_parent = Some(found.len() - 1);
            }
            if role.scope {
                child_prefix = qualified;
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(lang, child, source, &child_prefix, child_parent, found, ranges);
        }
    }
    walk(lang, root, bytes, "", None, &mut found, &mut ranges);

    // Own text: the symbol's source with the symbols directly nested in it cut out.
    let mut children_of: HashMap<usize, Vec<usize>> = HashMap::new();
    for (index, symbol) in found.iter().enumerate() {
        if let Some(parent) = symbol.parent {
            children_of.entry(parent).or_default().push(index);
        }
    }
    let mut out = Vec::with_capacity(found.len());
    for (index, symbol) in found.iter().enumerate() {
        let range = ranges[index];
        let (start, end) = (range.start_byte(), range.end_byte());
        let mut cuts: Vec<(usize, usize)> = children_of
            .get(&index)
            .map(|kids| kids.iter().map(|&k| (ranges[k].start_byte().max(start), ranges[k].end_byte().min(end))).collect())
            .unwrap_or_default();
        cuts.sort_unstable();
        let mut text = String::new();
        let mut at = start;
        for (cut_start, cut_end) in cuts {
            if cut_start < at {
                continue; // overlapping: already inside a cut
            }
            text.push_str(&String::from_utf8_lossy(&bytes[at..cut_start]));
            text.push('…');
            at = cut_end;
        }
        text.push_str(&String::from_utf8_lossy(&bytes[at..end]));
        out.push(Symbol {
            kind: symbol.kind.clone(),
            name: symbol.name.clone(),
            start_line: range.start_position().row as u32 + 1,
            end_line: range.end_position().row as u32 + 1,
            hash: hash_text(&text),
            own_text: text,
            parent: symbol.parent,
        });
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Added,
    Modified,
}

/// A symbol's identity across two versions of a file: its kind and qualified name, plus which
/// occurrence it is when a file defines the same name twice (overloads, `cfg`-gated twins).
pub type Key = (String, String, usize);

pub fn keyed(symbols: Vec<Symbol>) -> Vec<(Key, Symbol)> {
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    symbols
        .into_iter()
        .map(|symbol| {
            let counter = seen.entry((symbol.kind.clone(), symbol.name.clone())).or_insert(0);
            let key = (symbol.kind.clone(), symbol.name.clone(), *counter);
            *counter += 1;
            (key, symbol)
        })
        .collect()
}

/// The symbols of `new` that are not in `old` (`Added`) or whose own text differs (`Modified`).
/// `old = None` means there is nothing to compare to, so everything is new.
pub fn changed(old: Option<&[(Key, Symbol)]>, new: &[(Key, Symbol)]) -> Vec<(Change, Symbol)> {
    let before: HashMap<&Key, &Symbol> = old.map(|rows| rows.iter().map(|(key, symbol)| (key, symbol)).collect()).unwrap_or_default();
    new.iter()
        .filter_map(|(key, symbol)| match before.get(key) {
            None => Some((Change::Added, symbol.clone())),
            Some(previous) if previous.hash != symbol.hash => Some((Change::Modified, symbol.clone())),
            Some(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(symbols: &[Symbol]) -> Vec<String> {
        symbols.iter().map(|s| s.name.clone()).collect()
    }

    #[test]
    fn rust_functions_methods_and_types() {
        let source = "\
use std::fmt;

pub struct Parser { pos: usize }

impl Parser {
    pub fn new() -> Self {
        Parser { pos: 0 }
    }

    fn advance(&mut self) {
        self.pos += 1;
    }
}

impl fmt::Display for Parser {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, \"{}\", self.pos)
    }
}

pub fn parse(input: &str) -> usize {
    input.len()
}

mod tests {
    fn helper() {}
}
";
        let found = symbols(Lang::Rust, source);
        assert_eq!(names(&found), ["Parser", "Parser", "Parser::new", "Parser::advance", "<Parser as fmt::Display>", "<Parser as fmt::Display>::fmt", "parse", "tests", "tests::helper"]);
        let advance = found.iter().find(|s| s.name == "Parser::advance").unwrap();
        assert_eq!((advance.start_line, advance.end_line), (10, 12));
        assert!(advance.own_text.contains("self.pos += 1"));
        let imp = found.iter().find(|s| s.kind == "impl_item" && s.name == "Parser").unwrap();
        assert!(!imp.own_text.contains("self.pos += 1") && imp.own_text.contains('…'), "an impl's own text must not contain its methods: {:?}", imp.own_text);
    }

    #[test]
    fn python_classes_methods_and_decorators() {
        let source = "\
import os

@dataclass
class Config:
    name: str

    def load(self):
        return os.getcwd()

    @staticmethod
    def default():
        return Config('x')


def main():
    def inner():
        pass
    return Config.default()
";
        let found = symbols(Lang::Python, source);
        assert_eq!(names(&found), ["Config", "Config.load", "Config.default", "main", "main.inner"]);
        let class = &found[0];
        assert_eq!(class.start_line, 3, "a decorated class starts at its decorator");
        assert!(class.own_text.starts_with("@dataclass") && !class.own_text.contains("os.getcwd"), "{:?}", class.own_text);
        let default = found.iter().find(|s| s.name == "Config.default").unwrap();
        assert!(default.own_text.starts_with("@staticmethod"), "{:?}", default.own_text);
    }

    #[test]
    fn typescript_classes_arrow_functions_and_interfaces() {
        let source = "\
export interface Props { id: number }

export class Store {
  count = 0;
  inc(): void { this.count += 1; }
}

export const render = (props: Props) => {
  return props.id;
};

function helper(x: number) { return x * 2; }

type Id = string;
";
        let found = symbols(Lang::TypeScript, source);
        assert_eq!(names(&found), ["Props", "Store", "Store.inc", "render", "helper", "Id"]);
        let render = found.iter().find(|s| s.name == "render").unwrap();
        assert!(render.own_text.starts_with("export const render"), "{:?}", render.own_text);
    }

    #[test]
    fn javascript_and_tsx_parse() {
        assert_eq!(names(&symbols(Lang::JavaScript, "function a() {}\nclass B { m() {} }\nconst c = function() {};")), ["a", "B", "B.m", "c"]);
        assert_eq!(names(&symbols(Lang::Tsx, "export function App() { return <div/>; }")), ["App"]);
    }

    #[test]
    fn a_broken_file_still_yields_the_intact_symbols() {
        let found = symbols(Lang::Rust, "fn good() {}\nfn broken( {\nfn also_good() {}\n");
        assert!(names(&found).contains(&"good".to_owned()), "{found:?}");
    }

    #[test]
    fn editing_one_method_changes_only_that_method() {
        let before = keyed(symbols(Lang::Rust, "struct A;\nimpl A {\n    fn one(&self) -> i32 { 1 }\n    fn two(&self) -> i32 { 2 }\n}\nfn free() {}\n"));
        let after = keyed(symbols(Lang::Rust, "struct A;\nimpl A {\n    fn one(&self) -> i32 { 1 }\n    fn two(&self) -> i32 { 22 }\n}\nfn free() {}\n"));
        let diff = changed(Some(&before), &after);
        assert_eq!(diff.len(), 1, "{diff:?}");
        assert_eq!((diff[0].0, diff[0].1.name.as_str()), (Change::Modified, "A::two"));
    }

    #[test]
    fn a_new_function_is_added_and_a_removed_one_is_silent() {
        let before = keyed(symbols(Lang::Python, "def a():\n    pass\n\ndef b():\n    pass\n"));
        let after = keyed(symbols(Lang::Python, "def a():\n    pass\n\ndef c():\n    pass\n"));
        let diff = changed(Some(&before), &after);
        assert_eq!(diff.iter().map(|(c, s)| (*c, s.name.as_str())).collect::<Vec<_>>(), [(Change::Added, "c")]);
    }

    #[test]
    fn without_a_baseline_everything_is_added() {
        let all = keyed(symbols(Lang::Rust, "fn a() {}\nfn b() {}\n"));
        assert_eq!(changed(None, &all).len(), 2);
    }

    #[test]
    fn twin_definitions_are_told_apart_by_occurrence() {
        let before = keyed(symbols(Lang::Rust, "#[cfg(unix)]\nfn f() { 1 }\n#[cfg(windows)]\nfn f() { 2 }\n"));
        let after = keyed(symbols(Lang::Rust, "#[cfg(unix)]\nfn f() { 1 }\n#[cfg(windows)]\nfn f() { 3 }\n"));
        let diff = changed(Some(&before), &after);
        assert_eq!(diff.len(), 1, "{diff:?}");
        assert!(diff[0].1.own_text.contains('3'));
    }

    #[test]
    fn language_by_extension() {
        for (name, lang) in [("a.rs", Lang::Rust), ("a.py", Lang::Python), ("a.ts", Lang::TypeScript), ("a.tsx", Lang::Tsx), ("a.js", Lang::JavaScript), ("a.jsx", Lang::JavaScript)] {
            assert_eq!(Lang::from_path(Path::new(name)), Some(lang), "{name}");
        }
        assert_eq!(Lang::from_path(Path::new("a.txt")), None);
        assert_eq!(Lang::from_path(Path::new("Makefile")), None);
    }
}
