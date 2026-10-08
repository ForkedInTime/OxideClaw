/// WebFetchTool — port of tools/WebFetchTool/WebFetchTool.ts
/// Fetches a URL, converts HTML to readable text, returns content.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use crate::net_policy::NetPolicy;
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

const MAX_CONTENT_BYTES: usize = 200_000; // ~50K tokens worth
/// Raw response cap. HTML-to-text is ~10:1, so this comfortably covers
/// `MAX_CONTENT_BYTES` of output without letting a hostile server stream
/// gigabytes into memory.
const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub struct WebFetchTool {
    /// Which destinations this tool may reach. See `net_policy`.
    pub policy: NetPolicy,
}

#[derive(Deserialize)]
struct WebFetchInput {
    url: String,
    prompt: String,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "WebFetch"
    }

    fn description(&self) -> &str {
        "Fetch content from a URL and extract relevant information. \
        Converts HTML to readable text. Provide a prompt describing what \
        information you want to extract from the page."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch"
                },
                "prompt": {
                    "type": "string",
                    "description": "What information to extract from the page"
                }
            },
            "required": ["url", "prompt"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let input: WebFetchInput = serde_json::from_value(input)?;

        // Scheme, host, DNS and redirect hops are all checked by the
        // policy; the body is refused past MAX_RESPONSE_BYTES. Cross-host
        // redirects are not followed: `WebFetch(domain:...)` rules only saw
        // the first host, so the new one has to come back through the gate.
        let fetched = match crate::net_policy::fetch(
            &input.url,
            &self.policy,
            MAX_RESPONSE_BYTES,
            FETCH_TIMEOUT,
            false,
        )
        .await
        {
            Ok(f) => f,
            Err(e) => return Ok(ToolOutput::error(format!("Fetch failed: {e}"))),
        };

        if let Some(target) = fetched.redirect_to {
            return Ok(ToolOutput::success(format!(
                "{} redirected to {target} (a different host). Call WebFetch again \
                 with that URL to fetch it.",
                fetched.final_url
            )));
        }

        let status = fetched.status;
        if !status.is_success() {
            return Ok(ToolOutput::error(format!("HTTP {status}: {}", input.url)));
        }
        // Media types are case-insensitive (`Text/HTML` is HTML).
        let content_type = fetched.content_type.to_ascii_lowercase();
        let mime = content_type.split(';').next().unwrap_or("").trim();
        let bytes = fetched.body;
        // Report where the content actually came from (after redirects).
        let final_url = fetched.final_url;

        // Convert to readable text. XML (feeds, sitemaps) is text as it is.
        let text = if mime.is_empty() || mime == "text/html" || mime == "application/xhtml+xml" {
            let html = crate::net_policy::decode_body(&content_type, &bytes);
            convert_html(html, CONVERT_DEADLINE, html_to_text).await
        } else if mime.starts_with("text/")
            || mime.contains("json")
            || mime == "application/xml"
            || mime.ends_with("+xml")
        {
            crate::net_policy::decode_body(&content_type, &bytes)
        } else {
            return Ok(ToolOutput::error(format!(
                "Unsupported content type: {content_type}"
            )));
        };

        let mut text = text;
        if text.len() > MAX_CONTENT_BYTES {
            // `truncate` panics off a char boundary (any non-ASCII page).
            let mut cut = MAX_CONTENT_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str("\n... (content truncated)");
        }

        // Return the content with the prompt as context header
        let output = format!(
            "Content from: {final_url}\nPrompt: {}\n\n---\n\n{}",
            input.prompt, text
        );

        Ok(ToolOutput::success(output))
    }
}

/// Element nesting past which html2text is not used. Its HTML parser walks
/// the open-element stack for many start tags, so its time grows with
/// depth × tags: 20k nested `<div>`s took 1.4 s and the ~1M a 5 MiB page
/// can hold would take hours, pinning a runtime worker that Esc cannot
/// interrupt. Real pages stay far below this.
const MAX_HTML_NESTING: usize = 512;

/// Longest run of text without a break html2text is given: it wraps a
/// long word in time that grows with its square (80k characters took
/// 0.1 s, 320k 0.7 s, a 5 MiB one minutes), even when inline tags split it.
const MAX_HTML_WORD: usize = 16 * 1024;

/// How long html2text may take before the plain tag-stripped text is used
/// instead: the backstop for nesting the depth scan does not see.
const CONVERT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

/// Conversions past their deadline that are still running. A thread cannot
/// be stopped, so past this many no more are started and every page gets
/// the plain text, which bounds the CPU hostile pages can tie up.
static OVERDUE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_OVERDUE: usize = 2;

/// Run `convert` off the async runtime, falling back to `strip_tags` at the
/// deadline. A plain thread, not `spawn_blocking`: dropping the runtime at
/// exit waits for blocking tasks, and an overdue one must not hold up quit.
async fn convert_html(
    html: String,
    deadline: std::time::Duration,
    convert: fn(&str) -> String,
) -> String {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    if OVERDUE.load(Ordering::SeqCst) >= MAX_OVERDUE {
        return strip_tags(&html);
    }
    let html = Arc::new(html);
    let overdue = Arc::new(AtomicBool::new(false));
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = {
        let (html, overdue) = (Arc::clone(&html), Arc::clone(&overdue));
        std::thread::Builder::new()
            .name("webfetch-html".into())
            .spawn(move || {
                let _ = tx.send(convert(&html));
                // Whichever of this and the deadline comes second undoes
                // the count the deadline added.
                if overdue.swap(true, Ordering::SeqCst) {
                    OVERDUE.fetch_sub(1, Ordering::SeqCst);
                }
            })
    };
    if spawned.is_err() {
        return strip_tags(&html);
    }
    match tokio::time::timeout(deadline, rx).await {
        Ok(Ok(text)) => text,
        Ok(Err(_)) => strip_tags(&html),
        Err(_) => {
            // Counted before the flag is set, so the thread's decrement
            // can never come first.
            OVERDUE.fetch_add(1, Ordering::SeqCst);
            if overdue.swap(true, Ordering::SeqCst) {
                OVERDUE.fetch_sub(1, Ordering::SeqCst);
            }
            tracing::warn!("WebFetch: HTML conversion passed {deadline:?}; using plain text");
            strip_tags(&html)
        }
    }
}

/// Convert HTML to plain readable text using html2text
fn html_to_text(html: &str) -> String {
    if max_nesting_depth(html, MAX_HTML_NESTING) > MAX_HTML_NESTING
        || longest_word(html, MAX_HTML_WORD) > MAX_HTML_WORD
    {
        return strip_tags(html);
    }
    // `from_read` .expect()s, and deeply nested lists/quotes in a fetched page
    // return TooNarrow; under panic=abort that kills the whole agent. Width
    // overflow is harmless for text that only goes to the model.
    html2text::config::plain()
        .allow_width_overflow()
        .string_from_read(html.as_bytes(), 100)
        .unwrap_or_else(|_| html.to_string())
}

/// A start or end tag found by `next_tag`.
struct Tag {
    /// Byte range of the whole tag, `<` to `>`.
    start: usize,
    end: usize,
    /// Lowercased.
    name: String,
    closing: bool,
    self_closing: bool,
}

/// The next tag at or after `from`, skipping comments, doctypes and
/// processing instructions. A `<` that starts no tag is text. Names and
/// attribute quoting follow the HTML tokenizer closely enough that a page
/// cannot hide tags from the depth scan by misquoting.
fn next_tag(b: &[u8], mut from: usize) -> Option<Tag> {
    let find = |at: usize, pat: &[u8]| {
        b.get(at..)
            .and_then(|rest| rest.windows(pat.len()).position(|w| w == pat))
            .map(|i| at + i)
    };
    loop {
        let start = from + b.get(from..)?.iter().position(|&c| c == b'<')?;
        let rest = &b[start + 1..];
        if rest.starts_with(b"!--") {
            from = find(start + 4, b"-->").map_or(b.len(), |e| e + 3);
            continue;
        }
        if matches!(rest.first(), Some(b'!' | b'?')) {
            from = find(start, b">").map_or(b.len(), |e| e + 1);
            continue;
        }
        let closing = rest.first() == Some(&b'/');
        let name_at = start + 1 + usize::from(closing);
        if !b.get(name_at).is_some_and(u8::is_ascii_alphabetic) {
            from = start + 1;
            continue;
        }
        let name_len = b[name_at..]
            .iter()
            .take_while(|c| !c.is_ascii_whitespace() && **c != b'/' && **c != b'>')
            .count();
        let name = String::from_utf8_lossy(&b[name_at..name_at + name_len]).to_ascii_lowercase();
        // A quote opens a value only right after `=`.
        let mut i = name_at + name_len;
        let mut after_eq = false;
        while i < b.len() && b[i] != b'>' {
            let c = b[i];
            if after_eq && (c == b'"' || c == b'\'') {
                i = find(i + 1, &[c]).unwrap_or(b.len());
                after_eq = false;
            } else if c == b'=' {
                after_eq = true;
            } else if !c.is_ascii_whitespace() {
                after_eq = false;
            }
            i += 1;
        }
        let end = i.min(b.len());
        let self_closing = end > name_at + name_len && b[end - 1] == b'/';
        return Some(Tag {
            start,
            end: (end + 1).min(b.len()),
            name,
            closing,
            self_closing,
        });
    }
}

/// Elements whose content is text up to their end tag, not markup.
fn is_raw_text(name: &str) -> bool {
    matches!(
        name,
        "script"
            | "style"
            | "textarea"
            | "title"
            | "xmp"
            | "iframe"
            | "noembed"
            | "noframes"
            | "noscript"
    )
}

/// Where the text of the raw-text element `name` ends: at its end tag.
fn raw_text_end(b: &[u8], from: usize, name: &str) -> usize {
    let mut at = from;
    while let Some(i) = b[at..].windows(2).position(|w| w == b"</") {
        let tag = at + i + 2;
        if b.len() >= tag + name.len()
            && b[tag..tag + name.len()].eq_ignore_ascii_case(name.as_bytes())
        {
            return at + i;
        }
        at = tag;
    }
    b.len()
}

/// The deepest element nesting in `html`, counted up to just past `limit`.
///
/// An estimate of the HTML parser's open-element stack that errs high, so a
/// page cannot nest deeply while reading as shallow: an end tag closes only
/// what the parser's rules would close (none past a "special" element like
/// `div` for an inline one, none past a scope boundary like `table` for a
/// block one), `</body>` and `</html>` close nothing, and `/>` counts only
/// in SVG and MathML. Void elements and those whose end tag is optional
/// and which close their open siblings (unclosed `<p>`/`<li>` on legacy
/// pages) are not counted. What this misses, `CONVERT_DEADLINE` catches.
fn max_nesting_depth(html: &str, limit: usize) -> usize {
    // How far an end tag looks down the stack; past it the tag is taken to
    // close nothing (erring high), which keeps the scan linear.
    const MAX_WALK: usize = 64;
    let b = html.as_bytes();
    // Each open element, and whether its content is SVG/MathML.
    let mut open: Vec<(String, bool)> = Vec::new();
    let mut deepest = 0;
    let mut at = 0;
    while let Some(tag) = next_tag(b, at) {
        at = tag.end;
        let name = tag.name.as_str();
        if tag.closing {
            match name {
                "body" | "html" | "br" => {}
                // Removed alone: what it holds stays open.
                "form" => {
                    let from = open.len().saturating_sub(MAX_WALK);
                    if let Some(i) = open[from..].iter().rposition(|(n, _)| n == "form") {
                        open.remove(from + i);
                    }
                }
                _ => {
                    let heading = is_heading(name);
                    let block = heading || is_special(name);
                    for i in (open.len().saturating_sub(MAX_WALK)..open.len()).rev() {
                        let n = open[i].0.as_str();
                        if n == name || (heading && is_heading(n)) {
                            open.truncate(i);
                            break;
                        }
                        if if block {
                            is_scope_boundary(n)
                        } else {
                            is_special(n)
                        } {
                            break;
                        }
                    }
                }
            }
            continue;
        }
        let mut foreign = open.last().is_some_and(|(_, f)| *f);
        if foreign && breaks_out_of_foreign(name) {
            while open.last().is_some_and(|(_, f)| *f) {
                open.pop();
            }
            foreign = false;
        }
        if !foreign && is_raw_text(name) {
            at = raw_text_end(b, at, name);
            continue;
        }
        let uncounted = matches!(
            name,
            "area"
                | "base"
                | "br"
                | "col"
                | "embed"
                | "hr"
                | "img"
                | "input"
                | "link"
                | "meta"
                | "param"
                | "source"
                | "track"
                | "wbr"
                | "keygen"
                | "basefont"
                | "bgsound"
                | "frame"
                | "p"
                | "li"
                | "dt"
                | "dd"
                | "tr"
                | "td"
                | "th"
                | "option"
                | "optgroup"
                | "rt"
                | "rp"
        );
        if (!foreign && uncounted) || (foreign && tag.self_closing) {
            continue;
        }
        let inner = match name {
            "svg" | "math" => true,
            "foreignobject" | "desc" | "title" | "annotation-xml" | "mi" | "mo" | "mn" | "ms"
            | "mtext" => false,
            _ => foreign,
        };
        open.push((tag.name, inner));
        deepest = deepest.max(open.len());
        if deepest > limit {
            break;
        }
    }
    deepest
}

/// The longest run of text in `html` with no whitespace or block-level tag
/// in it, in bytes (entities count as written), counted up to just past
/// `limit`. Inline and unknown tags do not end a word.
fn longest_word(html: &str, limit: usize) -> usize {
    let b = html.as_bytes();
    fn text(segment: &[u8], run: &mut usize, longest: &mut usize) {
        for &c in segment {
            if c.is_ascii_whitespace() {
                *run = 0;
            } else {
                *run += 1;
                *longest = (*longest).max(*run);
            }
        }
    }
    let (mut longest, mut run) = (0, 0);
    let mut at = 0;
    while let Some(tag) = next_tag(b, at) {
        text(&b[at..tag.start], &mut run, &mut longest);
        at = tag.end;
        if !tag.closing && is_raw_text(&tag.name) {
            let end = raw_text_end(b, at, &tag.name);
            if !matches!(tag.name.as_str(), "script" | "style") {
                text(&b[at..end], &mut run, &mut longest);
            }
            at = end;
        }
        if is_heading(&tag.name)
            || matches!(
                tag.name.as_str(),
                "address"
                    | "article"
                    | "aside"
                    | "blockquote"
                    | "body"
                    | "br"
                    | "caption"
                    | "dd"
                    | "div"
                    | "dl"
                    | "dt"
                    | "figcaption"
                    | "figure"
                    | "footer"
                    | "form"
                    | "head"
                    | "header"
                    | "hr"
                    | "html"
                    | "li"
                    | "main"
                    | "nav"
                    | "ol"
                    | "p"
                    | "pre"
                    | "section"
                    | "table"
                    | "tbody"
                    | "td"
                    | "tfoot"
                    | "th"
                    | "thead"
                    | "title"
                    | "tr"
                    | "ul"
            )
        {
            run = 0;
        }
        if longest > limit {
            return longest;
        }
    }
    text(&b[at.min(b.len())..], &mut run, &mut longest);
    longest
}

fn is_heading(name: &str) -> bool {
    matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
}

/// Where a block end tag stops looking for its element.
fn is_scope_boundary(name: &str) -> bool {
    matches!(
        name,
        "applet" | "caption" | "html" | "table" | "td" | "th" | "marquee" | "object" | "template"
    )
}

/// The HTML parser's "special" elements that can be open.
fn is_special(name: &str) -> bool {
    is_scope_boundary(name)
        || is_heading(name)
        || matches!(
            name,
            "address"
                | "article"
                | "aside"
                | "blockquote"
                | "body"
                | "button"
                | "center"
                | "colgroup"
                | "dd"
                | "details"
                | "dir"
                | "div"
                | "dl"
                | "dt"
                | "fieldset"
                | "figcaption"
                | "figure"
                | "footer"
                | "form"
                | "frameset"
                | "head"
                | "header"
                | "hgroup"
                | "iframe"
                | "li"
                | "listing"
                | "main"
                | "menu"
                | "nav"
                | "noembed"
                | "noframes"
                | "noscript"
                | "ol"
                | "p"
                | "plaintext"
                | "pre"
                | "script"
                | "search"
                | "section"
                | "select"
                | "style"
                | "summary"
                | "tbody"
                | "textarea"
                | "tfoot"
                | "thead"
                | "title"
                | "tr"
                | "ul"
                | "xmp"
        )
}

/// HTML start tags that close an open `<svg>` or `<math>`.
fn breaks_out_of_foreign(name: &str) -> bool {
    is_heading(name)
        || matches!(
            name,
            "b" | "big"
                | "blockquote"
                | "body"
                | "br"
                | "center"
                | "code"
                | "dd"
                | "div"
                | "dl"
                | "dt"
                | "em"
                | "embed"
                | "font"
                | "head"
                | "hr"
                | "i"
                | "img"
                | "li"
                | "listing"
                | "menu"
                | "meta"
                | "nobr"
                | "ol"
                | "p"
                | "pre"
                | "ruby"
                | "s"
                | "small"
                | "span"
                | "strong"
                | "strike"
                | "sub"
                | "sup"
                | "table"
                | "tt"
                | "u"
                | "ul"
                | "var"
        )
}

/// Linear HTML-to-text for pages html2text would take too long on: tags
/// dropped, block boundaries as line breaks, common entities decoded,
/// whitespace collapsed.
fn strip_tags(html: &str) -> String {
    let b = html.as_bytes();
    let mut raw = String::with_capacity(html.len() / 2);
    let mut at = 0;
    while let Some(tag) = next_tag(b, at) {
        raw.push_str(&html[at..tag.start]);
        at = tag.end;
        if !tag.closing && is_raw_text(&tag.name) && tag.name != "title" {
            at = raw_text_end(b, at, &tag.name);
            continue;
        }
        let block = matches!(
            tag.name.as_str(),
            "address"
                | "article"
                | "aside"
                | "blockquote"
                | "br"
                | "dd"
                | "div"
                | "dl"
                | "dt"
                | "figcaption"
                | "figure"
                | "footer"
                | "form"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "header"
                | "hr"
                | "li"
                | "main"
                | "nav"
                | "ol"
                | "p"
                | "pre"
                | "section"
                | "table"
                | "title"
                | "tr"
                | "ul"
        );
        raw.push(if block { '\n' } else { ' ' });
    }
    raw.push_str(&html[at.min(html.len())..]);

    let mut out = String::with_capacity(raw.len());
    for line in decode_entities(&raw).lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.is_empty() {
            if !out.is_empty() && !out.ends_with("\n\n") {
                out.push('\n');
            }
            continue;
        }
        out.push_str(&words.join(" "));
        out.push('\n');
    }
    out.trim().to_string()
}

/// `&amp;`, `&lt;`, `&gt;`, `&quot;`, `&apos;`, `&#39;`-style numeric
/// references and `&nbsp;`; anything else is left as written.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest[..rest.len().min(12)].find(';');
        let decoded = end.and_then(|e| {
            let name = &rest[1..e];
            let c = match name {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => ' ',
                _ => {
                    let num = name.strip_prefix('#')?;
                    let code = match num.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => num.parse().ok()?,
                    };
                    char::from_u32(code)?
                }
            };
            Some((c, e + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::ToolResultContent;
    use crate::net_policy::test_support::{ok_with, redirect, scripted_server};
    use std::sync::atomic::Ordering;

    async fn run(policy: NetPolicy, url: &str) -> ToolOutput {
        let tool = WebFetchTool { policy };
        let ctx = ToolContext::new(std::env::temp_dir());
        tool.execute(json!({"url": url, "prompt": "x"}), &ctx)
            .await
            .expect("policy refusals are tool errors, not Err")
    }

    fn text(o: &ToolOutput) -> String {
        o.content
            .iter()
            .map(|c| match c {
                ToolResultContent::Text { text } => text.as_str(),
            })
            .collect()
    }

    /// Deep nesting used to hit html2text's TooNarrow `.expect()` and abort.
    #[test]
    fn deeply_nested_html_converts_without_panicking() {
        for tag in ["<ol><li>", "<ul><li>", "<blockquote>"] {
            let html = format!("{}leaf-text", tag.repeat(200));
            // Overflowed lines wrap mid-word under list/quote prefixes; only
            // the content matters.
            let out: String = html_to_text(&html)
                .chars()
                .filter(|c| c.is_alphabetic() || *c == '-')
                .collect();
            assert!(out.contains("leaf-text"), "{tag}");
        }
    }

    /// html2text is super-linear in nesting depth and word length: 100k
    /// nested `<div>`s (a 500 KB page) took 83 s, a 5 MiB one hours,
    /// freezing the turn and quit. Past either limit the page is
    /// tag-stripped.
    #[test]
    fn hostile_markup_converts_quickly_and_keeps_the_text() {
        // Plain nesting, and end tags or `/>` the parser ignores or that
        // close less than their name suggests.
        let cases = [
            ("", "<div>"),
            ("", "<ul><li>"),
            ("", "<span><div></span>"),
            ("", "<h1><span><h2></h1>"),
            ("", "<form><div></form>"),
            ("", "<div></body>"),
            ("<svg>", "<div/>"),
            ("", "<svg><title><div>"),
            // One unbroken word, plain or split by inline tags.
            ("", "aaaaa"),
            ("", "&lt;&gt;"),
            ("", "a<x>a</x>"),
        ];
        for (head, tag) in cases {
            let html = format!(
                "<p>intro &amp; more</p>{head}{}leaf-text",
                tag.repeat(100_000)
            );
            let started = std::time::Instant::now();
            let out = html_to_text(&html);
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "{tag}: {:?}",
                started.elapsed()
            );
            assert!(out.contains("leaf-text"), "{tag}");
            assert!(out.contains("intro & more"), "{tag}: {out:.100}");
        }
    }

    /// Unclosed `<p>`/`<li>`, void elements, SVG's `/>`, comments, scripts
    /// and quoted `>` must not read as nesting, or ordinary pages would lose
    /// html2text's formatting.
    #[test]
    fn ordinary_markup_is_not_counted_as_deep() {
        let page = format!(
            "<!DOCTYPE html><html><head><title>t</title><script>{}</script></head><body>{}</body></html>",
            "if (a<b) { x = '<div>'; }".repeat(1000),
            "<p>para<br><img src=x><li>item<!-- <div><div> --><svg><path d='M0'/></svg>\
             <a title='a > b'>link</a><div class=\"x\">ok</div>"
                .repeat(1000),
        );
        assert!(max_nesting_depth(&page, MAX_HTML_NESTING) <= 4);
        // Minified markup has no whitespace between blocks.
        let minified = "<ul><li><a href='/x'>Home</a></li><li><b>About</b></li></ul>".repeat(5000);
        assert!(longest_word(&minified, MAX_HTML_WORD) <= 5);
        assert!(longest_word(&page, MAX_HTML_WORD) <= 20);
        let deep = "<div>".repeat(MAX_HTML_NESTING + 1);
        assert_eq!(
            max_nesting_depth(&deep, MAX_HTML_NESTING),
            MAX_HTML_NESTING + 1
        );
        // A `"` outside a value does not hide the tags after it.
        let sneaky = "<div a\"><div>".repeat(MAX_HTML_NESTING);
        assert!(max_nesting_depth(&sneaky, MAX_HTML_NESTING) > MAX_HTML_NESTING);
    }

    #[test]
    fn stripped_text_keeps_blocks_and_entities() {
        let out = strip_tags(
            "<h1>Title</h1><script>var x = '<p>';</script><p>a &lt;b&gt; &#65;&#x42; &bogus; \
             c</p><style>p{}</style><div>  d\n\n\n\n e</div>",
        );
        assert_eq!(out, "Title\n\na <b> AB &bogus; c\n\nd\n\ne");
    }

    /// A conversion the depth scan misses must still not hold the turn.
    #[tokio::test]
    async fn slow_conversion_falls_back_at_the_deadline() {
        fn slow(_: &str) -> String {
            std::thread::sleep(std::time::Duration::from_secs(2));
            "converted".into()
        }
        let started = std::time::Instant::now();
        let out = convert_html(
            "<b>plain</b> text".into(),
            std::time::Duration::from_millis(100),
            slow,
        )
        .await;
        assert_eq!(out, "plain text");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let out = convert_html("<b>x</b>".into(), std::time::Duration::from_secs(10), |h| {
            h.len().to_string()
        })
        .await;
        assert_eq!(out, "8");
    }

    /// The default policy must refuse loopback *before connecting*, and
    /// report it as a tool error the model can read.
    #[tokio::test]
    async fn strict_policy_refuses_loopback_without_connecting() {
        let (base, hits) = scripted_server(vec![ok_with("text/html", "<p>secret</p>")]).await;
        let out = run(NetPolicy::STRICT, &base).await;
        assert!(out.is_error);
        assert!(text(&out).contains("private"), "{}", text(&out));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn local_ok_policy_fetches_and_converts_html() {
        let (base, _) =
            scripted_server(vec![ok_with("text/html", "<p>Hello <b>there</b></p>")]).await;
        let out = run(NetPolicy::LOCAL_OK, &base).await;
        assert!(!out.is_error, "{}", text(&out));
        assert!(text(&out).contains("Hello there"), "{}", text(&out));
    }

    /// The body was always read as UTF-8. The script server only sends
    /// strings, so declare windows-1252 over the UTF-8 bytes of "é"
    /// (C3 A9), which that charset reads as "Ã©".
    #[tokio::test]
    async fn body_is_decoded_by_the_declared_charset() {
        let (base, _) = scripted_server(vec![
            ok_with("text/plain; charset=windows-1252", "caf\u{e9}"),
            ok_with("text/html; charset=windows-1252", "<p>caf\u{e9}</p>"),
        ])
        .await;
        for _ in 0..2 {
            let out = run(NetPolicy::LOCAL_OK, &base).await;
            assert!(text(&out).contains("caf\u{c3}\u{a9}"), "{}", text(&out));
        }
    }

    /// A redirect to another host is reported, not followed, so the next
    /// hop goes through the permission gate's domain rules.
    #[tokio::test]
    async fn cross_host_redirect_is_returned_to_the_model() {
        let (other_base, other_hits) = scripted_server(vec![ok_with("text/plain", "denied")]).await;
        let target = format!("{}/raw", other_base.replace("127.0.0.1", "localhost"));
        let (base, _) = scripted_server(vec![redirect(&target)]).await;
        let out = run(NetPolicy::LOCAL_OK, &base).await;
        assert!(!out.is_error, "{}", text(&out));
        assert!(text(&out).contains(&target), "{}", text(&out));
        assert!(text(&out).contains("Call WebFetch again"), "{}", text(&out));
        assert!(!text(&out).contains("denied"));
        assert_eq!(other_hits.load(Ordering::SeqCst), 0);
    }

    /// Media types were matched case-sensitively and XHTML, RSS, Atom and
    /// plain XML were refused as unsupported.
    #[tokio::test]
    async fn xhtml_xml_feeds_and_mixed_case_types_are_read() {
        let (base, _) = scripted_server(vec![
            ok_with("Text/HTML; Charset=UTF-8", "<p>Hello <b>html</b></p>"),
            ok_with("application/xhtml+xml", "<p>Hello <b>xhtml</b></p>"),
            ok_with("application/rss+xml", "<rss><title>feed</title></rss>"),
            ok_with("application/atom+xml", "<feed><title>atom</title></feed>"),
            ok_with("Application/XML", "<urlset>sitemap</urlset>"),
            ok_with("application/octet-stream", "bin"),
        ])
        .await;
        for want in [
            "Hello html",
            "Hello xhtml",
            "<title>feed</title>",
            "<title>atom</title>",
            "<urlset>sitemap</urlset>",
        ] {
            let out = run(NetPolicy::LOCAL_OK, &base).await;
            assert!(!out.is_error, "{want}: {}", text(&out));
            assert!(text(&out).contains(want), "{want}: {}", text(&out));
        }
        let out = run(NetPolicy::LOCAL_OK, &base).await;
        assert!(out.is_error);
        assert!(
            text(&out).contains("Unsupported content type"),
            "{}",
            text(&out)
        );
    }

    #[tokio::test]
    async fn oversized_response_is_refused_as_a_tool_error() {
        let resp = "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\n\
                    content-length: 99999999\r\nconnection: close\r\n\r\nx";
        let (base, _) = scripted_server(vec![resp.to_string()]).await;
        let out = run(NetPolicy::LOCAL_OK, &base).await;
        assert!(out.is_error);
        assert!(text(&out).contains("too large"), "{}", text(&out));
    }
}
