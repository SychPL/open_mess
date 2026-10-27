//! `writing_context`: which of the user's own messages may show an agent how the user writes
//! (spec 8.5). The rules are teams-wrapper's (`src/writing-style/filter.ts` and `service.ts`),
//! ported without regular expressions: no secrets, code, links, JSON or operational data, at most
//! 500 characters, and a mix of short, medium and long examples, the asked purpose first.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Casual,
    TechnicalStatus,
    Sensitive,
}

impl Purpose {
    pub fn parse(name: &str) -> Option<Purpose> {
        match name {
            "casual" => Some(Purpose::Casual),
            "technical_status" => Some(Purpose::TechnicalStatus),
            "sensitive" => Some(Purpose::Sensitive),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Candidate {
    pub text: String,
    pub sent_ms: i64,
    pub purposes: Vec<Purpose>,
}

/// A message's HTML as an example, or `None` when the wrapper's filters leave it out.
pub fn candidate(html: &str, sent_ms: i64) -> Option<Candidate> {
    if has_hyperlink(html) {
        return None;
    }
    let text = normalize(html);
    let excluded = text.is_empty()
        || text.chars().count() > 500
        || is_link(&text)
        || is_json(&text)
        || is_secret(&text)
        || is_code(&text)
        || is_operational(&text);
    if excluded {
        return None;
    }
    let lower = text.to_lowercase();
    let mut purposes = Vec::new();
    if TECHNICAL.iter().any(|w| has_word(&lower, w)) && STATUS.iter().any(|s| has_stem(&lower, s)) {
        purposes.push(Purpose::TechnicalStatus);
    }
    if SENSITIVE.iter().any(|s| has_stem(&lower, s)) {
        purposes.push(Purpose::Sensitive);
    }
    if purposes.is_empty() {
        purposes.push(Purpose::Casual);
    }
    Some(Candidate { text, sent_ms, purposes })
}

/// Up to `limit` examples: one per text (the newest), those with `purpose` first, each group a
/// mix of lengths, newest first.
pub fn select(candidates: Vec<Candidate>, purpose: Purpose, limit: usize) -> Vec<Candidate> {
    let mut unique: Vec<Candidate> = Vec::new();
    for c in candidates {
        match unique.iter_mut().find(|u| u.text == c.text) {
            Some(u) if u.sent_ms < c.sent_ms => *u = c,
            Some(_) => {}
            None => unique.push(c),
        }
    }
    unique.sort_by_key(|c| std::cmp::Reverse(c.sent_ms));
    let (preferred, fallback): (Vec<_>, Vec<_>) = unique.into_iter().partition(|c| c.purposes.contains(&purpose));
    let mut selected = diverse(preferred, limit);
    let mut rest = diverse(fallback, limit.saturating_sub(selected.len()));
    selected.sort_by_key(|c| std::cmp::Reverse(c.sent_ms));
    rest.sort_by_key(|c| std::cmp::Reverse(c.sent_ms));
    selected.append(&mut rest);
    selected
}

/// Round robin over short (up to 30 characters), medium (up to 120) and long examples.
fn diverse(candidates: Vec<Candidate>, limit: usize) -> Vec<Candidate> {
    let mut buckets: [Vec<Candidate>; 3] = Default::default();
    for c in candidates.into_iter().rev() {
        let len = c.text.chars().count();
        let bucket = if len <= 30 {
            0
        } else if len <= 120 {
            1
        } else {
            2
        };
        if let Some(b) = buckets.get_mut(bucket) {
            b.push(c);
        }
    }
    // Reversed above, so `pop` takes each bucket's first (newest) one.
    let mut selected = Vec::new();
    while selected.len() < limit && buckets.iter().any(|b| !b.is_empty()) {
        for bucket in &mut buckets {
            if selected.len() == limit {
                break;
            }
            if let Some(c) = bucket.pop() {
                selected.push(c);
            }
        }
    }
    selected
}

/// Tags become spaces, the common entities characters, runs of whitespace one space.
fn normalize(html: &str) -> String {
    let mut stripped = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        stripped.push_str(rest.get(..i).unwrap_or_default());
        let after = rest.get(i + 1..).unwrap_or_default();
        match after.find('>') {
            // `<[^>]+>`: at least one character inside.
            Some(end) if end > 0 => {
                stripped.push(' ');
                rest = after.get(end + 1..).unwrap_or_default();
            }
            _ => {
                stripped.push('<');
                rest = after;
            }
        }
    }
    stripped.push_str(rest);
    let mut text = stripped;
    for (entity, c) in [("&nbsp;", " "), ("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", "\""), ("&#39;", "'")] {
        text = replace_ignore_case(&text, entity, c);
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn replace_ignore_case(text: &str, from: &str, to: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (i, _) in lower.match_indices(from) {
        out.push_str(text.get(last..i).unwrap_or_default());
        out.push_str(to);
        last = i + from.len();
    }
    out.push_str(text.get(last..).unwrap_or_default());
    out
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `<a`, `<area`, `<link` or an `href=` attribute.
fn has_hyperlink(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let tag = lower.match_indices('<').any(|(i, _)| {
        let rest = lower.get(i + 1..).unwrap_or_default().trim_start();
        ["a", "area", "link"].iter().any(|t| rest.strip_prefix(t).is_some_and(|after| !after.starts_with(is_word)))
    });
    let href = lower.match_indices("href").any(|(i, _)| {
        let before = lower.get(..i).and_then(|b| b.chars().next_back());
        let boundary = before.is_none_or(|c| c.is_whitespace() || "\"'=<".contains(c));
        boundary && lower.get(i + 4..).unwrap_or_default().trim_start().starts_with('=')
    });
    tag || href
}

/// A link and nothing else: `www.`, `mailto:`, `scheme://`, or a bare host name with a path.
fn is_link(text: &str) -> bool {
    if text.contains(char::is_whitespace) {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    let prefixed = |p: &str| lower.strip_prefix(p).is_some_and(|rest| !rest.is_empty());
    if prefixed("www.") || prefixed("mailto:") {
        return true;
    }
    if let Some((scheme, rest)) = lower.split_once("://") {
        let mut chars = scheme.chars();
        let valid = chars.next().is_some_and(|c| c.is_ascii_lowercase()) && chars.all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c));
        if valid && !rest.is_empty() {
            return true;
        }
    }
    is_bare_host(&lower)
}

/// `label.label.tld[:port][/path][?query][#fragment]`.
fn is_bare_host(text: &str) -> bool {
    let end = text.find(['/', '?', '#']).unwrap_or(text.len());
    let (authority, rest) = text.split_at(end);
    // `/path` may not hold `?` or `#` before its own query and fragment: anything else is fine.
    let _ = rest;
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if port.is_some_and(|p| p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let Some((tld, names)) = labels.split_last() else { return false };
    let label = |l: &&str| {
        (1..=63).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !l.starts_with('-') && !l.ends_with('-')
    };
    !names.is_empty() && names.iter().all(label) && (2..=63).contains(&tld.len()) && tld.bytes().all(|b| b.is_ascii_alphabetic())
}

/// JSON, or `label: {json}`.
fn is_json(text: &str) -> bool {
    let parses = |t: &str| serde_json::from_str::<serde_json::Value>(t).is_ok();
    if (text.starts_with('{') || text.starts_with('[')) && parses(text) {
        return true;
    }
    let Some((label, value)) = text.split_once(':') else { return false };
    let label = label.trim_end();
    let mut chars = label.chars();
    let label_ok = chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && label.chars().count() <= 41
        && chars.all(|c| c.is_alphanumeric() || " _-".contains(c));
    let value = value.trim_start();
    label_ok && value.starts_with(['[', '{']) && value.ends_with([']', '}']) && parses(value)
}

fn is_secret(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if ["defaultendpointsprotocol=", "accountkey=", "sharedaccesssignature="].iter().any(|k| lower.contains(k)) {
        return true;
    }
    let authorization = lower.match_indices("authorization").any(|(i, _)| {
        let rest = lower.get(i + "authorization".len()..).unwrap_or_default().trim_start();
        let Some(rest) = rest.strip_prefix(':') else { return false };
        let rest = rest.trim_start();
        ["bearer", "basic"]
            .iter()
            .any(|scheme| rest.strip_prefix(scheme).is_some_and(|after| after.starts_with(char::is_whitespace) && !after.trim_start().is_empty()))
    });
    let private_key = text.match_indices("-----BEGIN ").any(|(i, _)| {
        let rest = text.get(i + "-----BEGIN ".len()..).unwrap_or_default();
        rest.find("PRIVATE KEY-----").is_some_and(|end| rest.get(..end).unwrap_or_default().chars().all(|c| c.is_ascii_uppercase() || c == ' '))
    });
    let assignment = ["token", "secret", "password", "api_key", "api-key", "apikey"]
        .iter()
        .any(|k| lower.match_indices(k).any(|(i, _)| lower.get(i + k.len()..).unwrap_or_default().trim_start().starts_with([':', '='])));
    let known_key = words(text).any(|w| {
        let token =
            |prefix: &str, len: usize, ok: fn(u8) -> bool| w.strip_prefix(prefix).is_some_and(|rest| rest.len() == len && rest.bytes().all(ok));
        token("ghp_", 36, |b| b.is_ascii_alphanumeric()) || token("AKIA", 16, |b| b.is_ascii_uppercase() || b.is_ascii_digit())
    });
    let long_run = text.split(|c: char| !(c.is_ascii_alphanumeric() || "_+/=-".contains(c))).any(|run| run.len() >= 48);
    authorization || private_key || assignment || known_key || long_run
}

/// Maximal runs of word characters (`[A-Za-z0-9_]`).
fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !is_word(c)).filter(|w| !w.is_empty())
}

/// `name` or `a.b.c`, JavaScript identifiers; the rest of the text after it.
fn dotted_ident(text: &str) -> Option<&str> {
    let ident_start = |c: char| c.is_ascii_alphabetic() || c == '_' || c == '$';
    let ident = |c: char| is_word(c) || c == '$';
    let mut rest = text;
    loop {
        if !rest.starts_with(ident_start) {
            return None;
        }
        rest = rest.trim_start_matches(ident);
        match rest.strip_prefix('.') {
            Some(after) if after.starts_with(ident_start) => rest = after,
            _ => return Some(rest),
        }
    }
}

/// `ident(...)`, `ident(...);` or `await ident(...)`, and nothing else.
fn is_call(text: &str) -> bool {
    let text = text.strip_prefix("await").filter(|r| r.starts_with(char::is_whitespace)).map_or(text, str::trim_start);
    let Some(rest) = dotted_ident(text) else { return false };
    let Some(args) = rest.trim_start().strip_prefix('(') else { return false };
    let Some(close) = args.find(')') else { return false };
    let after = args.get(close + 1..).unwrap_or_default().trim_start();
    after.is_empty() || after == ";"
}

/// Whether `token` starts with `word` (ASCII case ignored) followed by a non-word character or
/// nothing.
fn starts_word(token: &str, word: &str) -> bool {
    token.get(..word.len()).is_some_and(|head| head.eq_ignore_ascii_case(word)) && !token.get(word.len()..).unwrap_or_default().starts_with(is_word)
}

const COMMANDS: &[(&str, bool, &[&str])] = &[
    ("npm", false, &["run", "exec", "test", "build", "start", "install", "add", "remove", "update", "publish", "pack"]),
    ("pnpm", false, &["run", "exec", "test", "build", "start", "install", "add", "remove", "update", "publish", "pack"]),
    ("yarn", false, &["run", "exec", "test", "build", "start", "install", "add", "remove", "update", "publish", "pack"]),
    ("bun", false, &["run", "exec", "test", "build", "start", "install", "add", "remove", "update", "publish", "pack"]),
    (
        "git",
        true,
        &[
            "add",
            "bisect",
            "branch",
            "checkout",
            "cherry-pick",
            "clone",
            "commit",
            "config",
            "diff",
            "fetch",
            "init",
            "log",
            "merge",
            "pull",
            "push",
            "rebase",
            "remote",
            "reset",
            "restore",
            "revert",
            "show",
            "stash",
            "status",
            "switch",
            "tag",
            "worktree",
        ],
    ),
    (
        "docker",
        true,
        &[
            "build",
            "compose",
            "container",
            "exec",
            "image",
            "inspect",
            "kill",
            "logs",
            "network",
            "pull",
            "push",
            "restart",
            "rm",
            "run",
            "start",
            "stop",
            "system",
            "tag",
            "volume",
        ],
    ),
    (
        "kubectl",
        true,
        &[
            "apply",
            "config",
            "create",
            "delete",
            "describe",
            "exec",
            "explain",
            "expose",
            "get",
            "logs",
            "patch",
            "port-forward",
            "rollout",
            "run",
            "scale",
            "set",
            "top",
            "wait",
        ],
    ),
    (
        "helm",
        true,
        &[
            "create",
            "dependency",
            "env",
            "get",
            "history",
            "install",
            "lint",
            "list",
            "package",
            "plugin",
            "pull",
            "repo",
            "rollback",
            "search",
            "show",
            "status",
            "template",
            "test",
            "uninstall",
            "upgrade",
            "verify",
            "version",
        ],
    ),
    ("az", true, &["account", "acr", "ad", "aks", "appconfig", "deployment", "functionapp", "group", "login", "logout", "rest", "storage", "webapp"]),
    ("aws", true, &["cloudformation", "configure", "ec2", "ecs", "eks", "iam", "lambda", "logs", "s3", "s3api", "sts"]),
];

fn is_code(text: &str) -> bool {
    if text.contains("```") || text.contains("=>") {
        return true;
    }
    // A declaration keyword at a word start, then whitespace and an identifier.
    let declaration = ["const", "let", "var", "function", "class", "import", "export"].iter().any(|k| {
        text.match_indices(k).any(|(i, _)| {
            let start = text.get(..i).and_then(|b| b.chars().next_back()).is_none_or(|c| !is_word(c));
            let rest = text.get(i + k.len()..).unwrap_or_default();
            start && rest.starts_with(char::is_whitespace) && rest.trim_start().starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '$')
        })
    });
    let control = ["if", "for", "while", "switch", "catch"].iter().any(|k| {
        let Some(rest) = text.strip_prefix(k) else { return false };
        let Some(inside) = rest.trim_start().strip_prefix('(') else { return false };
        let Some(close) = inside.find(')') else { return false };
        let after = inside.get(close + 1..).unwrap_or_default().trim_start();
        let after = after.strip_prefix("await").filter(|r| r.starts_with(char::is_whitespace)).map_or(after, str::trim_start);
        after.starts_with('{') || dotted_ident(after).is_some_and(|r| r.trim_start().starts_with('('))
    });
    let assignment = dotted_ident(text)
        .and_then(|rest| rest.trim_start().strip_prefix('='))
        .is_some_and(|value| value.chars().next().is_some_and(|c| c != '=' && c != '>') && !value.trim_start().is_empty());
    declaration || control || assignment || is_sql(text) || is_command(text) || is_call(text)
}

fn is_sql(text: &str) -> bool {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let word = |i: usize, w: &str| tokens.get(i).is_some_and(|t| starts_word(t, w) && t.len() == w.len());
    let first = |w: &str| tokens.first().is_some_and(|t| starts_word(t, w));
    (first("select") && tokens.iter().skip(1).any(|t| t.trim_matches(|c: char| !is_word(c)).eq_ignore_ascii_case("from")))
        || (word(0, "insert") && tokens.get(1).is_some_and(|t| starts_word(t, "into")))
        || (word(0, "update") && tokens.len() > 2 && tokens.get(2).is_some_and(|t| starts_word(t, "set")))
        || (word(0, "delete") && tokens.get(1).is_some_and(|t| starts_word(t, "from")))
        || (word(0, "create") && tokens.get(1).is_some_and(|t| starts_word(t, "table") || starts_word(t, "index")))
        || (word(0, "alter") && tokens.get(1).is_some_and(|t| starts_word(t, "table")))
        || (word(0, "drop") && tokens.get(1).is_some_and(|t| starts_word(t, "table") || starts_word(t, "index")))
}

fn is_command(text: &str) -> bool {
    let mut tokens = text.split_whitespace().peekable();
    let sudo = tokens.next_if(|t| t.eq_ignore_ascii_case("sudo")).is_some();
    let (Some(tool), Some(sub)) = (tokens.next(), tokens.next()) else { return false };
    let tool = tool.to_ascii_lowercase();
    if tool == "curl" || tool == "wget" {
        let flag = sub.strip_prefix('-').map(|f| f.trim_start_matches('-')).is_some_and(|f| f.starts_with(|c: char| c.is_ascii_alphanumeric()));
        let lower = sub.to_ascii_lowercase();
        return flag || ((lower.starts_with("http://") || lower.starts_with("https://")) && !lower.ends_with("//"));
    }
    COMMANDS.iter().any(|(name, takes_sudo, subs)| *name == tool && (*takes_sudo || !sudo) && subs.iter().any(|s| starts_word(sub, s)))
}

fn is_operational(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let id = lower.match_indices("8:orgid:").any(|(i, _)| lower.get(..i).and_then(|b| b.chars().next_back()).is_none_or(|c| !is_word(c)));
    let markers = words(&lower).any(|w| w == "callstarted" || w == "clientmessageid");
    id || markers || text.contains('·') || is_stack_trace(text) || is_log_line(text) || is_compiler_message(&lower) || is_rollout(&lower)
}

/// `SomeError: message ... at place (file:1:2)`.
fn is_stack_trace(text: &str) -> bool {
    let Some((head, tail)) = text.split_once(':') else { return false };
    let lower_head = head.to_ascii_lowercase();
    let head_ok = head.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && head.chars().all(|c| is_word(c) || c == '.' || c == '$')
        && (lower_head.ends_with("error") || lower_head.ends_with("exception"));
    let Some(at) = tail.match_indices("at").find(|(i, _)| {
        let start = tail.get(..*i).and_then(|b| b.chars().next_back()).is_none_or(|c| !is_word(c));
        start && tail.get(i + 2..).unwrap_or_default().starts_with(char::is_whitespace)
    }) else {
        return false;
    };
    let after = tail.get(at.0 + 2..).unwrap_or_default();
    // `:<line>:<column>` somewhere after `at`.
    let position = after.match_indices(':').any(|(i, _)| {
        let rest = after.get(i + 1..).unwrap_or_default();
        let line = rest.trim_start_matches(|c: char| c.is_ascii_digit());
        line.len() < rest.len() && line.strip_prefix(':').is_some_and(|col| col.starts_with(|c: char| c.is_ascii_digit()))
    });
    head_ok && position
}

/// `[2026-01-01T10:00:00Z] INFO ...`.
fn is_log_line(text: &str) -> bool {
    let rest = text.strip_prefix('[').map_or(text, str::trim_start);
    let end = rest.find(|c: char| c.is_whitespace() || c == ']').unwrap_or(rest.len());
    let (stamp, rest) = rest.split_at(end);
    if !stamp.ends_with('Z') || crate::time::iso8601_ms(stamp).is_none() {
        return false;
    }
    let rest = rest.strip_prefix(']').unwrap_or(rest);
    if !rest.starts_with(char::is_whitespace) {
        return false;
    }
    let rest = rest.trim_start();
    ["trace", "debug", "info", "warn", "error", "fatal"].iter().any(|level| starts_word(rest, level))
}

/// `src/a.ts(3,4): error TS2322 ...` or `error CS1002`.
fn is_compiler_message(lower: &str) -> bool {
    let mut rest = lower;
    if let Some((first, after)) = lower.split_once(char::is_whitespace)
        && first.ends_with(':')
        && [".ts", ".tsx", ".js", ".jsx", ".cs", ".java", ".py", ".go", ".rs"]
            .iter()
            .any(|ext| first.contains(&format!("{ext}(")) || first.contains(&format!("{ext}:")))
    {
        rest = after.trim_start();
    }
    ["error", "warning"].iter().any(|kind| {
        let Some(after) = rest.strip_prefix(kind).filter(|a| a.starts_with(char::is_whitespace)) else { return false };
        let code = after.trim_start();
        let code = code.strip_prefix("ts").or_else(|| code.strip_prefix("cs")).unwrap_or(code);
        let digits = code.len() - code.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        digits > 0 && !code.get(digits..).unwrap_or_default().starts_with(is_word)
    })
}

/// `deployment.apps/web configured`.
fn is_rollout(lower: &str) -> bool {
    let mut tokens = lower.split_whitespace();
    let (Some(resource), Some(state)) = (tokens.next(), tokens.next()) else { return false };
    let Some(kind) = resource.strip_prefix("deployment") else { return false };
    let Some((group, name)) = kind.split_once('/') else { return false };
    let group_ok = group.is_empty()
        || group.strip_prefix('.').is_some_and(|g| !g.is_empty() && g.chars().all(|c| c.is_ascii_alphanumeric() || ".-".contains(c)));
    let state_ok = ["configured", "created", "unchanged", "deleted"].iter().any(|s| starts_word(state, s))
        || (state == "rolled" && tokens.next().is_some_and(|t| starts_word(t, "out")));
    group_ok && !name.is_empty() && state_ok
}

const TECHNICAL: &[&str] =
    &["deploy", "dev", "prod", "pr", "commit", "branch", "pipeline", "workflow", "apply", "agent", "endpoint", "fix", "hotfix"];
const STATUS: &[&str] = &["działa", "przeszedł", "przyczyn", "popraw", "zmienił", "podnies", "wdroż", "gotowe"];
const SENSITIVE: &[&str] = &["sorka", "niestety", "nie mam", "brak czasu", "przeciąż", "odpowiedzial", "martwi", "winny", "spotkani"];

/// `stem` at the start of a word of `lower`.
fn has_stem(lower: &str, stem: &str) -> bool {
    lower.match_indices(stem).any(|(i, _)| lower.get(..i).and_then(|b| b.chars().next_back()).is_none_or(|c| !c.is_alphanumeric()))
}

/// `word` as a whole word of `lower`.
fn has_word(lower: &str, word: &str) -> bool {
    lower.match_indices(word).any(|(i, _)| {
        let start = lower.get(..i).and_then(|b| b.chars().next_back()).is_none_or(|c| !c.is_alphanumeric());
        start && !lower.get(i + word.len()..).unwrap_or_default().starts_with(char::is_alphanumeric)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(text: &str) -> bool {
        candidate(text, 0).is_some()
    }

    #[test]
    fn plain_messages_are_kept_and_normalized() {
        let c = candidate("<p>no to&nbsp;lecimy &amp; <b>zobaczymy</b></p>", 7).unwrap();
        assert_eq!(c, Candidate { text: "no to lecimy & zobaczymy".to_string(), sent_ms: 7, purposes: vec![Purpose::Casual] });
        assert!(kept("<p>ok, będę za 5 min</p>"));
        assert!(kept("a < b, więc nie"));
    }

    #[test]
    fn links_are_left_out() {
        for text in [
            "<p>zobacz <a href=\"https://example.com\">tu</a></p>",
            "<p href=x>x</p>",
            "https://example.com/a?b=1",
            "www.example.com",
            "mailto:someone@example.com",
            "example.com/path",
            "intranet.example.co:8080",
        ] {
            assert!(!kept(text), "{text}");
        }
        assert!(kept("zobacz example.com jutro"));
        assert!(kept("<p>nie <abbr>ma</abbr> linku</p>"));
    }

    #[test]
    fn json_is_left_out() {
        for text in ["{\"a\": 1}", "[1, 2]", "payload: {\"a\": [1]}", "Wynik testu: [1,2,3]"] {
            assert!(!kept(text), "{text}");
        }
        assert!(kept("{nie json}"));
        assert!(kept("uwaga: [to nie json"));
    }

    #[test]
    fn secrets_are_left_out() {
        for text in [
            "AccountKey=abc",
            "Authorization: Bearer abc.def",
            "-----BEGIN RSA PRIVATE KEY----- x",
            "password: hunter2",
            "my api_key = 1",
            "Token=1",
            "klucz ghp_abcdefghijklmnopqrstuvwxyz0123456789 tutaj",
            "AKIAABCDEFGHIJKLMNOP",
            &"x".repeat(48),
        ] {
            assert!(!kept(text), "{text}");
        }
        assert!(kept("token jutro, dziś nie"));
        assert!(kept(&"x".repeat(47)));
    }

    #[test]
    fn code_is_left_out() {
        for text in [
            "```rust fn x() {}```",
            "const x = 1",
            "export function a() {}",
            "if (a) { b() }",
            "for (x of y) await run(x)",
            "config.value = 5;",
            "a => b",
            "SELECT id FROM users",
            "insert into t values (1)",
            "update users set a = 1",
            "DROP TABLE x",
            "npm install",
            "sudo git status",
            "kubectl get pods",
            "curl -X POST x",
            "wget https://example.com/file",
            "await client.send(msg);",
            "doIt()",
        ] {
            assert!(!kept(text), "{text}");
        }
        // Like the wrapper's case-sensitive pattern, "let me know" is taken for code; "Let" is not.
        assert!(!kept("let me know"));
        for text in ["Let me know", "if you want, sure", "select the best one", "x == y", "npm jest super", "git gud", "no (serio) nie"] {
            assert!(kept(text), "{text}");
        }
    }

    #[test]
    fn operational_data_is_left_out() {
        for text in [
            "8:orgid:00000000-0000-0000-0000-000000000002",
            "callStarted",
            "TypeError: x is undefined at run (app.js:1:2)",
            "[2026-01-01T10:00:00Z] ERROR boom",
            "2026-01-01T10:00:00.123Z INFO started",
            "src/a.ts(3,4): error TS2322: nope",
            "error CS1002: ; expected",
            "deployment.apps/web configured",
            "deployment/web rolled out",
            "a · b",
        ] {
            assert!(!kept(text), "{text}");
        }
        assert!(kept("error w raporcie, sprawdzę"));
        assert!(kept("deployment jutro"));
    }

    #[test]
    fn long_messages_are_left_out() {
        assert!(!kept(&"słowo ".repeat(100)));
        assert!(kept(&"ab ".repeat(166)));
    }

    #[test]
    fn purposes_follow_the_wrapper() {
        let purposes = |text: &str| candidate(text, 0).unwrap().purposes;
        assert_eq!(purposes("deploy na prod przeszedł"), [Purpose::TechnicalStatus]);
        assert_eq!(purposes("fix gotowe"), [Purpose::TechnicalStatus]);
        // A technical word without a status word, and the reverse, are casual.
        assert_eq!(purposes("devops to stan umysłu"), [Purpose::Casual]);
        assert_eq!(purposes("prod padł"), [Purpose::Casual]);
        assert_eq!(purposes("Niestety nie mam czasu"), [Purpose::Sensitive]);
        assert_eq!(purposes("sorka, pr gotowe"), [Purpose::TechnicalStatus, Purpose::Sensitive]);
    }

    #[test]
    fn selection_prefers_the_purpose_and_mixes_lengths() {
        let c = |text: &str, sent_ms: i64, purpose: Purpose| Candidate { text: text.to_string(), sent_ms, purposes: vec![purpose] };
        let long = "x ".repeat(70);
        let candidates = vec![
            c("short one", 1, Purpose::Casual),
            c("short two", 2, Purpose::Casual),
            c("short two", 9, Purpose::Casual),
            c(&long, 3, Purpose::Casual),
            c("a technical status message of some medium length here", 4, Purpose::TechnicalStatus),
        ];
        let texts = |picked: Vec<Candidate>| picked.into_iter().map(|p| (p.text, p.sent_ms)).collect::<Vec<_>>();
        let medium = "a technical status message of some medium length here";
        // The purpose first, one of each length per round (the newest of duplicates), newest
        // first; the rest only when the purpose runs out.
        let picked = texts(select(candidates, Purpose::Casual, 4));
        let expected = [("short two", 9), (long.as_str(), 3), ("short one", 1), (medium, 4)];
        assert_eq!(picked, expected.map(|(t, ms)| (t.to_string(), ms)));
        let mixed = vec![c("s1", 5, Purpose::Casual), c("s2", 4, Purpose::Casual), c(medium, 1, Purpose::Casual)];
        assert_eq!(texts(select(mixed, Purpose::Casual, 2)), [("s1".to_string(), 5), (medium.to_string(), 1)]);
        assert_eq!(select(Vec::new(), Purpose::Casual, 3), []);
    }
}
