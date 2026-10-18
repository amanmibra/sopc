//! Deterministic text analysis for importing and reviewing prompts: `overlap`, `compare` and
//! `lint`. Prompts are split into sentence-sized units and compared by their words.

use crate::model::{Agent, Block};
use crate::render::{fill_variables, find_variables, render_step, resolve_blocks, Build, StepKind};
use crate::text::quote;
use crate::workspace::Workspace;
use regex::Regex;
use serde_json::{json, Value as Json};
use similar::{capture_diff_slices, Algorithm, DiffOp, DiffTag};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

static LIST_MARKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:[-*+]|\d+[.)])\s+").unwrap());
static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*#{1,6}\s+").unwrap());
static LABEL_ONLY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z ]{0,30}:$").unwrap());
static SOPC_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:Goal|When this applies):\s+").unwrap());
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z0-9]+(?:'[a-z]+)?").unwrap());
static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[0-9]+(?:[.:][0-9]+)?").unwrap());
/// Lines sopc itself adds to SOPs, which originals never contain.
static GENERATED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:Use the `[^`]+` tool\.|This applies to the `[^`]+` tool\.|Before following this procedure, call the `get_sop` tool.*)$").unwrap()
});

const NEGATIONS: &[&str] =
    &["not", "never", "no", "don't", "dont", "doesn't", "cannot", "can't", "won't", "without", "nothing"];
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "has", "have", "if", "in", "is", "it", "its", "of",
    "on", "or", "so", "that", "the", "their", "them", "they", "this", "to", "was", "were", "when", "with", "you",
    "your",
];

/// The shortest unit worth comparing, in words.
const MIN_WORDS: usize = 3;

/// Splits a line after `.`, `!` or `?` followed by whitespace and a capital, digit, quote or `(`.
fn split_sentences(line: &str) -> Vec<&str> {
    let (mut out, mut start) = (vec![], 0);
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let mut i = 1;
    while i < chars.len() {
        if !chars[i].1.is_whitespace() || !matches!(chars[i - 1].1, '.' | '!' | '?') {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < chars.len() && chars[j].1.is_whitespace() {
            j += 1;
        }
        if j < chars.len()
            && (chars[j].1.is_ascii_uppercase() || chars[j].1.is_ascii_digit() || "\"'(".contains(chars[j].1))
        {
            out.push(&line[start..chars[i].0]);
            start = chars[j].0;
        }
        i = j;
    }
    out.push(&line[start..]);
    out
}

/// Prompt text as sentences, list items and lines; headings and bare labels are dropped.
pub fn units(text: &str) -> Vec<String> {
    unit_ranges(text).into_iter().map(|r| text[r].to_string()).collect()
}

/// Where each of `units` is in the text, as byte ranges: every unit is a slice of the text.
fn unit_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = vec![];
    let mut line_start = 0;
    for raw in text.split_inclusive('\n') {
        let offset = line_start;
        line_start += raw.len();
        let raw = raw.strip_suffix('\n').unwrap_or(raw);
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let line = raw.trim();
        if line.is_empty() || HEADING.is_match(line) || LABEL_ONLY.is_match(line) {
            continue;
        }
        let mut at = offset + (raw.len() - raw.trim_start().len());
        let mut line = line;
        for prefix in [&*LIST_MARKER, &*SOPC_PREFIX] {
            let skip = prefix.find(line).map_or(0, |m| m.end());
            (line, at) = (&line[skip..], at + skip);
        }
        for s in split_sentences(line) {
            let start = at + (s.as_ptr() as usize - line.as_ptr() as usize) + (s.len() - s.trim_start().len());
            let unit = s.trim();
            if !unit.is_empty() && words(unit).len() >= MIN_WORDS {
                out.push(start..start + unit.len());
            }
        }
    }
    out
}

/// A unit of a prompt and where it is, in characters (Unicode scalar values).
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

/// `units` with their positions, in characters.
pub fn unit_spans(text: &str) -> Vec<Span> {
    let chars = |byte: usize| text[..byte].chars().count();
    unit_ranges(text)
        .into_iter()
        .map(|r| Span { text: text[r.clone()].to_string(), start: chars(r.start), end: chars(r.end) })
        .collect()
}

/// The lowercased words of text.
pub fn words(text: &str) -> Vec<String> {
    let lower = text.to_lowercase().replace('’', "'");
    WORD.find_iter(&lower).map(|m| m.as_str().to_string()).collect()
}

pub fn norm(text: &str) -> String {
    words(text).join(" ")
}

/// 2 × matches / total, as difflib's SequenceMatcher.ratio() (matches from a Myers diff).
fn ratio<T: PartialEq + Eq + std::hash::Hash + Ord>(a: &[T], b: &[T]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let ops = capture_diff_slices(Algorithm::Myers, a, b);
    let matches: usize = ops.iter().map(|op| if let DiffOp::Equal { len, .. } = op { *len } else { 0 }).sum();
    2.0 * matches as f64 / (a.len() + b.len()) as f64
}

pub fn similarity(a: &str, b: &str) -> f64 {
    ratio(&words(a), &words(b))
}

/// The words that differ between two near-identical sentences.
pub fn differing_words(a: &str, b: &str) -> (String, String) {
    let (wa, wb): (Vec<&str>, Vec<&str>) = (a.split_whitespace().collect(), b.split_whitespace().collect());
    let key = |ws: &[&str]| -> Vec<String> {
        ws.iter().map(|w| w.trim_matches(|c| ".,;:!?\"'".contains(c)).to_lowercase()).collect()
    };
    let (mut da, mut db) = (vec![], vec![]);
    for op in capture_diff_slices(Algorithm::Myers, &key(&wa), &key(&wb)) {
        let (tag, old, new) = op.as_tag_tuple();
        if tag != DiffTag::Equal {
            da.extend_from_slice(&wa[old]);
            db.extend_from_slice(&wb[new]);
        }
    }
    let trim = |ws: Vec<&str>| ws.join(" ").trim_matches(|c| ".,;:!?".contains(c)).to_string();
    (trim(da), trim(db))
}

/// Where `b`'s words differ from `a`'s, as byte ranges in `b`: one per run of changed words,
/// without surrounding punctuation. The same diff as `differing_words`.
fn differing_ranges(a: &str, b: &str) -> Vec<std::ops::Range<usize>> {
    static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\S+").unwrap());
    let key = |s: &str| -> Vec<String> {
        s.split_whitespace().map(|w| w.trim_matches(|c| ".,;:!?\"'".contains(c)).to_lowercase()).collect()
    };
    let tokens: Vec<std::ops::Range<usize>> = TOKEN.find_iter(b).map(|m| m.range()).collect();
    let mut out = vec![];
    for op in capture_diff_slices(Algorithm::Myers, &key(a), &key(b)) {
        let (tag, _, new) = op.as_tag_tuple();
        if tag == DiffTag::Equal || new.is_empty() {
            continue;
        }
        let (start, end) = (tokens[new.start].start, tokens[new.end - 1].end);
        let text = &b[start..end];
        let trimmed = text.trim_start_matches(|c| ".,;:!?\"'".contains(c));
        let start = start + (text.len() - trimmed.len());
        let end = start + trimmed.trim_end_matches(|c| ".,;:!?\"'".contains(c)).len();
        if start < end {
            out.push(start..end);
        }
    }
    out
}

fn dedupe(items: Vec<Span>) -> Vec<Span> {
    let mut seen = HashSet::new();
    items.into_iter().filter(|u| seen.insert(norm(&u.text))).collect()
}

// --- overlap ---------------------------------------------------------------------------------------

/// Text that several prompts share, with each prompt's own copy (agent, unit).
pub struct Shared {
    pub text: String,
    pub occurrences: Vec<(String, Span)>,
}

impl Shared {
    pub fn agents(&self) -> Vec<String> {
        self.occurrences.iter().map(|(a, _)| a.clone()).collect()
    }
}

/// The same sentence with a different value in some prompts. The maps are by agent.
pub struct NearCopy {
    pub variants: BTreeMap<String, Span>,
    /// The words that differ, and where they are in the variant (byte ranges in its text).
    pub differing: BTreeMap<String, String>,
    differing_ranges: BTreeMap<String, Vec<std::ops::Range<usize>>>,
    /// `rule_word` when a differing word changes what a rule says (a number, a negation, `must`
    /// vs `may`): drift. `value` otherwise: a placeholder candidate, named by `variable`.
    pub kind: &'static str,
    pub variable: Option<String>,
}

/// What a set of prompts shares.
pub struct Overlap {
    pub agents: Vec<String>,
    pub shared: Vec<Shared>,
    pub near_copies: Vec<NearCopy>,
    units: BTreeMap<String, Vec<Span>>,
}

/// Words that change what a rule says: the count words and negations lint checks, and modals.
fn is_rule_word(word: &str) -> bool {
    const MODALS: &[&str] =
        &["always", "must", "may", "should", "shall", "might", "can", "could", "only", "required", "optional"];
    NUMBER_WORDS.iter().any(|(n, _)| *n == word) || NEGATIONS.contains(&word) || MODALS.contains(&word)
}

/// Words before a number that make it a count or limit: "up to 3", "at most 1", "within 5".
const LIMIT_BEFORE: &[&str] = &[
    "to",
    "most",
    "least",
    "than",
    "max",
    "maximum",
    "min",
    "minimum",
    "within",
    "under",
    "over",
    "exceed",
    "exceeding",
    "only",
    "limit",
    "first",
    "last",
    "every",
    "after",
    "before",
];
/// Words after a number that make it a count, frequency or duration: "2 times", "5 minutes".
const COUNT_AFTER: &[&str] = &[
    "time",
    "times",
    "x",
    "attempt",
    "attempts",
    "try",
    "tries",
    "retry",
    "retries",
    "second",
    "seconds",
    "minute",
    "minutes",
    "min",
    "mins",
    "hour",
    "hours",
    "hr",
    "hrs",
    "day",
    "days",
    "week",
    "weeks",
    "month",
    "months",
    "year",
    "years",
    "ring",
    "rings",
    "call",
    "calls",
    "question",
    "questions",
    "item",
    "items",
    "message",
    "messages",
    "offer",
    "offers",
    "per",
    "more",
    "less",
    "fewer",
];

/// Whether the words at `range` in `text` change what a rule says: a count word, a negation or a
/// modal, or a plain whole number used as a count or limit ("up to 3", "2 times"). Other numbers
/// (phone numbers, prices, times, addresses, URLs, ids) are values.
fn changes_rule(text: &str, range: &std::ops::Range<usize>) -> bool {
    static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\S+").unwrap());
    TOKEN.find_iter(&text[range.clone()]).any(|m| {
        let core = m.as_str().trim_matches(|c| ".,;:!?\"'".contains(c));
        if !core.is_empty() && core.bytes().all(|b| b.is_ascii_digit()) {
            let (start, end) = (range.start + m.start(), range.start + m.end());
            let before = words(&text[..start]);
            let after = words(&text[end..]);
            let up_to = before.len() >= 2 && before[before.len() - 2] == "up" && before[before.len() - 1] == "to";
            let limit = before.last().is_some_and(|w| w != "to" && LIMIT_BEFORE.contains(&w.as_str()));
            return up_to || limit || after.first().is_some_and(|w| COUNT_AFTER.contains(&w.as_str()));
        }
        // A token with digits in it (10pm, $12.99, 555-0142, a URL) is a value.
        !core.chars().any(|c| c.is_ascii_digit()) && words(core).iter().any(|w| is_rule_word(w))
    })
}

/// A snake_case name for the value at `range` in `text`: the content words just before it, else
/// just after it.
fn variable_name(text: &str, range: &std::ops::Range<usize>) -> String {
    let content = |s: &str| -> Vec<String> {
        words(s)
            .into_iter()
            .filter(|w| {
                !STOPWORDS.contains(&w.as_str())
                    && !["us", "we", "our", "me", "my", "i", "please"].contains(&w.as_str())
                    && w.chars().all(|c| c.is_ascii_lowercase())
            })
            .collect()
    };
    let before = content(&text[..range.start]);
    let after = content(&text[range.end..]);
    let picked: Vec<String> = if !before.is_empty() {
        before[before.len().saturating_sub(2)..].to_vec()
    } else {
        after.into_iter().take(2).collect()
    };
    if picked.is_empty() {
        "value".into()
    } else {
        picked.join("_")
    }
}

/// Finds what prompts share; sentences at least `near` similar are near-copies.
pub fn overlap(prompts: &BTreeMap<String, String>, near: f64) -> Overlap {
    let agents: Vec<String> = prompts.keys().cloned().collect();
    let per_agent: BTreeMap<String, Vec<Span>> =
        prompts.iter().map(|(a, p)| (a.clone(), dedupe(unit_spans(p)))).collect();

    // norm → [(agent, unit)], in first-seen order
    let mut exact: Vec<(String, Vec<(String, Span)>)> = vec![];
    for (agent, us) in &per_agent {
        for u in us {
            let key = norm(&u.text);
            match exact.iter_mut().find(|(k, _)| *k == key) {
                Some((_, list)) => list.push((agent.clone(), u.clone())),
                None => exact.push((key, vec![(agent.clone(), u.clone())])),
            }
        }
    }

    let singles: Vec<&(String, Vec<(String, Span)>)> = exact.iter().filter(|(_, l)| l.len() < agents.len()).collect();
    let mut used: HashSet<&str> = HashSet::new();
    let mut names: HashSet<String> = HashSet::new();
    let mut near_copies = vec![];
    for (i, (key, list)) in singles.iter().map(|e| (&e.0, &e.1)).enumerate() {
        if used.contains(key.as_str()) {
            continue;
        }
        // Variants in the order they were merged: it decides which one each is compared with.
        let mut variants: Vec<(String, Span)> = list.clone();
        for (other_key, other) in singles[i + 1..].iter().map(|e| (&e.0, &e.1)) {
            if used.contains(other_key.as_str()) || other.iter().any(|(a, _)| variants.iter().any(|(b, _)| a == b)) {
                continue;
            }
            if similarity(key, other_key) >= near {
                variants.extend(other.iter().cloned());
                used.insert(other_key);
            }
        }
        let norms: HashSet<String> = variants.iter().map(|(_, v)| norm(&v.text)).collect();
        if variants.len() < 2 || norms.len() < 2 {
            continue;
        }
        used.insert(key);
        let (mut differing, mut ranges) = (BTreeMap::new(), BTreeMap::new());
        for (agent, span) in &variants {
            // Compared with the first variant that words it differently.
            let other = variants.iter().find(|(_, o)| norm(&o.text) != norm(&span.text)).map_or("", |(_, o)| &o.text);
            differing.insert(agent.clone(), differing_words(other, &span.text).1);
            ranges.insert(agent.clone(), differing_ranges(other, &span.text));
        }
        let rule = variants.iter().any(|(agent, span)| ranges[agent].iter().any(|r| changes_rule(&span.text, r)));
        let variable = (!rule).then(|| {
            let (agent, span) = variants.iter().min_by(|a, b| a.0.cmp(&b.0)).unwrap();
            let base = ranges[agent].first().map_or("value".into(), |r| variable_name(&span.text, r));
            let mut name = base.clone();
            for n in 2.. {
                if names.insert(name.clone()) {
                    break;
                }
                name = format!("{base}_{n}");
            }
            name
        });
        let kind = if rule { "rule_word" } else { "value" };
        let variants = variants.into_iter().collect();
        near_copies.push(NearCopy { variants, differing, differing_ranges: ranges, kind, variable });
    }
    let shared = exact
        .iter()
        .filter(|(key, l)| l.len() > 1 && !used.contains(key.as_str()))
        .map(|(_, l)| Shared { text: l[0].1.text.clone(), occurrences: l.clone() })
        .collect();
    Overlap { agents, shared, near_copies, units: per_agent }
}

/// A unit's position as JSON, in characters: `{"agent", "start", "end", "text"}`.
fn occurrence_json(agent: &str, span: &Span) -> Json {
    json!({"agent": agent, "start": span.start, "end": span.end, "text": span.text})
}

impl Overlap {
    /// Sentences that are in one prompt only, by agent.
    fn only_in_one(&self) -> BTreeMap<&str, usize> {
        let mut covered: HashSet<(&str, String)> = HashSet::new();
        for s in &self.shared {
            covered.extend(s.occurrences.iter().map(|(a, u)| (a.as_str(), norm(&u.text))));
        }
        for nc in &self.near_copies {
            covered.extend(nc.variants.iter().map(|(a, v)| (a.as_str(), norm(&v.text))));
        }
        self.agents
            .iter()
            .map(|a| {
                (a.as_str(), self.units[a].iter().filter(|u| !covered.contains(&(a.as_str(), norm(&u.text)))).count())
            })
            .collect()
    }

    pub fn to_json(&self) -> Json {
        let shared: Vec<Json> = self
            .shared
            .iter()
            .map(|s| {
                let occurrences: Vec<Json> = s.occurrences.iter().map(|(a, u)| occurrence_json(a, u)).collect();
                json!({"text": s.text, "agents": s.agents(), "occurrences": occurrences})
            })
            .collect();
        let near_copies: Vec<Json> = self
            .near_copies
            .iter()
            .map(|nc| {
                let mut by_variant = serde_json::Map::new();
                for (agent, span) in &nc.variants {
                    let entry = by_variant.entry(span.text.clone()).or_insert_with(|| json!([]));
                    entry.as_array_mut().unwrap().push(json!(agent));
                }
                let occurrences: Vec<Json> = nc
                    .variants
                    .iter()
                    .map(|(agent, span)| {
                        let chars = |byte: usize| span.start + span.text[..byte].chars().count();
                        let differing: Vec<Json> = nc.differing_ranges[agent]
                            .iter()
                            .map(|r| json!({"start": chars(r.start), "end": chars(r.end), "text": &span.text[r.clone()]}))
                            .collect();
                        let mut o = occurrence_json(agent, span);
                        o["differing"] = json!(differing);
                        o
                    })
                    .collect();
                json!({
                    "kind": nc.kind,
                    "agents_by_variant": by_variant,
                    "differing_tokens": nc.differing,
                    "suggested_variable": nc.variable,
                    "occurrences": occurrences,
                })
            })
            .collect();
        json!({
            "agents": self.agents,
            "shared": shared,
            "near_copies": near_copies,
            "only_in_one": self.only_in_one(),
        })
    }

    pub fn text(&self) -> String {
        let mut out = vec![format!("{} prompts: {}", self.agents.len(), self.agents.join(", ")), String::new()];
        let mut groups: Vec<(Vec<String>, Vec<&str>)> = vec![];
        for s in &self.shared {
            let agents = s.agents();
            match groups.iter_mut().find(|(a, _)| *a == agents) {
                Some((_, texts)) => texts.push(&s.text),
                None => groups.push((agents, vec![&s.text])),
            }
        }
        groups.sort_by(|(a, _), (b, _)| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        for (agents, texts) in groups {
            let label = if agents.len() == self.agents.len() { "ALL agents".to_string() } else { agents.join(", ") };
            out.push(format!("Shared by {label} ({} sentences):", texts.len()));
            out.extend(texts.iter().map(|t| format!("  - {t}")));
            out.push(String::new());
        }
        if !self.near_copies.is_empty() {
            out.push("Near-copies (same sentence, different value; use a {{placeholder}}):".into());
            for nc in &self.near_copies {
                let note = match &nc.variable {
                    Some(name) => format!("  (placeholder: {{{{{name}}}}})"),
                    None => "  (drift: a rule word differs)".into(),
                };
                out.push(format!("  - {}{note}", nc.variants.values().next().unwrap().text));
                out.extend(nc.differing.iter().map(|(a, d)| format!("      {a}: {}", quote(d))));
            }
            out.push(String::new());
        }
        let only: Vec<String> = self.only_in_one().iter().map(|(a, n)| format!("{a} {n}")).collect();
        out.push(format!("Only in one prompt (sentences): {}", only.join(", ")));
        out.join("\n") + "\n"
    }
}

// --- compare ---------------------------------------------------------------------------------------

/// How one rendered prompt covers its original.
#[derive(Debug, Default)]
pub struct Comparison {
    pub agent: String,
    pub total: usize,
    pub missing: Vec<String>,
    pub changed: Vec<(String, String)>,       // (original, rendered)
    pub reworded: Vec<(String, Vec<String>)>, // (original, the compiled units it was spread over)
    pub added: Vec<String>,
}

impl Comparison {
    /// The share of original sentences kept.
    pub fn coverage(&self) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        self.kept() as f64 / self.total as f64
    }
    /// Original sentences found in the compiled prompt, as written or reworded.
    pub fn kept(&self) -> usize {
        self.total - self.missing.len() - self.changed.len()
    }
    /// Nothing was lost or changed.
    pub fn ok(&self) -> bool {
        self.missing.is_empty() && self.changed.is_empty()
    }
}

fn found(unit: &str, pool: &[String], threshold: f64) -> bool {
    let n = norm(unit);
    pool.iter().any(|p| {
        let np = norm(p);
        n == np || np.contains(&n) || similarity(unit, p) >= threshold
    })
}

/// The share of a unit's content words found in `pool`.
fn word_coverage(unit: &str, pool: &HashSet<String>) -> f64 {
    let content: Vec<String> = words(unit).into_iter().filter(|w| !STOPWORDS.contains(&w.as_str())).collect();
    if content.is_empty() {
        return 1.0;
    }
    content.iter().filter(|w| pool.contains(*w)).count() as f64 / content.len() as f64
}

/// The rendered units the unit's words are spread over, if it was split across several.
fn split_across(unit: &str, pool: &[String]) -> Option<Vec<String>> {
    let unit_words: HashSet<String> = words(unit).into_iter().collect();
    let parts: Vec<&String> =
        pool.iter().filter(|p| !GENERATED.is_match(p) && word_coverage(p, &unit_words) >= 0.6).collect();
    if parts.len() < 2 {
        return None;
    }
    let union: HashSet<String> = parts.iter().flat_map(|p| words(p)).collect();
    (word_coverage(unit, &union) >= 0.7).then(|| parts.into_iter().cloned().collect())
}

/// Checks each rendered prompt still says everything its original said.
pub fn compare(build: &Build, originals: &BTreeMap<String, String>, threshold: f64) -> Vec<Comparison> {
    let mut results = vec![];
    for (agent, original) in originals {
        let rendered = build.agents.get(agent).map(|r| units(&r.prompt)).unwrap_or_default();
        let original_units = units(original);
        let mut c = Comparison { agent: agent.clone(), total: original_units.len(), ..Comparison::default() };
        for u in &original_units {
            if found(u, &rendered, threshold) {
                continue;
            }
            let closest =
                rendered.iter().map(|p| (p, similarity(u, p))).fold(None, |best: Option<(&String, f64)>, (p, s)| {
                    match best {
                        Some((_, b)) if b >= s => best,
                        _ => Some((p, s)),
                    }
                });
            if let Some(parts) = split_across(u, &rendered) {
                c.reworded.push((u.clone(), parts));
            } else if let Some((p, _)) = closest.filter(|(_, s)| *s >= 0.5) {
                c.changed.push((u.clone(), p.clone()));
            } else {
                c.missing.push(u.clone());
            }
        }
        let original_words: HashSet<String> = words(original).into_iter().collect();
        for u in &rendered {
            if !GENERATED.is_match(u)
                && !found(u, &original_units, threshold)
                && word_coverage(u, &original_words) < 0.9
            {
                c.added.push(u.clone());
            }
        }
        results.push(c);
    }
    results
}

/// Agents the build has but no original was given for.
fn without_original<'a>(results: &[Comparison], build: &'a Build) -> Vec<&'a str> {
    build.agents.keys().filter(|id| !results.iter().any(|r| &r.agent == *id)).map(String::as_str).collect()
}

pub fn compare_json(results: &[Comparison], build: &Build) -> Json {
    let agents: Vec<Json> = results
        .iter()
        .map(|r| {
            let changed: Vec<Json> = r
                .changed
                .iter()
                .map(|(original, compiled)| {
                    let (was, now) = differing_words(original, compiled);
                    json!({"original": original, "compiled": compiled, "changed_words": {"original": was, "compiled": now}})
                })
                .collect();
            let reworded: Vec<Json> =
                r.reworded.iter().map(|(u, parts)| json!({"original": u, "compiled_candidates": parts})).collect();
            json!({
                "agent": r.agent,
                "ok": r.ok(),
                "sentences_total": r.total,
                "kept": r.kept(),
                "score": r.coverage(),
                "lost": r.missing.iter().map(|u| json!({"text": u})).collect::<Vec<_>>(),
                "changed": changed,
                "reworded": reworded,
                "added": r.added.iter().map(|u| json!({"text": u})).collect::<Vec<_>>(),
            })
        })
        .collect();
    let failed = results.iter().filter(|r| !r.ok()).count();
    json!({
        "summary": {"ok": failed == 0, "agents": results.len(), "failed": failed},
        "agents": agents,
        "no_original": without_original(results, build),
    })
}

pub fn compare_text(results: &[Comparison], build: &Build) -> String {
    let mut out = vec![];
    for r in results {
        let status = if r.ok() { "ok" } else { "LOST OR CHANGED TEXT" };
        out.push(format!("{}: {:.0}% of {} sentences kept ({status})", r.agent, r.coverage() * 100.0, r.total));
        out.extend(r.missing.iter().map(|u| format!("  - missing:  {u}")));
        for (original, rendered) in &r.changed {
            let (was, now) = differing_words(original, rendered);
            out.push(format!("  ! changed:  {original}"));
            out.push(format!("              now: {rendered}   ({} → {})", quote(&was), quote(&now)));
        }
        out.extend(r.reworded.iter().map(|(u, _)| format!("  ~ reworded: {u}")));
        out.extend(r.added.iter().map(|u| format!("  + added:    {u}")));
    }
    let unmatched = without_original(results, build);
    if !unmatched.is_empty() {
        out.push(format!("no original for: {}", unmatched.join(", ")));
    }
    out.join("\n") + "\n"
}

// --- lint ------------------------------------------------------------------------------------------

/// A duplicate or mechanical conflict. Sources are (block label, text).
#[derive(Debug)]
pub struct Finding {
    pub code: &'static str,
    pub message: String,
    pub sources: Vec<(String, String)>,
    pub agents: Vec<String>,
}

impl Finding {
    pub fn to_json(&self) -> Json {
        let sources: Vec<Json> =
            self.sources.iter().map(|(block, text)| json!({"block": block, "text": text})).collect();
        json!({"code": self.code, "message": self.message, "sources": sources, "agents": self.agents})
    }
}

/// What `conflict` compares, worked out once per sentence: lint compares every pair.
struct Unit {
    /// Equal for units with the same `norm`.
    norm_id: usize,
    /// Word ids, for `similarity`; `sorted` is the same multiset, sorted.
    words: Vec<u32>,
    sorted: Vec<u32>,
    negated: bool,
    /// Word ids without negations and "always", for the negation check.
    stripped: Vec<u32>,
    stripped_sorted: Vec<u32>,
    /// Numbers (number words as digits), and the text with each one as `#`.
    numbers: Vec<String>,
    /// The masked text with a space on each side, its length in chars, and its fields.
    masked: String,
    masked_chars: usize,
    masked_fields: Vec<u32>,
    masked_sorted: Vec<u32>,
}

/// Units by text, and word ids by word, shared by every agent in a lint run.
#[derive(Default)]
struct Units {
    units: Vec<Unit>,
    by_text: HashMap<String, usize>,
    word_ids: HashMap<String, u32>,
    norm_ids: HashMap<String, usize>,
}

impl Units {
    fn id(&mut self, text: &str) -> usize {
        if let Some(&i) = self.by_text.get(text) {
            return i;
        }
        let mut word_id = |w: &String| -> u32 {
            let next = self.word_ids.len() as u32;
            *self.word_ids.entry(w.clone()).or_insert(next)
        };
        let ws = words(text);
        let ids: Vec<u32> = ws.iter().map(&mut word_id).collect();
        let stripped: Vec<u32> =
            ws.iter().filter(|w| !NEGATIONS.contains(&w.as_str()) && *w != "always").map(&mut word_id).collect();
        let norm = ws.join(" ");
        let num = numerals(&norm);
        let masked = NUMBER.replace_all(&num, "#").into_owned();
        let next = self.norm_ids.len();
        let norm_id = *self.norm_ids.entry(norm).or_insert(next);
        let masked_fields: Vec<u32> = masked.split_whitespace().map(|f| word_id(&f.to_string())).collect();
        let sorted_of = |v: &Vec<u32>| {
            let mut v = v.clone();
            v.sort_unstable();
            v
        };
        let unit = Unit {
            sorted: sorted_of(&ids),
            words: ids,
            negated: ws.iter().any(|w| NEGATIONS.contains(&w.as_str())),
            stripped_sorted: sorted_of(&stripped),
            stripped,
            numbers: NUMBER.find_iter(&num).map(|m| m.as_str().to_string()).collect(),
            masked_sorted: sorted_of(&masked_fields),
            masked_fields,
            masked_chars: masked.chars().count(),
            masked: format!(" {masked} "),
            norm_id,
        };
        self.units.push(unit);
        self.by_text.insert(text.to_string(), self.units.len() - 1);
        self.units.len() - 1
    }
}

/// Whether `ratio` of two sequences with these sorted elements can reach `threshold`: matches
/// can't exceed the elements they share.
fn ratio_can_reach(a: &[u32], b: &[u32], threshold: f64) -> bool {
    let total = (a.len() + b.len()) as f64;
    if total == 0.0 || 2.0 * a.len().min(b.len()) as f64 / total < threshold {
        return total == 0.0;
    }
    2.0 * shared(a, b) as f64 / total >= threshold
}

/// How many elements two sorted sequences share, counting repeats.
fn shared(a: &[u32], b: &[u32]) -> usize {
    let (mut i, mut j, mut n) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => (n, i, j) = (n + 1, i + 1, j + 1),
        }
    }
    n
}

/// What `conflict` returns for the units' texts, skipping comparisons that can't succeed.
fn unit_conflict(a: &Unit, b: &Unit) -> Option<&'static str> {
    if a.norm_id == b.norm_id {
        return Some("duplicate_text");
    }
    if !a.numbers.is_empty() && !b.numbers.is_empty() && a.numbers != b.numbers && masked_match(a, b) {
        return Some("numeric_conflict");
    }
    if a.negated != b.negated
        && !a.stripped.is_empty()
        && ratio_can_reach(&a.stripped_sorted, &b.stripped_sorted, 0.9)
        && ratio(&a.stripped, &b.stripped) >= 0.9
    {
        return Some("negation_conflict");
    }
    if ratio_can_reach(&a.sorted, &b.sorted, 0.85) && ratio(&a.words, &b.words) >= 0.85 {
        return Some("near_duplicate");
    }
    None
}

/// The second half of `numbers_differ_in_same_sentence`, on precomputed texts.
fn masked_match(a: &Unit, b: &Unit) -> bool {
    let (short, long) = if b.masked_chars < a.masked_chars { (b, a) } else { (a, b) };
    if a.masked == b.masked {
        return true;
    }
    // Containment needs every field of the shorter text in the longer one.
    let n = short.masked_fields.len();
    if n >= 3 && shared(&short.masked_sorted, &long.masked_sorted) == n && long.masked.contains(&short.masked) {
        return true;
    }
    ratio_can_reach(&a.masked_sorted, &b.masked_sorted, 0.8) && ratio(&a.masked_fields, &b.masked_fields) >= 0.8
}

/// `unit_conflict` on two texts, to compare with `conflict` in tests.
#[cfg(test)]
pub fn fast_conflict(a: &str, b: &str) -> Option<&'static str> {
    let mut units = Units::default();
    let (i, j) = (units.id(a), units.id(b));
    unit_conflict(&units.units[i], &units.units[j])
}

/// The direct definition `unit_conflict` must agree with; tests compare the two.
#[cfg(test)]
pub fn conflict(a: &str, b: &str) -> Option<&'static str> {
    let (na, nb) = (norm(a), norm(b));
    if na == nb {
        return Some("duplicate_text");
    }
    if numbers_differ_in_same_sentence(&na, &nb) {
        return Some("numeric_conflict");
    }
    let (wa, wb) = (words(a), words(b));
    let negated = |ws: &[String]| ws.iter().any(|w| NEGATIONS.contains(&w.as_str()));
    let strip = |ws: &[String]| -> Vec<String> {
        ws.iter().filter(|w| !NEGATIONS.contains(&w.as_str()) && *w != "always").cloned().collect()
    };
    let (sa, sb) = (strip(&wa), strip(&wb));
    if negated(&wa) != negated(&wb) && !sa.is_empty() && ratio(&sa, &sb) >= 0.9 {
        return Some("negation_conflict");
    }
    if similarity(a, b) >= 0.85 {
        return Some("near_duplicate");
    }
    None
}

/// Number words that rules use for counts, as digits.
const NUMBER_WORDS: &[(&str, &str)] = &[
    ("once", "1"),
    ("twice", "2"),
    ("thrice", "3"),
    ("one", "1"),
    ("two", "2"),
    ("three", "3"),
    ("four", "4"),
    ("five", "5"),
    ("six", "6"),
    ("seven", "7"),
    ("eight", "8"),
    ("nine", "9"),
    ("ten", "10"),
];

/// Normalized text with number words written as digits.
fn numerals(norm: &str) -> String {
    let words = norm.split(' ').map(|w| NUMBER_WORDS.iter().find(|(n, _)| *n == w).map_or(w, |(_, d)| d));
    words.collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
fn numbers_differ_in_same_sentence(na: &str, nb: &str) -> bool {
    let (na, nb) = (&numerals(na), &numerals(nb));
    let nums = |s: &str| -> Vec<String> { NUMBER.find_iter(s).map(|m| m.as_str().to_string()).collect() };
    let (numbers_a, numbers_b) = (nums(na), nums(nb));
    if numbers_a.is_empty() || numbers_b.is_empty() || numbers_a == numbers_b {
        return false;
    }
    let (ma, mb) = (NUMBER.replace_all(na, "#").into_owned(), NUMBER.replace_all(nb, "#").into_owned());
    let (short, long) = if mb.chars().count() < ma.chars().count() { (&mb, &ma) } else { (&ma, &mb) };
    if ma == mb || (short.split_whitespace().count() >= 3 && format!(" {long} ").contains(&format!(" {short} "))) {
        return true;
    }
    let fields = |s: &str| -> Vec<String> { s.split_whitespace().map(String::from).collect() };
    ratio(&fields(&ma), &fields(&mb)) >= 0.8
}

/// (block label, text) for every block an agent uses; `split` breaks texts into units.
fn agent_texts(ws: &Workspace, agent: &Agent, split: bool) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut add = |label: String, text: &str| {
        if split {
            out.extend(units(text).into_iter().map(|u| (label.clone(), u)));
        } else {
            out.push((label, text.to_string()));
        }
    };
    add(format!("agent `{}`", agent.id), &agent.context);
    for block in resolve_blocks(ws, agent) {
        let s = match block {
            Block::Instruction(i) => {
                add(format!("instruction `{}`", i.id), &i.text);
                continue;
            }
            Block::Sop(s) => s,
        };
        let mut parts = vec![s.description.clone(), s.scope.clone(), s.guidance.clone()];
        for (list, kind) in [
            (&s.procedure_steps, StepKind::Step),
            (&s.forbidden_actions, StepKind::Forbidden),
            (&s.warning_signs, StepKind::Warning),
        ] {
            parts.extend(list.iter().map(|st| render_step(st, kind)));
        }
        parts.retain(|p| !p.is_empty());
        add(format!("SOP `{}`", s.id), &parts.join("\n"));
    }
    out
}

/// Duplicated text and mechanical conflicts within each agent's prompt, and unused variables.
pub fn lint(ws: &Workspace) -> Vec<Finding> {
    let mut units = Units::default();
    lint_with(ws, |sourced| {
        let ids: Vec<usize> = sourced.iter().map(|(_, t)| units.id(t)).collect();
        pairs(ids.len(), |i, j| unit_conflict(&units.units[ids[i]], &units.units[ids[j]]))
    })
}

/// `lint` comparing every pair with `conflict` directly: the reference the fast one must match.
#[cfg(test)]
pub fn lint_naive(ws: &Workspace) -> Vec<Finding> {
    lint_with(ws, |sourced| pairs(sourced.len(), |i, j| conflict(&sourced[i].1, &sourced[j].1)))
}

/// (i, j, code) for every i < j that `compare` finds a conflict in, in order.
fn pairs(n: usize, compare: impl Fn(usize, usize) -> Option<&'static str>) -> Vec<(usize, usize, &'static str)> {
    let mut out = vec![];
    for i in 0..n {
        out.extend((i + 1..n).filter_map(|j| compare(i, j).map(|code| (i, j, code))));
    }
    out
}

/// Lint, given what conflicts among an agent's (block, text) pairs.
fn lint_with(
    ws: &Workspace,
    mut conflicts: impl FnMut(&[(String, String)]) -> Vec<(usize, usize, &'static str)>,
) -> Vec<Finding> {
    let mut findings: Vec<Finding> = vec![];
    let mut index: HashMap<String, usize> = HashMap::new(); // key → position in findings
    let mut agents: Vec<&Agent> = ws.agents.iter().collect();
    agents.sort_by(|a, b| a.id.cmp(&b.id));
    for agent in agents {
        let values = ws.config.variables.merged(&agent.variables);
        let sourced: Vec<(String, String)> =
            agent_texts(ws, agent, true).into_iter().map(|(b, t)| (b, fill_variables(&t, &values))).collect();
        for (i, j, code) in conflicts(&sourced) {
            let (a, b) = (&sourced[i], &sourced[j]);
            if code == "near_duplicate" && a.0 == b.0 {
                continue;
            }
            let key = [code, &a.0, &norm(&a.1), &b.0, &norm(&b.1)].join("\0");
            if let Some(&n) = index.get(&key) {
                findings[n].agents.push(agent.id.clone());
                continue;
            }
            let message = match code {
                "duplicate_text" => "Same sentence appears twice in the prompt",
                "numeric_conflict" => "Same sentence with different numbers",
                "negation_conflict" => "One block says it, another says the opposite",
                _ => "Nearly identical sentences; a copy that drifted?",
            };
            index.insert(key, findings.len());
            findings.push(Finding {
                code,
                message: message.into(),
                sources: vec![a.clone(), b.clone()],
                agents: vec![agent.id.clone()],
            });
        }
        let used: BTreeSet<String> =
            agent_texts(ws, agent, false).iter().flat_map(|(_, t)| find_variables(t)).collect();
        let unused: BTreeSet<&String> =
            agent.variables.0.iter().map(|(k, _)| k).filter(|k| !used.contains(*k)).collect();
        for name in unused {
            let key = format!("unused_variable\0{}\0{name}", agent.id);
            if index.contains_key(&key) {
                continue;
            }
            index.insert(key, findings.len());
            findings.push(Finding {
                code: "unused_variable",
                message: format!("'{name}' is set but no block this agent uses mentions {{{{{name}}}}}"),
                sources: vec![(format!("agent `{}`", agent.id), name.clone())],
                agents: vec![agent.id.clone()],
            });
        }
    }
    findings
}

pub fn lint_text(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return "No duplicates or mechanical conflicts found.\n".into();
    }
    let mut out =
        vec![format!("{} finding(s). These are advisory; decide which text is right.", findings.len()), String::new()];
    for f in findings {
        out.push(format!("[{}] {} (agents: {})", f.code, f.message, f.agents.join(", ")));
        out.extend(f.sources.iter().map(|(block, text)| format!("    {block}: {text}")));
        out.push(String::new());
    }
    out.join("\n")
}
