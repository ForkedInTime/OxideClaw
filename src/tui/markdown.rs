/// Lightweight markdown → ratatui Line renderer.
/// Handles the subset Claude actually produces: bold, code, headers, bullets,
/// numbered lists, hr, blockquotes, tables, and inline code/links.
///
/// Code blocks get language-aware syntax highlighting via a small built-in
/// tokenizer (no external crate required).
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

// ── Base styles ───────────────────────────────────────────────────────────────

const WHITE: Style = Style::new().fg(Color::White);
const YELLOW: Style = Style::new().fg(Color::Yellow);
const CYAN: Style = Style::new().fg(Color::Cyan);
const GRAY: Style = Style::new().fg(Color::DarkGray);

// ── Syntax highlight styles ───────────────────────────────────────────────────

// Keyword: orange-ish (matches rustyclaw accent)
const SYN_KW: Style = Style::new().fg(Color::Rgb(255, 140, 50));
// Type / builtin: cyan
const SYN_TYPE: Style = Style::new().fg(Color::Rgb(86, 182, 194));
// String literal: green
const SYN_STR: Style = Style::new().fg(Color::Rgb(152, 195, 121));
// Number: magenta/purple
const SYN_NUM: Style = Style::new().fg(Color::Rgb(198, 120, 221));
// Comment: dark gray / italic
const SYN_COMMENT: Style = Style::new()
    .fg(Color::Rgb(92, 99, 112))
    .add_modifier(Modifier::ITALIC);
// Default token (identifiers, operators …)
const SYN_DEFAULT: Style = Style::new().fg(Color::Rgb(220, 220, 200));
// Code block border/badge
const SYN_BORDER: Style = Style::new().fg(Color::Rgb(80, 80, 100));
// Code block language badge
const SYN_BADGE: Style = Style::new()
    .fg(Color::Rgb(255, 165, 0))
    .add_modifier(Modifier::BOLD);

// ── Public entry points ───────────────────────────────────────────────────────

pub fn render(text: &str) -> Vec<Line<'static>> {
    render_with_base(text, WHITE)
}

pub fn render_dim(text: &str) -> Vec<Line<'static>> {
    render_with_base(text, GRAY)
}

fn render_with_base(text: &str, base: Style) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut last_was_blank = true; // start true so we don't emit a leading blank

    let raw_lines: Vec<&str> = text.lines().collect();
    let total = raw_lines.len();
    let mut i = 0;

    while i < total {
        let raw = raw_lines[i];

        // ── Code fence toggle ─────────────────────────────────────────────────
        if raw.trim_start().starts_with("```") {
            if in_code_block {
                // Closing fence — emit a closing border line
                lines.push(Line::from(Span::styled("  ╰─", SYN_BORDER)));
                in_code_block = false;
                code_lang.clear();
            } else {
                in_code_block = true;
                code_lang = raw
                    .trim_start()
                    .trim_start_matches('`')
                    .trim()
                    .to_lowercase();

                // Opening badge: "  ╭─ rust" or "  ╭─" if no language
                let badge_line = if code_lang.is_empty() {
                    Line::from(Span::styled("  ╭─", SYN_BORDER))
                } else {
                    Line::from(vec![
                        Span::styled("  ╭─ ", SYN_BORDER),
                        Span::styled(code_lang.clone(), SYN_BADGE),
                    ])
                };
                lines.push(badge_line);
                last_was_blank = false;
            }
            i += 1;
            continue;
        }

        if in_code_block {
            lines.push(highlight_code_line(raw, &code_lang));
            last_was_blank = false;
            i += 1;
            continue;
        }

        // ── Table ─────────────────────────────────────────────────────────────
        if is_table_row(raw) {
            let table_start = i;
            let mut table_end = i;
            while table_end < total && is_table_row(raw_lines[table_end]) {
                table_end += 1;
            }
            render_table(&raw_lines[table_start..table_end], base, &mut lines);
            last_was_blank = false;
            i = table_end;
            continue;
        }

        // ── Horizontal rule ───────────────────────────────────────────────────
        let trimmed = raw.trim();
        if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            if !last_was_blank {
                lines.push(Line::raw(""));
                last_was_blank = true;
            }
            i += 1;
            continue;
        }

        // ── ATX headers ───────────────────────────────────────────────────────
        if let Some(rest) = raw.strip_prefix("### ") {
            lines.push(inline_line(rest, CYAN.add_modifier(Modifier::BOLD)));
            last_was_blank = false;
            i += 1;
            continue;
        }
        if let Some(rest) = raw.strip_prefix("## ") {
            lines.push(inline_line(rest, CYAN.add_modifier(Modifier::BOLD)));
            last_was_blank = false;
            i += 1;
            continue;
        }
        if let Some(rest) = raw.strip_prefix("# ") {
            lines.push(inline_line(rest, CYAN.add_modifier(Modifier::BOLD)));
            last_was_blank = false;
            i += 1;
            continue;
        }

        // ── Blockquote ────────────────────────────────────────────────────────
        if let Some(rest) = raw.strip_prefix("> ") {
            let mut spans = vec![Span::styled("│ ", GRAY)];
            spans.extend(inline_spans(rest, GRAY));
            lines.push(Line::from(spans));
            last_was_blank = false;
            i += 1;
            continue;
        }

        // ── Bullet list ───────────────────────────────────────────────────────
        let ltrim = raw.trim_start();
        let indent = raw.len() - ltrim.len();
        if let Some(rest) = ltrim
            .strip_prefix("- ")
            .or_else(|| ltrim.strip_prefix("* "))
        {
            let mut spans: Vec<Span<'static>> =
                vec![Span::raw(" ".repeat(indent)), Span::styled("• ", GRAY)];
            spans.extend(inline_spans(rest, base));
            lines.push(Line::from(spans));
            last_was_blank = false;
            i += 1;
            continue;
        }

        // ── Numbered list ─────────────────────────────────────────────────────
        if ltrim.len() > 2 {
            let prefix_end = ltrim.find(". ").unwrap_or(0);
            if prefix_end > 0
                && prefix_end <= 2
                && ltrim[..prefix_end].chars().all(|c| c.is_ascii_digit())
            {
                let num = &ltrim[..prefix_end + 2]; // "1. "
                let rest = &ltrim[prefix_end + 2..];
                let mut spans: Vec<Span<'static>> = vec![
                    Span::raw(" ".repeat(indent)),
                    Span::styled(num.to_string(), GRAY),
                ];
                spans.extend(inline_spans(rest, base));
                lines.push(Line::from(spans));
                last_was_blank = false;
                i += 1;
                continue;
            }
        }

        // ── Regular paragraph line with inline markup ─────────────────────────
        if raw.is_empty() {
            if !last_was_blank {
                lines.push(Line::raw(""));
                last_was_blank = true;
            }
        } else {
            lines.push(inline_line(raw, base));
            last_was_blank = false;
        }
        i += 1;
    }

    // If we ended while still inside a code block, close it
    if in_code_block {
        lines.push(Line::from(Span::styled("  ╰─", SYN_BORDER)));
    }

    lines
}

// ── Syntax highlighting ───────────────────────────────────────────────────────

/// Render one line inside a code block with language-aware highlighting.
fn highlight_code_line(line: &str, lang: &str) -> Line<'static> {
    let mut spans = vec![Span::styled("  │ ", SYN_BORDER)];
    spans.extend(tokenize_line(line, lang));
    Line::from(spans)
}

/// Tokenize a single source line into styled spans based on language.
fn tokenize_line(line: &str, lang: &str) -> Vec<Span<'static>> {
    let (keywords, types, line_comment, line_comment2) = lang_def(lang);
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut i = 0;

    while i < len {
        // ── Line comment ──────────────────────────────────────────────────────
        if !line_comment.is_empty() && starts_with_at(&chars, i, line_comment) {
            let rest: String = chars[i..].iter().collect();
            spans.push(Span::styled(rest, SYN_COMMENT));
            return spans;
        }
        if let Some(lc2) = line_comment2
            && starts_with_at(&chars, i, lc2)
        {
            let rest: String = chars[i..].iter().collect();
            spans.push(Span::styled(rest, SYN_COMMENT));
            return spans;
        }

        // ── String literal (double-quoted) ────────────────────────────────────
        if chars[i] == '"' {
            let (s, end) = consume_string(&chars, i, '"');
            spans.push(Span::styled(s, SYN_STR));
            i = end;
            continue;
        }
        // Single-quoted string (not in langs that use ' for lifetime/char differently)
        if chars[i] == '\'' && !matches!(lang, "rust" | "rs") {
            let (s, end) = consume_string(&chars, i, '\'');
            spans.push(Span::styled(s, SYN_STR));
            i = end;
            continue;
        }
        // Rust-style char/lifetime: 'a' or 'static — just treat as string if it looks like 'x'
        if chars[i] == '\'' && matches!(lang, "rust" | "rs") && i + 2 < len && chars[i + 2] == '\''
        {
            // char literal: 'x'
            let s: String = chars[i..=(i + 2)].iter().collect();
            spans.push(Span::styled(s, SYN_STR));
            i += 3;
            continue;
        }

        // ── Template literal (JS/TS) ──────────────────────────────────────────
        if chars[i] == '`'
            && matches!(
                lang,
                "javascript" | "js" | "typescript" | "ts" | "jsx" | "tsx"
            )
        {
            let (s, end) = consume_string(&chars, i, '`');
            spans.push(Span::styled(s, SYN_STR));
            i = end;
            continue;
        }

        // ── Number ────────────────────────────────────────────────────────────
        if chars[i].is_ascii_digit()
            || (chars[i] == '-'
                && i + 1 < len
                && chars[i + 1].is_ascii_digit()
                && (i == 0 || !chars[i - 1].is_alphanumeric()))
        {
            let mut n = String::new();
            // Allow leading minus only if previous was not alphanumeric
            if chars[i] == '-' {
                n.push('-');
                i += 1;
            }
            while i < len
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                n.push(chars[i]);
                i += 1;
            }
            spans.push(Span::styled(n, SYN_NUM));
            continue;
        }

        // ── Identifier / keyword ──────────────────────────────────────────────
        if chars[i].is_alphabetic() || chars[i] == '_' {
            let start = i;
            while i < len && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if keywords.contains(&word.as_str()) {
                spans.push(Span::styled(word, SYN_KW));
            } else if types.contains(&word.as_str()) {
                spans.push(Span::styled(word, SYN_TYPE));
            } else {
                spans.push(Span::styled(word, SYN_DEFAULT));
            }
            continue;
        }

        // ── Whitespace / operators / punctuation — group adjacent non-word chars
        let mut buf = String::new();
        while i < len
            && !chars[i].is_alphabetic()
            && chars[i] != '_'
            && chars[i] != '"'
            && chars[i] != '\''
            && chars[i] != '`'
            && !chars[i].is_ascii_digit()
            && !(!line_comment.is_empty() && starts_with_at(&chars, i, line_comment)
                || line_comment2.is_some_and(|lc| starts_with_at(&chars, i, lc)))
        {
            buf.push(chars[i]);
            i += 1;
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, SYN_DEFAULT));
        }
    }

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), SYN_DEFAULT));
    }
    spans
}

// ── Language definitions ──────────────────────────────────────────────────────

type LangDef = (
    &'static [&'static str],
    &'static [&'static str],
    &'static str,
    Option<&'static str>,
);
//              (keywords,                types/builtins,             line_comment,   alt_comment)

fn lang_def(lang: &str) -> LangDef {
    match lang {
        "rust" | "rs" => (
            &[
                "fn", "let", "mut", "pub", "use", "mod", "struct", "enum", "impl", "trait", "for",
                "in", "while", "loop", "if", "else", "match", "return", "async", "await", "move",
                "ref", "self", "super", "crate", "where", "type", "const", "static", "unsafe",
                "extern", "dyn", "box", "break", "continue", "true", "false", "as", "Some", "None",
                "Ok", "Err",
            ],
            &[
                "String", "Vec", "HashMap", "Option", "Result", "Box", "Arc", "Rc", "Mutex", "i8",
                "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
                "f32", "f64", "bool", "char", "str",
            ],
            "//",
            None,
        ),
        "python" | "py" => (
            &[
                "def", "class", "return", "import", "from", "as", "if", "elif", "else", "for",
                "in", "while", "break", "continue", "pass", "try", "except", "finally", "with",
                "yield", "lambda", "and", "or", "not", "is", "None", "True", "False", "async",
                "await", "global", "nonlocal", "del", "assert", "raise",
            ],
            &[
                "str", "int", "float", "bool", "list", "dict", "tuple", "set", "type", "len",
                "range", "print", "input", "open", "super", "self", "cls", "Any", "Optional",
                "List", "Dict", "Union", "Tuple", "Callable",
            ],
            "#",
            None,
        ),
        "javascript" | "js" | "jsx" => (
            &[
                "const",
                "let",
                "var",
                "function",
                "return",
                "if",
                "else",
                "for",
                "of",
                "in",
                "while",
                "do",
                "switch",
                "case",
                "break",
                "continue",
                "class",
                "extends",
                "new",
                "this",
                "super",
                "import",
                "export",
                "default",
                "from",
                "async",
                "await",
                "try",
                "catch",
                "finally",
                "throw",
                "typeof",
                "instanceof",
                "null",
                "undefined",
                "true",
                "false",
                "void",
                "delete",
                "yield",
            ],
            &[
                "console",
                "Object",
                "Array",
                "String",
                "Number",
                "Boolean",
                "Promise",
                "Error",
                "Map",
                "Set",
                "JSON",
                "Math",
                "Date",
                "RegExp",
                "Symbol",
                "parseInt",
                "parseFloat",
                "isNaN",
                "require",
                "module",
                "exports",
            ],
            "//",
            None,
        ),
        "typescript" | "ts" | "tsx" => (
            &[
                "const",
                "let",
                "var",
                "function",
                "return",
                "if",
                "else",
                "for",
                "of",
                "in",
                "while",
                "do",
                "switch",
                "case",
                "break",
                "continue",
                "class",
                "extends",
                "implements",
                "interface",
                "type",
                "enum",
                "namespace",
                "declare",
                "abstract",
                "new",
                "this",
                "super",
                "import",
                "export",
                "default",
                "from",
                "async",
                "await",
                "try",
                "catch",
                "finally",
                "throw",
                "typeof",
                "instanceof",
                "null",
                "undefined",
                "true",
                "false",
                "void",
                "never",
                "any",
                "unknown",
                "keyof",
                "readonly",
                "as",
                "satisfies",
            ],
            &[
                "string", "number", "boolean", "object", "symbol", "bigint", "Array", "Promise",
                "Record", "Partial", "Required", "Pick", "Omit", "console", "Object", "String",
                "Number", "Boolean", "Math", "Date", "JSON",
            ],
            "//",
            None,
        ),
        "bash" | "sh" | "shell" | "zsh" => (
            &[
                "if", "then", "else", "elif", "fi", "for", "do", "done", "while", "case", "esac",
                "in", "function", "return", "exit", "break", "continue", "local", "export",
                "readonly", "unset", "shift", "source", "exec", "eval",
            ],
            &[
                "echo", "printf", "read", "cd", "ls", "mkdir", "rm", "cp", "mv", "grep", "sed",
                "awk", "find", "cat", "head", "tail", "sort", "uniq", "curl", "wget", "git",
                "cargo", "npm", "python3", "python",
            ],
            "#",
            None,
        ),
        "go" => (
            &[
                "func",
                "var",
                "const",
                "type",
                "struct",
                "interface",
                "map",
                "chan",
                "for",
                "range",
                "if",
                "else",
                "switch",
                "case",
                "default",
                "break",
                "continue",
                "return",
                "goto",
                "fallthrough",
                "defer",
                "go",
                "select",
                "import",
                "package",
                "nil",
                "true",
                "false",
                "iota",
            ],
            &[
                "string",
                "int",
                "int8",
                "int16",
                "int32",
                "int64",
                "uint",
                "uint8",
                "uint16",
                "uint32",
                "uint64",
                "float32",
                "float64",
                "bool",
                "byte",
                "rune",
                "error",
                "any",
                "comparable",
                "append",
                "make",
                "new",
                "len",
                "cap",
                "delete",
                "copy",
                "close",
                "panic",
                "recover",
                "print",
                "println",
            ],
            "//",
            None,
        ),
        "sql" => (
            &[
                "SELECT", "FROM", "WHERE", "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "FULL", "ON",
                "AND", "OR", "NOT", "IN", "EXISTS", "LIKE", "BETWEEN", "IS", "NULL", "INSERT",
                "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE", "DROP", "ALTER", "TABLE",
                "VIEW", "INDEX", "DATABASE", "SCHEMA", "GROUP", "BY", "ORDER", "HAVING", "LIMIT",
                "OFFSET", "DISTINCT", "AS", "CASE", "WHEN", "THEN", "ELSE", "END", "WITH",
                // lowercase variants
                "select", "from", "where", "join", "left", "right", "inner", "outer", "full", "on",
                "and", "or", "not", "in", "exists", "like", "between", "is", "null", "insert",
                "into", "values", "update", "set", "delete", "create", "drop", "alter", "table",
                "view", "index", "database", "schema", "group", "by", "order", "having", "limit",
                "offset", "distinct", "as", "case", "when", "then", "else", "end", "with",
            ],
            &[
                "COUNT",
                "SUM",
                "AVG",
                "MIN",
                "MAX",
                "COALESCE",
                "NULLIF",
                "CAST",
                "INTEGER",
                "VARCHAR",
                "TEXT",
                "BOOLEAN",
                "TIMESTAMP",
                "DATE",
                "FLOAT",
                "count",
                "sum",
                "avg",
                "min",
                "max",
                "coalesce",
                "nullif",
                "cast",
            ],
            "--",
            Some("#"),
        ),
        "toml" => (&["true", "false"], &[], "#", None),
        "yaml" | "yml" => (
            &["true", "false", "null", "yes", "no", "on", "off"],
            &[],
            "#",
            None,
        ),
        "json" => (
            &["true", "false", "null"],
            &[],
            "",
            None, // JSON has no line comments
        ),
        "html" | "xml" => (
            &[],
            &[
                "html", "head", "body", "div", "span", "p", "a", "img", "ul", "ol", "li", "table",
                "tr", "td", "th", "form", "input", "button", "script", "style", "link", "meta",
                "title",
            ],
            "",
            None,
        ),
        "css" | "scss" | "sass" => (
            &[
                "important",
                "not",
                "has",
                "is",
                "where",
                "nth-child",
                "hover",
                "focus",
                "active",
                "disabled",
                "checked",
                "first-child",
                "last-child",
            ],
            &[
                "color",
                "background",
                "font",
                "margin",
                "padding",
                "border",
                "display",
                "flex",
                "grid",
                "position",
                "top",
                "left",
                "right",
                "bottom",
                "width",
                "height",
                "max-width",
                "min-width",
                "overflow",
                "opacity",
            ],
            "//",
            Some("/*"),
        ),
        "c" | "cpp" | "c++" | "cxx" => (
            &[
                "int",
                "char",
                "float",
                "double",
                "long",
                "short",
                "unsigned",
                "signed",
                "void",
                "bool",
                "auto",
                "const",
                "static",
                "extern",
                "register",
                "volatile",
                "if",
                "else",
                "for",
                "while",
                "do",
                "switch",
                "case",
                "default",
                "break",
                "continue",
                "return",
                "goto",
                "sizeof",
                "typedef",
                "struct",
                "union",
                "enum",
                "class",
                "public",
                "private",
                "protected",
                "virtual",
                "override",
                "final",
                "namespace",
                "using",
                "new",
                "delete",
                "this",
                "true",
                "false",
                "nullptr",
                "template",
                "typename",
                "inline",
                "explicit",
                "operator",
            ],
            &[
                "std",
                "string",
                "vector",
                "map",
                "set",
                "list",
                "array",
                "pair",
                "unique_ptr",
                "shared_ptr",
                "weak_ptr",
                "cout",
                "cin",
                "endl",
                "printf",
                "scanf",
                "malloc",
                "free",
                "NULL",
            ],
            "//",
            None,
        ),
        _ => (&[] as &[&str], &[] as &[&str], "", None),
    }
}

// ── Helpers for tokenizer ─────────────────────────────────────────────────────

/// Returns true if `chars[pos..]` starts with the given string.
/// Compares char-by-char — avoids allocating a Vec<char> for the pattern on every call.
fn starts_with_at(chars: &[char], pos: usize, s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut idx = pos;
    for sc in s.chars() {
        match chars.get(idx) {
            Some(&c) if c == sc => idx += 1,
            _ => return false,
        }
    }
    true
}

/// Consume a quoted string starting at `start` (which must be the opening quote).
/// Returns (string_text, next_index).
fn consume_string(chars: &[char], start: usize, quote: char) -> (String, usize) {
    let mut s = String::new();
    s.push(chars[start]);
    let mut i = start + 1;
    while i < chars.len() {
        let c = chars[i];
        s.push(c);
        if c == quote && (i == start + 1 || chars[i - 1] != '\\') {
            i += 1;
            break;
        }
        i += 1;
    }
    (s, i)
}

// ── Table rendering ───────────────────────────────────────────────────────────

fn is_table_row(s: &str) -> bool {
    let t = s.trim();
    t.starts_with('|') && t.len() > 1
}

fn is_separator_row(cells: &[String]) -> bool {
    cells.iter().all(|c| {
        let t = c.trim();
        !t.is_empty() && t.chars().all(|ch| ch == '-' || ch == ':' || ch == ' ')
    })
}

fn split_cells(row: &str) -> Vec<String> {
    let trimmed = row.trim().trim_start_matches('|').trim_end_matches('|');
    trimmed.split('|').map(|c| c.trim().to_string()).collect()
}

fn render_table(rows: &[&str], base: Style, lines: &mut Vec<Line<'static>>) {
    if rows.is_empty() {
        return;
    }

    let parsed: Vec<Vec<String>> = rows.iter().map(|r| split_cells(r)).collect();
    let ncols = parsed.iter().map(|r| r.len()).max().unwrap_or(0);
    if ncols == 0 {
        return;
    }

    let mut col_widths: Vec<usize> = vec![0; ncols];
    for row in &parsed {
        for (j, cell) in row.iter().enumerate() {
            if j < ncols {
                col_widths[j] = col_widths[j].max(cell.len());
            }
        }
    }

    let has_separator = parsed.len() > 1 && is_separator_row(&parsed[1]);
    let data_start = if has_separator { 2 } else { 1 };

    // Header
    {
        let header = &parsed[0];
        let mut spans: Vec<Span<'static>> = vec![Span::styled("  ", base)];
        for (j, width) in col_widths.iter().enumerate().take(ncols) {
            let cell = header.get(j).map(|s| s.as_str()).unwrap_or("");
            let padded = format!(" {:<width$} ", cell, width = *width);
            spans.push(Span::styled(padded, CYAN.add_modifier(Modifier::BOLD)));
            if j + 1 < ncols {
                spans.push(Span::styled("│", GRAY));
            }
        }
        lines.push(Line::from(spans));
    }

    // Separator
    if has_separator {
        let mut sep = String::from("  ");
        for (j, width) in col_widths.iter().enumerate().take(ncols) {
            sep.push_str(&"─".repeat(width + 2));
            if j + 1 < ncols {
                sep.push('┼');
            }
        }
        lines.push(Line::from(Span::styled(sep, GRAY)));
    }

    // Data rows
    for row in &parsed[data_start..] {
        let mut spans: Vec<Span<'static>> = vec![Span::styled("  ", base)];
        for (j, width) in col_widths.iter().enumerate().take(ncols) {
            let cell = row.get(j).map(|s| s.as_str()).unwrap_or("");
            let padded = format!(" {:<width$} ", cell, width = *width);
            let mut cell_spans = inline_spans(&padded, base);
            spans.append(&mut cell_spans);
            if j + 1 < ncols {
                spans.push(Span::styled("│", GRAY));
            }
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::raw(""));
}

// ── Inline rendering ──────────────────────────────────────────────────────────

fn inline_line(text: &str, base: Style) -> Line<'static> {
    Line::from(inline_spans(text, base))
}

/// Parse inline markdown in `text` and return styled Spans.
/// Handles: **bold**, *italic*, `code`, [link](url)
fn inline_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut current = String::new();

    let flush = |current: &mut String, spans: &mut Vec<Span<'static>>, style: Style| {
        if !current.is_empty() {
            spans.push(Span::styled(std::mem::take(current), style));
        }
    };

    while i < len {
        // **bold**
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            flush(&mut current, &mut spans, base);
            i += 2;
            let mut inner = String::new();
            while i < len {
                if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
                    i += 2;
                    break;
                }
                inner.push(chars[i]);
                i += 1;
            }
            spans.push(Span::styled(inner, base.add_modifier(Modifier::BOLD)));
            continue;
        }

        // *italic*
        if chars[i] == '*' && (i + 1 >= len || chars[i + 1] != '*') {
            flush(&mut current, &mut spans, base);
            i += 1;
            let mut inner = String::new();
            while i < len && chars[i] != '*' {
                inner.push(chars[i]);
                i += 1;
            }
            if i < len {
                i += 1;
            }
            spans.push(Span::styled(inner, base.add_modifier(Modifier::ITALIC)));
            continue;
        }

        // `inline code`
        if chars[i] == '`' {
            flush(&mut current, &mut spans, base);
            i += 1;
            let mut inner = String::new();
            while i < len && chars[i] != '`' {
                inner.push(chars[i]);
                i += 1;
            }
            if i < len {
                i += 1;
            }
            spans.push(Span::styled(inner, YELLOW));
            continue;
        }

        // [link text](url) — show link text underlined
        if chars[i] == '[' {
            let start = i + 1;
            if let Some(close) = chars[start..].iter().position(|&c| c == ']') {
                let after_bracket = start + close + 1;
                if after_bracket < len
                    && chars[after_bracket] == '('
                    && let Some(close_paren) =
                        chars[after_bracket + 1..].iter().position(|&c| c == ')')
                {
                    flush(&mut current, &mut spans, base);
                    let link_text: String = chars[start..start + close].iter().collect();
                    spans.push(Span::styled(
                        link_text,
                        CYAN.add_modifier(Modifier::UNDERLINED),
                    ));
                    i = after_bracket + 1 + close_paren + 1;
                    continue;
                }
            }
        }

        current.push(chars[i]);
        i += 1;
    }

    flush(&mut current, &mut spans, base);

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

#[cfg(test)]
mod robustness_tests {
    use super::{render, render_dim};

    /// Model output is adversarial by accident: unclosed fences, lone
    /// markers, huge lines, mixed scripts. The renderer is a parser and
    /// must never panic on any of it.
    #[test]
    fn hostile_inputs_never_panic() {
        let long = "x".repeat(200_000);
        let deep = "> ".repeat(500) + "quote";
        let nest = "* ".repeat(300) + "item";
        let cases: Vec<String> = vec![
            String::new(),
            "```".into(),
            "```rust\nfn main() {".into(),
            "```\n```\n```".into(),
            "`".into(),
            "``".into(),
            "**".into(),
            "***".into(),
            "*".into(),
            "_".into(),
            "#".into(),
            "#######".into(),
            "# ".into(),
            "1.".into(),
            "1. ".into(),
            "12. x".into(),
            "- ".into(),
            "-".into(),
            "[link".into(),
            "[](".into(),
            "](".into(),
            "![img".into(),
            "|".into(),
            "| a | b |\n|---|".into(),
            "\r\n\r\n".into(),
            "\t\t\t".into(),
            "日本語 **太字** `コード`".into(),
            "مرحبا **بالعالم**".into(),
            "e\u{301}\u{301}\u{301}".into(),
            "\u{200b}\u{200b}**\u{200b}**".into(),
            "🦀🦀🦀 ```🦀```".into(),
            long.clone(),
            format!("# {long}"),
            format!("```\n{long}\n```"),
            format!("`{long}`"),
            deep,
            nest,
            "\u{0}\u{1}\u{7f}".into(),
        ];
        for (i, c) in cases.iter().enumerate() {
            let _ = render(c);
            let _ = render_dim(c);
            let _ = i;
        }
    }

    #[test]
    fn ordinary_markdown_produces_lines() {
        let lines = render(
            "# Title\n\nSome **bold** text and `code`.\n\n- a\n- b\n\n```rs\nlet x = 1;\n```",
        );
        assert!(lines.len() >= 5, "got {} lines", lines.len());
    }
}
