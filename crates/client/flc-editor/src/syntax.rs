//! Tree-sitter highlighting: bundled grammars, language detection, incremental
//! reparsing and per-line highlight spans.
//!
//! Queries come from each grammar crate (C++ and TypeScript extend the C and
//! JavaScript queries, as their upstream configs do). Injections (HTML
//! `<script>`, Markdown inline) are not layered: each document is highlighted
//! by its own grammar.
use std::ops::{ControlFlow, Range};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ropey::Rope;
use tree_sitter::{
    InputEdit, Node, ParseOptions, Parser, Point, Query, QueryCursor, StreamingIterator, Tree,
};

use crate::buffer::TextChange;

/// Documents larger than this are shown as plain text.
pub const HIGHLIGHT_MAX_BYTES: usize = 4 * 1024 * 1024;
/// A parse that takes longer is abandoned (the document stays plain).
const PARSE_BUDGET: Duration = Duration::from_millis(1500);

/// The upstream YAML query is written for "last pattern wins" highlighters:
/// it captures every scalar as a string before the mapping-key patterns. Here
/// the earliest pattern wins, so keys come first.
const YAML_HIGHLIGHTS: &str = r#"
(block_mapping_pair key: (flow_node [(double_quote_scalar) (single_quote_scalar)] @property))
(block_mapping_pair key: (flow_node (plain_scalar (string_scalar) @property)))
(flow_mapping (_ key: (flow_node [(double_quote_scalar) (single_quote_scalar)] @property)))
(flow_mapping (_ key: (flow_node (plain_scalar (string_scalar) @property))))
(boolean_scalar) @boolean
(null_scalar) @constant.builtin
[(double_quote_scalar) (single_quote_scalar) (block_scalar) (string_scalar)] @string
[(integer_scalar) (float_scalar)] @number
(comment) @comment
[(anchor_name) (alias_name)] @label
(tag) @type
[(yaml_directive) (tag_directive) (reserved_directive)] @attribute
["," "-" ":" ">" "?" "|"] @punctuation.delimiter
["[" "]" "{" "}"] @punctuation.bracket
["*" "&" "---" "..."] @punctuation.special
"#;

/// The upstream TOML query captures every key as a type and paints whole
/// pairs as properties, so keys showed as types and the spaces around `=`
/// as properties. Table headers are types; keys are properties.
const TOML_HIGHLIGHTS: &str = r#"
(table (bare_key) @type)
(table (dotted_key (bare_key) @type))
(table (quoted_key) @type)
(table_array_element (bare_key) @type)
(table_array_element (dotted_key (bare_key) @type))
(table_array_element (quoted_key) @type)
(bare_key) @property
(quoted_key) @property
(boolean) @boolean
(comment) @comment
(string) @string
[(integer) (float)] @number
[(offset_date_time) (local_date_time) (local_date) (local_time)] @string.special
["." ","] @punctuation.delimiter
"=" @operator
["[" "]" "[[" "]]" "{" "}"] @punctuation.bracket
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Bash,
    C,
    CSharp,
    Cpp,
    Css,
    Go,
    Html,
    Java,
    JavaScript,
    Json,
    Lua,
    Markdown,
    Php,
    Python,
    Ruby,
    Rust,
    Sql,
    Toml,
    Tsx,
    TypeScript,
    Yaml,
}

impl Language {
    pub const ALL: [Language; 21] = [
        Self::Bash,
        Self::C,
        Self::CSharp,
        Self::Cpp,
        Self::Css,
        Self::Go,
        Self::Html,
        Self::Java,
        Self::JavaScript,
        Self::Json,
        Self::Lua,
        Self::Markdown,
        Self::Php,
        Self::Python,
        Self::Ruby,
        Self::Rust,
        Self::Sql,
        Self::Toml,
        Self::Tsx,
        Self::TypeScript,
        Self::Yaml,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "Shell",
            Self::C => "C",
            Self::CSharp => "C#",
            Self::Cpp => "C++",
            Self::Css => "CSS",
            Self::Go => "Go",
            Self::Html => "HTML",
            Self::Java => "Java",
            Self::JavaScript => "JavaScript",
            Self::Json => "JSON",
            Self::Lua => "Lua",
            Self::Markdown => "Markdown",
            Self::Php => "PHP",
            Self::Python => "Python",
            Self::Ruby => "Ruby",
            Self::Rust => "Rust",
            Self::Sql => "SQL",
            Self::Toml => "TOML",
            Self::Tsx => "TSX",
            Self::TypeScript => "TypeScript",
            Self::Yaml => "YAML",
        }
    }

    /// Detect from the file name, then from a `#!` line.
    pub fn detect(path: &str, first_line: &str) -> Option<Self> {
        Self::from_path(path).or_else(|| Self::from_shebang(first_line))
    }

    pub fn from_path(path: &str) -> Option<Self> {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let lower = name.to_ascii_lowercase();
        let by_name = match lower.as_str() {
            ".bashrc" | ".bash_profile" | ".profile" | ".zshrc" | ".zprofile" | ".envrc"
            | "pkgbuild" => Some(Self::Bash),
            "cargo.lock" | "pipfile" | "poetry.lock" | "uv.lock" => Some(Self::Toml),
            "gemfile" | "rakefile" | "podfile" | "vagrantfile" | "guardfile" => Some(Self::Ruby),
            ".babelrc" | ".eslintrc" | "tsconfig.json" | "jsconfig.json" => Some(Self::Json),
            _ => None,
        };
        if by_name.is_some() {
            return by_name;
        }
        let ext = lower.rsplit_once('.')?.1;
        Some(match ext {
            "sh" | "bash" | "zsh" | "ksh" | "bats" => Self::Bash,
            "c" | "h" => Self::C,
            "cs" | "csx" => Self::CSharp,
            "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" | "h++" | "ipp" | "inl" => {
                Self::Cpp
            }
            "css" => Self::Css,
            "go" => Self::Go,
            "html" | "htm" | "xhtml" => Self::Html,
            "java" => Self::Java,
            "js" | "mjs" | "cjs" | "jsx" => Self::JavaScript,
            "json" | "jsonc" | "json5" | "geojson" | "webmanifest" => Self::Json,
            "lua" => Self::Lua,
            "md" | "markdown" | "mdown" | "mkd" => Self::Markdown,
            "php" | "phtml" => Self::Php,
            "py" | "pyi" | "pyw" => Self::Python,
            "rb" | "rake" | "gemspec" | "ru" => Self::Ruby,
            "rs" => Self::Rust,
            "sql" => Self::Sql,
            "toml" => Self::Toml,
            "tsx" => Self::Tsx,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "yaml" | "yml" => Self::Yaml,
            _ => return None,
        })
    }

    /// `#!/usr/bin/env python3`, `#!/bin/bash -e`, …
    pub fn from_shebang(first_line: &str) -> Option<Self> {
        let rest = first_line.strip_prefix("#!")?;
        let mut words = rest.split_whitespace();
        let mut program = words.next()?.rsplit('/').next()?;
        if program == "env" {
            program = words.find(|w| !w.starts_with('-'))?;
        }
        let program = program.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
        Some(match program {
            "sh" | "bash" | "zsh" | "dash" | "ksh" => Self::Bash,
            "python" => Self::Python,
            "node" | "deno" | "bun" => Self::JavaScript,
            "ruby" => Self::Ruby,
            "php" => Self::Php,
            "lua" | "luajit" => Self::Lua,
            _ => return None,
        })
    }

    fn grammar(self) -> Option<&'static Grammar> {
        static GRAMMARS: [OnceLock<Option<Grammar>>; 21] = [const { OnceLock::new() }; 21];
        let index = Self::ALL
            .iter()
            .position(|&l| l == self)
            .expect("ALL lists every language");
        GRAMMARS[index].get_or_init(|| Grammar::load(self)).as_ref()
    }

    fn sources(self) -> (tree_sitter::Language, Vec<&'static str>) {
        match self {
            Self::Bash => (
                tree_sitter_bash::LANGUAGE.into(),
                vec![tree_sitter_bash::HIGHLIGHT_QUERY],
            ),
            Self::C => (
                tree_sitter_c::LANGUAGE.into(),
                vec![tree_sitter_c::HIGHLIGHT_QUERY],
            ),
            Self::CSharp => (
                tree_sitter_c_sharp::LANGUAGE.into(),
                vec![tree_sitter_c_sharp::HIGHLIGHTS_QUERY],
            ),
            Self::Cpp => (
                tree_sitter_cpp::LANGUAGE.into(),
                vec![
                    tree_sitter_c::HIGHLIGHT_QUERY,
                    tree_sitter_cpp::HIGHLIGHT_QUERY,
                ],
            ),
            Self::Css => (
                tree_sitter_css::LANGUAGE.into(),
                vec![tree_sitter_css::HIGHLIGHTS_QUERY],
            ),
            Self::Go => (
                tree_sitter_go::LANGUAGE.into(),
                vec![tree_sitter_go::HIGHLIGHTS_QUERY],
            ),
            Self::Html => (
                tree_sitter_html::LANGUAGE.into(),
                vec![tree_sitter_html::HIGHLIGHTS_QUERY],
            ),
            Self::Java => (
                tree_sitter_java::LANGUAGE.into(),
                vec![tree_sitter_java::HIGHLIGHTS_QUERY],
            ),
            Self::JavaScript => (
                tree_sitter_javascript::LANGUAGE.into(),
                vec![
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                ],
            ),
            Self::Json => (
                tree_sitter_json::LANGUAGE.into(),
                vec![tree_sitter_json::HIGHLIGHTS_QUERY],
            ),
            Self::Lua => (
                tree_sitter_lua::LANGUAGE.into(),
                vec![tree_sitter_lua::HIGHLIGHTS_QUERY],
            ),
            Self::Markdown => (
                tree_sitter_md::LANGUAGE.into(),
                vec![tree_sitter_md::HIGHLIGHT_QUERY_BLOCK],
            ),
            Self::Php => (
                tree_sitter_php::LANGUAGE_PHP.into(),
                vec![tree_sitter_php::HIGHLIGHTS_QUERY],
            ),
            Self::Python => (
                tree_sitter_python::LANGUAGE.into(),
                vec![tree_sitter_python::HIGHLIGHTS_QUERY],
            ),
            Self::Ruby => (
                tree_sitter_ruby::LANGUAGE.into(),
                vec![tree_sitter_ruby::HIGHLIGHTS_QUERY],
            ),
            Self::Rust => (
                tree_sitter_rust::LANGUAGE.into(),
                vec![tree_sitter_rust::HIGHLIGHTS_QUERY],
            ),
            Self::Sql => (
                arborium_sql::language().into(),
                vec![arborium_sql::HIGHLIGHTS_QUERY],
            ),
            Self::Toml => (tree_sitter_toml_ng::LANGUAGE.into(), vec![TOML_HIGHLIGHTS]),
            Self::Tsx => (
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                vec![
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                ],
            ),
            Self::TypeScript => (
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                vec![
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                ],
            ),
            Self::Yaml => (tree_sitter_yaml::LANGUAGE.into(), vec![YAML_HIGHLIGHTS]),
        }
    }
}

/// What a highlighted range is, independent of colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HighlightClass {
    /// Explicitly unhighlighted (e.g. fenced code content in Markdown).
    Plain,
    Keyword,
    Function,
    Type,
    String,
    Escape,
    Constant,
    Comment,
    Property,
    VariableBuiltin,
    Operator,
    Punctuation,
    Tag,
    Attribute,
    Label,
    Heading,
    Literal,
    Link,
    Emphasis,
}

impl HighlightClass {
    /// Map a query capture name (`keyword.return`, `string.special.key`, …).
    pub fn from_capture(name: &str) -> Option<Self> {
        let mut parts = name.split('.');
        let first = parts.next()?;
        let second = parts.next().unwrap_or("");
        let third = parts.next().unwrap_or("");
        Some(match first {
            "keyword" | "conditional" | "repeat" | "include" | "exception" | "storageclass"
            | "import" | "preproc" | "define" | "charset" | "keyframes" | "media" | "supports" => {
                Self::Keyword
            }
            "function" | "method" => Self::Function,
            "type" if second == "qualifier" => Self::Keyword,
            "type" | "namespace" | "module" | "constructor" => Self::Type,
            "string" if third == "key" => Self::Property,
            "string" if matches!(second, "escape" | "special" | "regex" | "regexp") => Self::Escape,
            "string" => Self::String,
            "character" if second == "special" => Self::Escape,
            "character" => Self::String,
            "escape" => Self::Escape,
            "number" | "float" | "boolean" | "constant" => Self::Constant,
            "comment" => Self::Comment,
            "property" | "field" => Self::Property,
            "variable" => match second {
                "builtin" => Self::VariableBuiltin,
                "member" | "field" => Self::Property,
                _ => return None,
            },
            "operator" => Self::Operator,
            "punctuation" | "delimiter" => Self::Punctuation,
            "tag" => match second {
                "attribute" => Self::Attribute,
                "delimiter" => Self::Punctuation,
                _ => Self::Tag,
            },
            "attribute" => Self::Attribute,
            "label" => Self::Label,
            "title" => Self::Heading,
            "text" | "markup" => match second {
                "title" | "heading" => Self::Heading,
                "literal" | "raw" => Self::Literal,
                "uri" | "reference" | "link" => Self::Link,
                "emphasis" | "strong" | "italic" | "bold" => Self::Emphasis,
                _ => return None,
            },
            "none" => Self::Plain,
            _ => return None,
        })
    }
}

/// A highlighted byte range inside one line's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSpan {
    pub range: Range<usize>,
    pub class: HighlightClass,
}

struct Grammar {
    language: tree_sitter::Language,
    query: Query,
    classes: Vec<Option<HighlightClass>>,
}

impl Grammar {
    fn load(language: Language) -> Option<Self> {
        let (ts_language, sources) = language.sources();
        let source = sources.join("\n");
        let query = match Query::new(&ts_language, &source) {
            Ok(query) => query,
            Err(e) => {
                eprintln!("flotilla: {} highlight query failed: {e}", language.name());
                return None;
            }
        };
        let classes = query
            .capture_names()
            .iter()
            .map(|name| HighlightClass::from_capture(name))
            .collect();
        Some(Self {
            language: ts_language,
            query,
            classes,
        })
    }
}

/// Highlighting state for one document: the parser and its current tree.
pub struct Syntax {
    language: Language,
    grammar: &'static Grammar,
    parser: Parser,
    tree: Option<Tree>,
    cursor: QueryCursor,
}

impl Syntax {
    /// `None` when the grammar or its query cannot be loaded.
    pub fn new(language: Language) -> Option<Self> {
        let grammar = language.grammar()?;
        let mut parser = Parser::new();
        parser.set_language(&grammar.language).ok()?;
        Some(Self {
            language,
            grammar,
            parser,
            tree: None,
            cursor: QueryCursor::new(),
        })
    }

    pub fn language(&self) -> Language {
        self.language
    }

    /// Whether a tree exists (the document is small enough and parsed in budget).
    pub fn is_parsed(&self) -> bool {
        self.tree.is_some()
    }

    /// Parse from scratch.
    pub fn parse(&mut self, rope: &Rope) {
        self.tree = None;
        self.reparse(rope);
    }

    /// Apply buffer edits to the tree and reparse incrementally.
    pub fn apply_changes(&mut self, changes: &[TextChange], rope: &Rope) {
        if changes.is_empty() {
            return;
        }
        if let Some(tree) = self.tree.as_mut() {
            for change in changes {
                tree.edit(&InputEdit {
                    start_byte: change.start_byte,
                    old_end_byte: change.old_end_byte,
                    new_end_byte: change.new_end_byte,
                    start_position: point(change.start_point),
                    old_end_position: point(change.old_end_point),
                    new_end_position: point(change.new_end_point),
                });
            }
        }
        self.reparse(rope);
    }

    fn reparse(&mut self, rope: &Rope) {
        if rope.len_bytes() > HIGHLIGHT_MAX_BYTES {
            self.tree = None;
            return;
        }
        let len = rope.len_bytes();
        let started = Instant::now();
        let mut progress = |_: &tree_sitter::ParseState| {
            if started.elapsed() > PARSE_BUDGET {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let tree = self.parser.parse_with_options(
            &mut |byte: usize, _: Point| -> &[u8] {
                if byte >= len {
                    return &[];
                }
                let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
                &chunk.as_bytes()[byte - chunk_start..]
            },
            self.tree.as_ref(),
            Some(ParseOptions::new().progress_callback(&mut progress)),
        );
        if tree.is_none() {
            self.parser.reset();
        }
        self.tree = tree;
    }

    /// Highlight spans for each line in `lines` (content bytes, terminators
    /// excluded). Lines past the end yield empty vectors.
    pub fn highlight_lines(&mut self, rope: &Rope, lines: Range<usize>) -> Vec<Vec<HighlightSpan>> {
        let line_count = rope.len_lines();
        let first = lines.start.min(line_count);
        let last = lines.end.min(line_count);
        let mut out = vec![Vec::new(); lines.len()];
        let Some(tree) = self.tree.as_ref() else {
            return out;
        };
        if first >= last {
            return out;
        }
        let start = rope.line_to_byte(first);
        let end = if last < line_count {
            rope.line_to_byte(last)
        } else {
            rope.len_bytes()
        };
        let paint = paint_range(&mut self.cursor, self.grammar, tree, rope, start..end);
        for (slot, line) in out.iter_mut().zip(first..last) {
            let line_start = rope.line_to_byte(line);
            let content_end = line_content_end(rope, line);
            *slot = spans_from_paint(&paint[line_start - start..content_end - start]);
        }
        out
    }
}

/// One-shot highlighting of a whole text (diff sides): spans per line.
/// Plain (all empty) when the language has no grammar or the text is too large.
pub fn highlight_text(language: Option<Language>, text: &str) -> Vec<Vec<HighlightSpan>> {
    let rope = Rope::from_str(text);
    let lines = rope.len_lines();
    match language.and_then(Syntax::new) {
        Some(mut syntax) => {
            syntax.parse(&rope);
            syntax.highlight_lines(&rope, 0..lines)
        }
        None => vec![Vec::new(); lines],
    }
}

fn point((row, column): (usize, usize)) -> Point {
    Point { row, column }
}

fn line_content_end(rope: &Rope, line: usize) -> usize {
    if line + 1 >= rope.len_lines() {
        return rope.len_bytes();
    }
    let start = rope.line_to_byte(line);
    let mut end = rope.line_to_byte(line + 1) - 1;
    if end > start && rope.byte(end - 1) == b'\r' {
        end -= 1;
    }
    end
}

/// Class per byte of `range` (0 = unhighlighted, otherwise class index + 1).
/// Inner nodes paint over outer ones; for the same node the earliest query
/// pattern wins, as in tree-sitter-highlight.
fn paint_range(
    cursor: &mut QueryCursor,
    grammar: &Grammar,
    tree: &Tree,
    rope: &Rope,
    range: Range<usize>,
) -> Vec<u8> {
    let len = rope.len_bytes();
    let text = |node: Node| {
        let r = node.byte_range();
        rope.byte_slice(r.start.min(len)..r.end.min(len))
            .chunks()
            .map(str::as_bytes)
    };
    cursor.set_byte_range(range.clone());
    let mut captures = cursor.captures(&grammar.query, tree.root_node(), text);
    let mut found: Vec<(usize, usize, usize, HighlightClass)> = Vec::new();
    while let Some((m, index)) = captures.next() {
        let capture = m.captures[*index];
        if let Some(class) = grammar.classes[capture.index as usize] {
            let r = capture.node.byte_range();
            found.push((r.start, r.end, m.pattern_index, class));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(b.2.cmp(&a.2)));
    let mut paint = vec![0u8; range.len()];
    for (start, end, _, class) in found {
        let s = start.clamp(range.start, range.end) - range.start;
        let e = end.clamp(range.start, range.end) - range.start;
        paint[s..e].fill(class as u8 + 1);
    }
    paint
}

const CLASSES: [HighlightClass; 19] = [
    HighlightClass::Plain,
    HighlightClass::Keyword,
    HighlightClass::Function,
    HighlightClass::Type,
    HighlightClass::String,
    HighlightClass::Escape,
    HighlightClass::Constant,
    HighlightClass::Comment,
    HighlightClass::Property,
    HighlightClass::VariableBuiltin,
    HighlightClass::Operator,
    HighlightClass::Punctuation,
    HighlightClass::Tag,
    HighlightClass::Attribute,
    HighlightClass::Label,
    HighlightClass::Heading,
    HighlightClass::Literal,
    HighlightClass::Link,
    HighlightClass::Emphasis,
];

fn spans_from_paint(paint: &[u8]) -> Vec<HighlightSpan> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < paint.len() {
        let value = paint[i];
        let start = i;
        while i < paint.len() && paint[i] == value {
            i += 1;
        }
        if value != 0 {
            let class = CLASSES[value as usize - 1];
            if class != HighlightClass::Plain {
                spans.push(HighlightSpan {
                    range: start..i,
                    class,
                });
            }
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Selection, TextBuffer};
    use std::collections::HashSet;

    use HighlightClass as H;

    fn classes_of(language: Language, source: &str) -> HashSet<(String, HighlightClass)> {
        let rope = Rope::from_str(source);
        let mut syntax = Syntax::new(language).expect("grammar loads");
        syntax.parse(&rope);
        assert!(syntax.is_parsed(), "{} parsed", language.name());
        let spans = syntax.highlight_lines(&rope, 0..rope.len_lines());
        let mut out = HashSet::new();
        for (line, spans) in spans.iter().enumerate() {
            let text = rope.line(line).to_string();
            for span in spans {
                out.insert((text[span.range.clone()].to_string(), span.class));
            }
        }
        out
    }

    fn assert_has(language: Language, source: &str, expected: &[(&str, HighlightClass)]) {
        let got = classes_of(language, source);
        for (text, class) in expected {
            assert!(
                got.contains(&(text.to_string(), *class)),
                "{}: expected {text:?} as {class:?}; got {got:?}",
                language.name()
            );
        }
    }

    #[test]
    fn every_bundled_grammar_loads_and_its_query_compiles() {
        for language in Language::ALL {
            assert!(
                language.grammar().is_some(),
                "{} grammar/query failed",
                language.name()
            );
        }
        assert!(Language::ALL.len() >= 20);
    }

    #[test]
    fn samples_highlight_keywords_strings_and_comments() {
        type Case<'a> = (Language, &'a str, &'a [(&'a str, HighlightClass)]);
        let cases: &[Case] = &[
            (
                Language::Rust,
                "// note\nfn main() { let s = \"hi\"; }\n",
                &[
                    ("fn", H::Keyword),
                    ("let", H::Keyword),
                    ("\"hi\"", H::String),
                    ("// note", H::Comment),
                    ("main", H::Function),
                ],
            ),
            (
                Language::Python,
                "# c\ndef f(x):\n    return 'v'\n",
                &[
                    ("def", H::Keyword),
                    ("return", H::Keyword),
                    ("'v'", H::String),
                    ("# c", H::Comment),
                ],
            ),
            (
                Language::JavaScript,
                "// c\nconst a = 'x';\nfunction f() { return <div/>; }\n",
                &[
                    ("const", H::Keyword),
                    ("function", H::Keyword),
                    ("'x'", H::String),
                    ("// c", H::Comment),
                    ("div", H::Tag),
                ],
            ),
            (
                Language::TypeScript,
                "interface P { n: number }\nconst s: string = \"q\";\n",
                &[
                    ("interface", H::Keyword),
                    ("const", H::Keyword),
                    ("\"q\"", H::String),
                    ("number", H::Type),
                ],
            ),
            (
                Language::Tsx,
                "const el = <Box id=\"a\" />;\nexport default el;\n",
                &[
                    ("const", H::Keyword),
                    ("export", H::Keyword),
                    ("\"a\"", H::String),
                ],
            ),
            (
                Language::Json,
                "{\"key\": \"value\", \"n\": 1, \"b\": true}\n",
                &[
                    ("\"key\"", H::Property),
                    ("\"value\"", H::String),
                    ("1", H::Constant),
                    ("true", H::Constant),
                ],
            ),
            (
                Language::Toml,
                "# c\n[package]\nname = \"flotilla\"\n",
                &[
                    ("name", H::Property),
                    ("\"flotilla\"", H::String),
                    ("# c", H::Comment),
                ],
            ),
            (
                Language::Yaml,
                "# c\nkey: \"value\"\nlist:\n  - 3\n",
                &[
                    ("key", H::Property),
                    ("\"value\"", H::String),
                    ("3", H::Constant),
                    ("# c", H::Comment),
                ],
            ),
            (
                Language::Markdown,
                "# Title\n\n```rust\nlet x = 1;\n```\n",
                &[
                    ("Title", H::Heading),
                    ("#", H::Punctuation),
                    ("```", H::Punctuation),
                ],
            ),
            (
                Language::Bash,
                "# c\nif [ -f x ]; then echo \"hi\"; fi\n",
                &[
                    ("if", H::Keyword),
                    ("then", H::Keyword),
                    ("\"hi\"", H::String),
                    ("# c", H::Comment),
                ],
            ),
            (
                Language::Go,
                "// c\npackage main\nfunc main() { s := \"x\" }\n",
                &[
                    ("package", H::Keyword),
                    ("func", H::Keyword),
                    ("\"x\"", H::String),
                    ("// c", H::Comment),
                ],
            ),
            (
                Language::C,
                "/* c */\nint main(void) { return \"s\"[0]; }\n",
                &[
                    ("return", H::Keyword),
                    ("\"s\"", H::String),
                    ("/* c */", H::Comment),
                    ("int", H::Type),
                ],
            ),
            (
                Language::Cpp,
                "// c\nclass A { public: int x; };\nauto s = \"v\";\n",
                &[
                    ("class", H::Keyword),
                    ("public", H::Keyword),
                    ("\"v\"", H::String),
                    ("// c", H::Comment),
                ],
            ),
            (
                Language::Java,
                "// c\npublic class A { String s = \"v\"; }\n",
                &[
                    ("public", H::Keyword),
                    ("class", H::Keyword),
                    ("\"v\"", H::String),
                    ("// c", H::Comment),
                ],
            ),
            (
                Language::Ruby,
                "# c\ndef f\n  puts 'v'\nend\n",
                &[
                    ("def", H::Keyword),
                    ("end", H::Keyword),
                    ("'v'", H::String),
                    ("# c", H::Comment),
                ],
            ),
            (
                Language::Css,
                "/* c */\n@media screen { .a { color: red; } }\n",
                &[
                    ("@media", H::Keyword),
                    ("color", H::Property),
                    ("/* c */", H::Comment),
                ],
            ),
            (
                Language::Html,
                "<!-- c -->\n<a href=\"x\">t</a>\n",
                &[
                    ("a", H::Tag),
                    ("href", H::Attribute),
                    ("x", H::String),
                    ("<!-- c -->", H::Comment),
                ],
            ),
            (
                Language::Sql,
                "-- c\nSELECT name FROM users WHERE id = 'x';\n",
                &[
                    ("SELECT", H::Keyword),
                    ("FROM", H::Keyword),
                    ("'x'", H::String),
                    ("-- c", H::Comment),
                ],
            ),
            (
                Language::Lua,
                "-- c\nlocal function f() return \"v\" end\n",
                &[
                    ("local", H::Keyword),
                    ("function", H::Keyword),
                    ("\"v\"", H::String),
                    ("-- c", H::Comment),
                ],
            ),
            (
                Language::Php,
                "<?php\n// c\nfunction f() { return 'v'; }\n",
                &[
                    ("function", H::Keyword),
                    ("return", H::Keyword),
                    ("'v'", H::String),
                    ("// c", H::Comment),
                ],
            ),
            (
                Language::CSharp,
                "// c\npublic class A { string s = \"v\"; }\n",
                &[
                    ("public", H::Keyword),
                    ("class", H::Keyword),
                    ("\"v\"", H::String),
                    ("// c", H::Comment),
                ],
            ),
        ];
        let covered: HashSet<Language> = cases.iter().map(|c| c.0).collect();
        assert_eq!(
            covered.len(),
            Language::ALL.len(),
            "a sample for every grammar"
        );
        for (language, source, expected) in cases {
            assert_has(*language, source, expected);
        }
    }

    #[test]
    fn capture_names_map_to_classes() {
        assert_eq!(
            HighlightClass::from_capture("keyword.return"),
            Some(H::Keyword)
        );
        assert_eq!(
            HighlightClass::from_capture("string.special.key"),
            Some(H::Property)
        );
        assert_eq!(
            HighlightClass::from_capture("string.escape"),
            Some(H::Escape)
        );
        assert_eq!(
            HighlightClass::from_capture("variable.builtin"),
            Some(H::VariableBuiltin)
        );
        assert_eq!(HighlightClass::from_capture("variable"), None);
        assert_eq!(HighlightClass::from_capture("spell"), None);
        assert_eq!(HighlightClass::from_capture("_name"), None);
        assert_eq!(HighlightClass::from_capture("text.title"), Some(H::Heading));
        assert_eq!(HighlightClass::from_capture("none"), Some(H::Plain));
    }

    #[test]
    fn language_detection_by_extension_name_and_shebang() {
        let cases = [
            ("src/main.rs", "", Some(Language::Rust)),
            ("C:\\proj\\App.TSX", "", Some(Language::Tsx)),
            ("lib/a.d.ts", "", Some(Language::TypeScript)),
            ("Cargo.lock", "", Some(Language::Toml)),
            ("Gemfile", "", Some(Language::Ruby)),
            ("/home/u/.bashrc", "", Some(Language::Bash)),
            ("include/x.hpp", "", Some(Language::Cpp)),
            (
                "scripts/deploy",
                "#!/usr/bin/env python3",
                Some(Language::Python),
            ),
            ("bin/run", "#!/bin/bash -e", Some(Language::Bash)),
            (
                "bin/tool",
                "#!/usr/bin/env -S node --flag",
                Some(Language::JavaScript),
            ),
            ("notes.txt", "plain", None),
            ("Makefile", "", None),
        ];
        for (path, first, expected) in cases {
            assert_eq!(Language::detect(path, first), expected, "{path}");
        }
    }

    #[test]
    fn spans_are_per_line_and_exclude_crlf_terminators() {
        let source = "let a = \"x\";\r\n/* multi\r\nline */ let b;\r\n";
        let rope = Rope::from_str(source);
        let mut syntax = Syntax::new(Language::JavaScript).unwrap();
        syntax.parse(&rope);
        let spans = syntax.highlight_lines(&rope, 0..4);
        assert_eq!(spans.len(), 4);
        // The block comment spans two lines; each line gets its own part.
        assert!(spans[1].contains(&HighlightSpan {
            range: 0..8,
            class: H::Comment
        }));
        assert!(spans[2].contains(&HighlightSpan {
            range: 0..7,
            class: H::Comment
        }));
        assert!(
            spans[2]
                .iter()
                .any(|s| s.class == H::Keyword && s.range == (8..11))
        );
        for (line, line_spans) in spans.iter().enumerate() {
            let len = rope.line(line).len_bytes();
            for span in line_spans {
                assert!(
                    span.range.end <= len.saturating_sub(2) || line == 3,
                    "{span:?}"
                );
            }
        }
        assert!(spans[3].is_empty());
        // A window in the middle highlights only those lines.
        let window = syntax.highlight_lines(&rope, 2..3);
        assert_eq!(window[0], spans[2]);
    }

    #[test]
    fn incremental_reparse_tracks_edits() {
        let mut buffer = TextBuffer::new("fn a() {}\nlet x = 1;\n");
        let mut syntax = Syntax::new(Language::Rust).unwrap();
        syntax.parse(buffer.rope());
        let before = syntax.highlight_lines(buffer.rope(), 1..2);
        assert!(
            before[0]
                .iter()
                .any(|s| s.class == H::Keyword && s.range == (0..3))
        );

        // Comment out line 2: the whole line becomes a comment.
        buffer.set_selection(Selection::caret(buffer.line_range(1).start));
        buffer.insert("// ");
        syntax.apply_changes(&buffer.take_changes(), buffer.rope());
        let after = syntax.highlight_lines(buffer.rope(), 1..2);
        assert!(
            after[0]
                .iter()
                .any(|s| s.class == H::Comment && s.range == (0..13)),
            "{after:?}"
        );

        buffer.undo();
        syntax.apply_changes(&buffer.take_changes(), buffer.rope());
        let undone = syntax.highlight_lines(buffer.rope(), 1..2);
        assert_eq!(undone, before);

        // The incremental tree matches a from-scratch parse.
        buffer.set_selection(Selection::caret(3));
        buffer.paste("main_with_a_long_name");
        syntax.apply_changes(&buffer.take_changes(), buffer.rope());
        let incremental = syntax.highlight_lines(buffer.rope(), 0..3);
        let mut fresh = Syntax::new(Language::Rust).unwrap();
        fresh.parse(buffer.rope());
        assert_eq!(incremental, fresh.highlight_lines(buffer.rope(), 0..3));
    }

    #[test]
    fn oversized_documents_stay_plain() {
        let big = "x".repeat(HIGHLIGHT_MAX_BYTES + 1);
        let rope = Rope::from_str(&big);
        let mut syntax = Syntax::new(Language::Rust).unwrap();
        syntax.parse(&rope);
        assert!(!syntax.is_parsed());
        assert_eq!(syntax.highlight_lines(&rope, 0..1), vec![Vec::new()]);
        assert_eq!(highlight_text(None, "a\nb"), vec![Vec::new(), Vec::new()]);
    }
}
