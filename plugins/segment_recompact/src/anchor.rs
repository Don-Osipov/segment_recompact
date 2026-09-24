//! Mechanical fidelity for summaries: what a summary must not lose is decided by code, not by the
//! summarizer.
//!
//! Two losses dominated the live record. Haiku summaries dropped exactly the specifics a later
//! turn needed (SQLSTATE strings, env var names, revision names — all recoverable, none visible),
//! and artifact tracking (which files changed) is the weakest dimension of every compaction
//! method evaluated so far. An offline compactor knows the session's future, so it can tell which
//! identifiers a unit introduced that later turns still use, and carry those — plus the unit's
//! files and verbatim errors — beneath the summary. The additions are deterministic, derived from
//! raw records on every pass, and never fed back to a model.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

use crate::{
    content, human_queued_text, is_genuine_user, is_human_queued, rec_type, truncate_head_tail,
};

/// Where a mention came from. Tool output is the noisiest source (an `ls` lists every path), so a
/// later mention there counts for less than one the agent or the human wrote.
#[derive(Clone, Copy)]
enum Voice {
    Human = 3,
    Agent = 2,
    Tool = 1,
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~' | '#' | ':' | '@')
}

fn trim_punct(s: &str) -> &str {
    s.trim_matches(|c: char| {
        matches!(
            c,
            '.' | ',' | ':' | ';' | ')' | '(' | '"' | '\'' | ']' | '['
        )
    })
}

fn looks_like_path(t: &str) -> bool {
    if t.len() < 4 || t.len() > 160 || t.contains("://") || !t.contains('/') {
        return false;
    }
    // Path characters only: a sed script like `/^\$fn\$;$/p` is not a file.
    if !t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '~' | '@' | '+'))
    {
        return false;
    }
    let segments = t.split('/').filter(|x| !x.is_empty()).count();
    let last = t.rsplit('/').next().unwrap_or(t);
    let ext = last
        .rsplit_once('.')
        .map(|(stem, e)| {
            !stem.is_empty()
                && (1..=6).contains(&e.len())
                && e.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .unwrap_or(false);
    // A file with an extension, or a directory path of two or more segments.
    (ext && segments >= 2)
        || (segments >= 2 && (t.starts_with('/') || t.starts_with("~/") || t.starts_with("./")))
}

/// A backticked span worth tracking: code-shaped, not a plain word the writer put in backticks
/// for emphasis (`views`, `main`, `ALTER`).
fn code_shaped(span: &str) -> bool {
    span.chars()
        .any(|c| matches!(c, '_' | '.' | '/' | ':' | '-' | '#' | '(' | ' ' | '=' | '@'))
        || span.chars().any(|c| c.is_ascii_digit())
        || span.chars().skip(1).any(|c| c.is_ascii_uppercase())
            && span.chars().any(|c| c.is_ascii_lowercase())
}

fn looks_like_upper_snake(t: &str) -> bool {
    t.len() >= 6
        && t.contains('_')
        && t.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && t.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && !t.ends_with('_')
}

fn looks_like_sha(t: &str) -> bool {
    (7..=40).contains(&t.len())
        && t.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        && t.chars().any(|c| c.is_ascii_digit())
        && t.chars().any(|c| c.is_ascii_alphabetic())
}

fn looks_like_ref(t: &str) -> bool {
    t.len() >= 4 && t.len() <= 8 && t.starts_with('#') && t[1..].chars().all(|c| c.is_ascii_digit())
}

/// Identifiers in free text: backticked spans, file paths, PR/issue refs (#1234), UPPER_SNAKE
/// names, git SHAs, and URLs. Hand-rolled (the crate stays at two dependencies); deliberately
/// conservative — a false identifier only costs a few characters under one summary, but a noisy
/// extractor would bury the real ones.
pub fn identifiers(text: &str, out: &mut Vec<String>) {
    // Backticked spans: what the writer marked as code.
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        match after.find('`') {
            Some(end) => {
                let span = after[..end].trim();
                if (3..=80).contains(&span.len())
                    && !span.contains('\n')
                    && !span.starts_with("``")
                    && code_shaped(span)
                {
                    out.push(span.to_string());
                }
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    // Whitespace-delimited tokens.
    for raw in text.split(|c: char| {
        c.is_whitespace() || c == '`' || c == '"' || c == '\'' || c == ',' || c == '(' || c == ')'
    }) {
        if raw.is_empty() {
            continue;
        }
        let t = trim_punct(raw);
        if t.len() < 4 {
            continue;
        }
        if (t.starts_with("https://") || t.starts_with("http://")) && t.len() <= 160 {
            out.push(t.to_string());
            continue;
        }
        if looks_like_ref(t) || looks_like_path(t) || looks_like_upper_snake(t) || looks_like_sha(t)
        {
            out.push(t.to_string());
            continue;
        }
        // PR refs glued to words ("PR#5074", "(#5074)") and SHAs inside brackets ("[main abc1234]").
        for piece in t
            .split(|c: char| !is_ident_char(c) || c == '#')
            .filter(|p| !p.is_empty())
        {
            if looks_like_sha(piece) || looks_like_upper_snake(piece) {
                out.push(piece.to_string());
            }
        }
        if let Some(i) = t.find('#') {
            let tail: String = t[i..]
                .chars()
                .take_while(|c| *c == '#' || c.is_ascii_digit())
                .collect();
            if looks_like_ref(&tail) && tail != t {
                out.push(tail);
            }
        }
    }
}

/// The voiced text of one record: what the model saw, labeled by who said it.
fn voiced_text(r: &Value) -> Vec<(Voice, String)> {
    let mut v = Vec::new();
    if is_human_queued(r) {
        v.push((Voice::Human, human_queued_text(r)));
        return v;
    }
    let human = is_genuine_user(r);
    match content(r) {
        Some(Value::String(s)) => {
            v.push((if human { Voice::Human } else { Voice::Tool }, s.clone()))
        }
        Some(Value::Array(blocks)) => {
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        let voice = match rec_type(r) {
                            "assistant" => Voice::Agent,
                            _ if human => Voice::Human,
                            _ => Voice::Tool,
                        };
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            v.push((voice, t.to_string()));
                        }
                    }
                    Some("tool_use") => {
                        if let Some(i) = b.get("input") {
                            v.push((Voice::Agent, serde_json::to_string(i).unwrap_or_default()));
                        }
                    }
                    Some("tool_result") => match b.get("content") {
                        Some(Value::String(s)) => v.push((Voice::Tool, s.clone())),
                        Some(Value::Array(inner)) => {
                            for ib in inner {
                                if let Some(t) = ib.get("text").and_then(|t| t.as_str()) {
                                    v.push((Voice::Tool, t.to_string()));
                                }
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        _ => {}
    }
    v
}

/// Tool-use inputs arrive JSON-encoded; unescape the common sequences so paths and names inside
/// commands tokenize the same way they do in prose.
fn unescape_json_text(s: &str) -> String {
    s.replace("\\n", " ")
        .replace("\\t", " ")
        .replace("\\\"", "\"")
        .replace("\\\\", "\\")
}

/// Split text into candidate tokens for vocabulary lookup.
fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '`' | '"' | '\'' | ',' | '(' | ')' | '[' | ']' | '{' | '}' | '=' | ';' | '<' | '>'
            )
    })
    .map(trim_punct)
    .filter(|t| t.len() >= 4)
}

/// Every identifier mention across a record set: identifier -> (record index, voice weight).
///
/// Two passes. The first collects the vocabulary — anything that looks like an identifier
/// anywhere, including backticked names. The second counts mentions of vocabulary words in plain
/// text too: a name the agent introduced as `crosspost-worker-00011-ch4` comes back later from a
/// human who does not type backticks.
pub struct Mentions {
    pub by_ident: HashMap<String, Vec<(usize, u8)>>,
    pub vocab: HashSet<String>,
}

impl Mentions {
    pub fn build(records: &[Value]) -> Mentions {
        let texts: Vec<Vec<(Voice, String)>> = records
            .iter()
            .map(|r| {
                voiced_text(r)
                    .into_iter()
                    .map(|(v, t)| (v, unescape_json_text(&t)))
                    .collect()
            })
            .collect();
        let mut vocab: HashSet<String> = HashSet::new();
        let mut ids = Vec::new();
        for rec in &texts {
            for (_, text) in rec {
                ids.clear();
                identifiers(text, &mut ids);
                vocab.extend(ids.drain(..));
            }
        }
        let mut by_ident: HashMap<String, Vec<(usize, u8)>> = HashMap::new();
        for (i, rec) in texts.iter().enumerate() {
            for (voice, text) in rec {
                let mut seen: HashSet<String> = HashSet::new();
                ids.clear();
                identifiers(text, &mut ids);
                seen.extend(ids.drain(..));
                for t in tokens(text) {
                    if vocab.contains(t) {
                        seen.insert(t.to_string());
                    }
                }
                for id in seen {
                    by_ident.entry(id).or_default().push((i, *voice as u8));
                }
            }
        }
        Mentions { by_ident, vocab }
    }

    /// Weight of mentions strictly after record index `end`.
    fn later_weight(&self, id: &str, end: usize) -> u32 {
        self.by_ident.get(id).map_or(0, |v| {
            v.iter()
                .filter(|(i, _)| *i > end)
                .map(|(_, w)| *w as u32)
                .sum()
        })
    }
}

/// Identifiers a unit introduced or used that later turns still reference, and that the summary
/// text does not already contain — the ones a resumed session would otherwise have to rehydrate
/// or re-derive. Ordered by how heavily the future leans on them.
pub fn hindsight_anchors(
    records: &[Value],
    part: &[usize],
    mentions: &Mentions,
    summary: &str,
    max_items: usize,
    max_chars: usize,
) -> Vec<String> {
    let Some(&end) = part.iter().max() else {
        return Vec::new();
    };
    let mut local: Vec<String> = Vec::new();
    for &i in part {
        for (_, text) in voiced_text(&records[i]) {
            identifiers(&unescape_json_text(&text), &mut local);
        }
    }
    let mut scored: BTreeMap<String, u32> = BTreeMap::new();
    for id in local {
        if summary.contains(id.as_str()) || scored.contains_key(&id) {
            continue;
        }
        let w = mentions.later_weight(&id, end);
        // One agent/human mention later, or two in tool output: the future depends on it.
        if w >= 2 {
            scored.insert(id, w);
        }
    }
    let mut ranked: Vec<(String, u32)> = scored.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.len().cmp(&b.0.len())));
    let mut out = Vec::new();
    let mut used = 0usize;
    for (id, _) in ranked {
        // A path whose tail is already carried is noise.
        if out
            .iter()
            .any(|o: &String| o.contains(id.as_str()) || id.contains(o.as_str()))
        {
            continue;
        }
        if out.len() >= max_items || used + id.len() + 4 > max_chars {
            break;
        }
        used += id.len() + 4;
        out.push(id);
    }
    out
}

/// Distinct error results in a unit, verbatim (head+tail within `each` chars), with the tool that
/// produced them. The error floor exists because error text is the one thing a resumed session
/// cannot cheaply re-derive; carrying it mechanically lets the unit be summarized without losing it.
pub fn error_evidence(
    records: &[Value],
    part: &[usize],
    max_errors: usize,
    each: usize,
) -> Vec<(String, String)> {
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for &i in part {
        if let Some(blocks) = content(&records[i]).and_then(|c| c.as_array()) {
            for b in blocks {
                if b.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                    if let (Some(id), Some(name)) = (
                        b.get("id").and_then(|v| v.as_str()),
                        b.get("name").and_then(|v| v.as_str()),
                    ) {
                        tool_names.insert(id.to_string(), name.to_string());
                    }
                }
            }
        }
    }
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for &i in part {
        let Some(blocks) = content(&records[i]).and_then(|c| c.as_array()) else {
            continue;
        };
        for b in blocks {
            if b.get("type").and_then(|v| v.as_str()) != Some("tool_result")
                || !b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false)
            {
                continue;
            }
            let text = match b.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            let text = text.trim().to_string();
            if text.is_empty() || !seen.insert(text.clone()) {
                continue;
            }
            let tool = b
                .get("tool_use_id")
                .and_then(|v| v.as_str())
                .and_then(|id| tool_names.get(id).cloned())
                .unwrap_or_else(|| "tool".into());
            out.push((
                tool,
                truncate_head_tail(&text, each, 0.6).replace('\n', " ⏎ "),
            ));
            if out.len() >= max_errors {
                return out;
            }
        }
    }
    out
}

/// Retention of cross-referenced identifiers: those mentioned in at least two different user
/// turns' spans, with total weight >= 3, in the source. `visible` is the identifier set of the assembled output's
/// model-visible text. Returns (kept, total, a few missing examples).
pub fn retention(
    source: &Mentions,
    visible: &HashSet<String>,
    turn_of: &dyn Fn(usize) -> usize,
) -> (usize, usize, Vec<String>) {
    let mut total = 0usize;
    let mut kept = 0usize;
    let mut missing: Vec<(u32, String)> = Vec::new();
    for (id, occ) in &source.by_ident {
        // Cross-turn references are what continuity needs; one used only within its own turn
        // is that turn's business.
        let distinct: HashSet<usize> = occ.iter().map(|(i, _)| turn_of(*i)).collect();
        let weight: u32 = occ.iter().map(|(_, w)| *w as u32).sum();
        if distinct.len() < 2 || weight < 3 {
            continue;
        }
        total += 1;
        if visible.contains(id) {
            kept += 1;
        } else {
            missing.push((weight, id.clone()));
        }
    }
    missing.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    (
        kept,
        total,
        missing.into_iter().take(8).map(|(_, id)| id).collect(),
    )
}

/// Identifier set of a record set's model-visible text, recognizing `vocab` words in plain
/// text the same way `Mentions` does.
pub fn visible_identifiers(records: &[Value], vocab: &HashSet<String>) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    let mut ids = Vec::new();
    for r in records {
        for (_, text) in voiced_text(r) {
            let text = unescape_json_text(&text);
            ids.clear();
            identifiers(&text, &mut ids);
            out.extend(ids.drain(..));
            for t in tokens(&text) {
                if vocab.contains(t) {
                    out.insert(t.to_string());
                }
            }
        }
    }
    out
}
