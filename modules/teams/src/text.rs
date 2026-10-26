//! Teams HTML to the Markdown agents read, and the Markdown they write back to HTML (spec 8.5).
//! Teams uses a small HTML subset (docs/protocols/teams.md 3.3), so a hand-written tokenizer is
//! enough. Hostile input degrades to text: unknown tags are dropped with their text kept,
//! `<script>` and `<style>` with their content, and nothing panics.

/// Entities longer than this are left as text, which bounds the scan after each `&`.
const MAX_ENTITY: usize = 12;
/// Deeper Markdown nesting stays literal text, which bounds the recursion.
const MAX_NESTING: u8 = 4;

pub fn html_to_markdown(html: &str) -> String {
    let mut out = String::new();
    let mut link: Option<String> = None;
    let mut in_pre = false;
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        let (text, tail) = rest.split_at(i);
        push_text(&mut out, text);
        let after = tail.get(1..).unwrap_or("");
        if let Some(comment) = after.strip_prefix("!--") {
            rest = comment.split_once("-->").map_or("", |(_, r)| r);
            continue;
        }
        if !after.starts_with(|c: char| c.is_ascii_alphabetic() || c == '/' || c == '!') {
            out.push('<');
            rest = after;
            continue;
        }
        let Some((tag, after_tag)) = after.split_once('>') else {
            // An unterminated tag: what follows is not text.
            rest = "";
            break;
        };
        rest = after_tag;
        let closing = tag.starts_with('/');
        let tag = tag.trim_start_matches('/');
        let (name, attrs) = tag.split_at(tag.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(tag.len()));
        let name = name.to_ascii_lowercase();
        match (name.as_str(), closing) {
            ("script" | "style", false) => rest = skip_past_close(rest, &name),
            ("p" | "div" | "ul" | "ol" | "blockquote" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6", _) => line_break(&mut out),
            ("br", _) => out.push('\n'),
            ("li", false) => {
                line_break(&mut out);
                out.push_str("- ");
            }
            ("b" | "strong", _) => out.push_str("**"),
            ("i" | "em", _) => out.push('_'),
            ("code", _) if !in_pre => out.push('`'),
            ("pre", _) => {
                line_break(&mut out);
                out.push_str("```\n");
                in_pre = !closing;
            }
            ("a", false) => {
                link = attr(attrs, "href");
                if link.is_some() {
                    out.push('[');
                }
            }
            ("a", true) => {
                if let Some(href) = link.take() {
                    out.push_str("](");
                    out.push_str(&href);
                    out.push(')');
                }
            }
            ("span", false) if attr(attrs, "itemtype").is_some_and(|t| t.ends_with("/Mention")) => out.push('@'),
            ("img", false) => out.push_str(&attr(attrs, "alt").unwrap_or_default()),
            _ => {}
        }
    }
    push_text(&mut out, rest);
    out.trim_matches('\n').to_string()
}

fn line_break(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// The text after `</name ...>`, or nothing when the element never closes.
fn skip_past_close<'a>(rest: &'a str, name: &str) -> &'a str {
    // ASCII lower-casing keeps byte offsets, so the index is valid in `rest`.
    match rest.to_ascii_lowercase().find(&format!("</{name}")) {
        Some(i) => rest.get(i..).and_then(|r| r.split_once('>')).map_or("", |(_, after)| after),
        None => "",
    }
}

/// The value of attribute `name` (any letter case), entities decoded.
fn attr(attrs: &str, name: &str) -> Option<String> {
    let mut rest = attrs;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
        if rest.is_empty() {
            return None;
        }
        let (key, tail) = rest.split_at(rest.find(|c: char| c == '=' || c == '/' || c.is_whitespace()).unwrap_or(rest.len()));
        let tail = tail.trim_start();
        let (value, after) = match tail.strip_prefix('=').map(str::trim_start) {
            None => ("", tail),
            Some(v) => match v.strip_prefix(['"', '\'']) {
                Some(quoted) => {
                    let quote = if v.starts_with('"') { '"' } else { '\'' };
                    quoted.split_once(quote).unwrap_or((quoted, ""))
                }
                None => v.split_at(v.find(char::is_whitespace).unwrap_or(v.len())),
            },
        };
        if key.eq_ignore_ascii_case(name) {
            let mut decoded = String::new();
            push_text(&mut decoded, value);
            return Some(decoded);
        }
        rest = after;
    }
}

/// Appends text with its entities decoded; unknown or malformed entities stay as they are.
fn push_text(out: &mut String, text: &str) {
    let mut rest = text;
    while let Some(i) = rest.find('&') {
        let (before, tail) = rest.split_at(i);
        out.push_str(before);
        let after_amp = tail.get(1..).unwrap_or("");
        let decoded = after_amp.char_indices().take(MAX_ENTITY).find(|&(_, c)| c == ';').and_then(|(end, _)| {
            let c = entity(after_amp.get(..end)?)?;
            Some((c, after_amp.get(end + 1..)?))
        });
        match decoded {
            Some((c, after)) => {
                out.push(c);
                rest = after;
            }
            None => {
                out.push('&');
                rest = after_amp;
            }
        }
    }
    out.push_str(rest);
}

fn entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        // A plain space; agents don't need the no-break kind.
        "nbsp" => ' ',
        _ => {
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            return char::from_u32(code);
        }
    })
}

/// Markdown to Teams HTML. Everything is escaped first, so text that looks like HTML arrives as
/// text; then `**bold**`, `_italic_`, `` `code` ``, fenced code, `[text](http(s) url)` and line
/// breaks become HTML again.
pub fn markdown_to_html(md: &str) -> String {
    let mut out = String::new();
    let mut in_pre = false;
    let mut line_open = false;
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            out.push_str(if in_pre { "</pre>" } else { "<pre>" });
            in_pre = !in_pre;
            line_open = false;
        } else if in_pre {
            if !out.ends_with("<pre>") {
                out.push('\n');
            }
            out.push_str(&escape_html(line));
        } else {
            if line_open {
                out.push_str("<br>");
            }
            out.push_str(&inline(&escape_html(line), 0));
            line_open = true;
        }
    }
    if in_pre {
        out.push_str("</pre>");
    }
    out
}

fn inline(text: &str, depth: u8) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if let Some((html, after)) = span(rest, depth, out.chars().next_back()) {
            out.push_str(&html);
            rest = after;
        } else {
            out.push(c);
            rest = rest.get(c.len_utf8()..).unwrap_or("");
        }
    }
    out
}

/// The span starting at `rest`, as HTML, and the text after it.
fn span(rest: &str, depth: u8, prev: Option<char>) -> Option<(String, &str)> {
    if depth >= MAX_NESTING {
        return None;
    }
    if let Some(after) = rest.strip_prefix('`') {
        let (code, after) = after.split_once('`')?;
        return (!code.is_empty()).then(|| (format!("<code>{code}</code>"), after));
    }
    if let Some(after) = rest.strip_prefix("**") {
        let (inner, after) = after.split_once("**")?;
        return (!inner.is_empty()).then(|| (format!("<b>{}</b>", inline(inner, depth + 1)), after));
    }
    if let Some(after) = rest.strip_prefix('_') {
        // Only at word boundaries, so snake_case stays text.
        let (inner, after) = after.split_once('_')?;
        let boundary = !prev.is_some_and(char::is_alphanumeric) && !after.starts_with(char::is_alphanumeric);
        return (boundary && !inner.is_empty()).then(|| (format!("<i>{}</i>", inline(inner, depth + 1)), after));
    }
    let (label, after) = rest.strip_prefix('[')?.split_once("](")?;
    let (url, after) = after.split_once(')')?;
    let web = (url.starts_with("https://") || url.starts_with("http://")) && !url.contains(char::is_whitespace);
    web.then(|| (format!("<a href=\"{url}\">{}</a>", inline(label, depth + 1)), after))
}

pub(crate) fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_and_breaks() {
        assert_eq!(html_to_markdown("<p>one</p><p>two<br>three</p>"), "one\ntwo\nthree");
        assert_eq!(html_to_markdown("<div>a</div><div>b<br/></div>"), "a\nb");
    }

    #[test]
    fn bold_italic_links() {
        let html = "<p><b>b</b> <strong>s</strong> <i>i</i> <em>e</em> <a href=\"https://example.com/?a=1&amp;b=2\" title=x>link</a></p>";
        assert_eq!(html_to_markdown(html), "**b** **s** _i_ _e_ [link](https://example.com/?a=1&b=2)");
    }

    #[test]
    fn code_and_pre() {
        let html = "<pre>let x = 1;\nlet y;</pre><p>use <code>x</code></p>";
        assert_eq!(html_to_markdown(html), "```\nlet x = 1;\nlet y;\n```\nuse `x`");
    }

    #[test]
    fn mention_becomes_at_name() {
        let html = "<p>Hi <span itemtype=\"http://schema.skype.com/Mention\" itemscope itemid=\"0\">Synthetic Person</span>!</p>";
        assert_eq!(html_to_markdown(html), "Hi @Synthetic Person!");
    }

    #[test]
    fn emoji_keeps_its_alt_text() {
        let html = "<p>ok <img itemtype=\"http://schema.skype.com/Emoji\" alt=\"\u{1f642}\" src=\"x\"></p>";
        assert_eq!(html_to_markdown(html), "ok \u{1f642}");
    }

    #[test]
    fn entities_are_decoded() {
        let html = "&lt;tag&gt; &amp; &quot;q&quot; &#39;s&#39; &#x41;&#66;&nbsp;x &bogus; & &#xFFFFFFFF;";
        assert_eq!(html_to_markdown(html), "<tag> & \"q\" 's' AB x &bogus; & &#xFFFFFFFF;");
    }

    #[test]
    fn malformed_html_degrades_to_text() {
        let md = html_to_markdown("<p>a <b>b <i>c</p><script>x()</script>&amp");
        for part in ["a", "b", "c"] {
            assert!(md.contains(part), "{md}");
        }
        assert!(!md.contains('<') && !md.contains("x()"), "{md}");
        assert!(!html_to_markdown("<style>p { color: red }</style>ok").contains("color"));
        assert_eq!(html_to_markdown("a < b"), "a < b");
        for odd in ["<", "<a", "<a href=\"x", "<!-- x", "<!-- a > b -->c", "<script>", "</", "<p\u{e9}>\u{e9}", "&", "&#;", "<a href=>x</a>"] {
            html_to_markdown(odd);
        }
    }

    #[test]
    fn send_escapes_html() {
        assert!(markdown_to_html("1 < 2 & <b>").contains("1 &lt; 2 &amp; &lt;b&gt;"));
        assert_eq!(markdown_to_html("[x](javascript:alert(1))"), "[x](javascript:alert(1))");
        assert_eq!(markdown_to_html("[x](https://a.example/\"onmouseover=y)"), "<a href=\"https://a.example/&quot;onmouseover=y\">x</a>");
        assert_eq!(markdown_to_html("my_var_name and 2*3*4"), "my_var_name and 2*3*4");
    }

    #[test]
    fn round_trip_of_simple_markdown() {
        let md = "**bold** and _it_ with `code` and [link](https://example.com/a?b=1&c=2)\nsecond line\n```\nfn x() {}\n```\nafter";
        let html = markdown_to_html(md);
        assert_eq!(
            html,
            "<b>bold</b> and <i>it</i> with <code>code</code> and <a href=\"https://example.com/a?b=1&amp;c=2\">link</a><br>second line\
             <pre>fn x() {}</pre>after"
        );
        assert_eq!(html_to_markdown(&html), md);
    }
}
