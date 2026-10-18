//! Deterministic text analysis for importing and reviewing prompts: `overlap`, `compare` and
//! `lint`. Prompts are split into sentence-sized units and compared by their words.

use crate::model::{Agent, Block};
use crate::render::{fill_variables, find_variables, render_step, resolve_blocks, Build, StepKind};
use crate::text::{quote, sha256_hex};
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
#[cfg(test)]
pub fn unit_spans(text: &str) -> Vec<Span> {
    located_units(text).into_iter().map(|(span, _)| span).collect()
}

/// `unit_spans`, each with its byte range in the text.
fn located_units(text: &str) -> Vec<(Span, std::ops::Range<usize>)> {
    // Ranges are in order and don't overlap, so characters are counted once, up to each boundary.
    let (mut byte, mut chars) = (0, 0);
    let mut to_chars = |to: usize| {
        chars += text[byte..to].chars().count();
        byte = to;
        chars
    };
    unit_ranges(text)
        .into_iter()
        .map(|r| {
            let (start, end) = (to_chars(r.start), to_chars(r.end));
            (Span { text: text[r.clone()].to_string(), start, end }, r)
        })
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

/// Small integer ids for words, so sentences compare as `u32` slices.
#[derive(Default)]
struct WordIds(HashMap<String, u32>);

impl WordIds {
    fn of(&mut self, text: &str) -> Vec<u32> {
        words(text)
            .into_iter()
            .map(|w| {
                let next = self.0.len() as u32;
                *self.0.entry(w).or_insert(next)
            })
            .collect()
    }
}

fn sorted(ids: &[u32]) -> Vec<u32> {
    let mut v = ids.to_vec();
    v.sort_unstable();
    v
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

#[cfg(test)]
pub fn similarity(a: &str, b: &str) -> f64 {
    ratio(&words(a), &words(b))
}

/// For each sequence, the others whose `ratio` with it is at least `near`, in index order.
///
/// Instead of comparing every pair, candidates come from prefix filtering: `ratio(x, y) >= near`
/// needs at least `r·|x|` shared words (`r = near / (2 - near)`), so when each sequence's words are
/// sorted rarest first, any such pair shares a word among the first `|x| - ⌈r·|x|⌉ + 1` of x and
/// the first `|y| - ⌈r·|y|⌉ + 1` of y. Only those pairs are diffed.
fn similar_pairs(seqs: &[Vec<u32>], near: f64) -> Vec<Vec<usize>> {
    // The nth repeat of a word is its own token, so shared tokens are shared words, repeats counted.
    let mut token_ids: HashMap<(u32, u32), u32> = HashMap::new();
    let mut sets: Vec<Vec<u32>> = seqs
        .iter()
        .map(|seq| {
            let mut repeats: HashMap<u32, u32> = HashMap::new();
            seq.iter()
                .map(|w| {
                    let n = repeats.entry(*w).or_insert(0);
                    *n += 1;
                    let next = token_ids.len() as u32;
                    *token_ids.entry((*w, *n)).or_insert(next)
                })
                .collect()
        })
        .collect();
    let mut frequency = vec![0u32; token_ids.len()];
    for t in sets.iter().flatten() {
        frequency[*t as usize] += 1;
    }
    let sorted_seqs: Vec<Vec<u32>> = seqs.iter().map(|s| sorted(s)).collect();
    let r = near / (2.0 - near);
    let mut postings: HashMap<u32, Vec<usize>> = HashMap::new();
    let mut seen = vec![usize::MAX; seqs.len()];
    let mut out = vec![vec![]; seqs.len()];
    for (i, set) in sets.iter_mut().enumerate() {
        set.sort_unstable_by_key(|t| (frequency[*t as usize], *t));
        let needed = ((r * set.len() as f64 - 1e-9).ceil() as usize).max(1);
        let prefix = (set.len() + 1).saturating_sub(needed).min(set.len());
        let mut candidates = vec![];
        for t in &set[..prefix] {
            let list = postings.entry(*t).or_default();
            for &j in list.iter() {
                if seen[j] != i {
                    seen[j] = i;
                    candidates.push(j);
                }
            }
            list.push(i);
        }
        for j in candidates {
            if ratio_can_reach(&sorted_seqs[j], &sorted_seqs[i], near) && ratio(&seqs[j], &seqs[i]) >= near {
                out[i].push(j);
                out[j].push(i);
            }
        }
    }
    for list in &mut out {
        list.sort_unstable();
    }
    out
}

/// `similar_pairs` on texts, and the same by comparing every pair, for tests.
#[cfg(test)]
pub fn similar_pairs_and_brute_force(texts: &[String], near: f64) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
    let mut ids = WordIds::default();
    let seqs: Vec<Vec<u32>> = texts.iter().map(|t| ids.of(t)).collect();
    let brute = (0..seqs.len())
        .map(|i| (0..seqs.len()).filter(|&j| j != i && ratio(&seqs[i], &seqs[j]) >= near).collect())
        .collect();
    (similar_pairs(&seqs, near), brute)
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

static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\S+").unwrap());

/// A whitespace-separated token without the punctuation around it.
fn token_core(token: &str) -> &str {
    token.trim_matches(|c| ".,;:!?\"'".contains(c))
}

/// Text with the punctuation at its ends removed, and where that is in it (bytes).
fn trim_punctuation(text: &str, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    let part = &text[range.clone()];
    let trimmed = part.trim_start_matches(|c| ".,;:!?\"'".contains(c));
    let start = range.start + (part.len() - trimmed.len());
    start..start + trimmed.trim_end_matches(|c| ".,;:!?\"'".contains(c)).len()
}

/// Where `b`'s words differ from `a`'s, as byte ranges in `b`: one per run of changed words,
/// without surrounding punctuation. The same diff as `differing_words`.
fn differing_ranges(a: &str, b: &str) -> Vec<std::ops::Range<usize>> {
    let key = |s: &str| -> Vec<String> { s.split_whitespace().map(|w| token_core(w).to_lowercase()).collect() };
    let tokens: Vec<std::ops::Range<usize>> = TOKEN.find_iter(b).map(|m| m.range()).collect();
    let mut out = vec![];
    for op in capture_diff_slices(Algorithm::Myers, &key(a), &key(b)) {
        let (tag, _, new) = op.as_tag_tuple();
        if tag == DiffTag::Equal || new.is_empty() {
            continue;
        }
        let range = trim_punctuation(b, tokens[new.start].start..tokens[new.end - 1].end);
        if !range.is_empty() {
            out.push(range);
        }
    }
    out
}

/// A short id for a group in `overlap`'s output: 12 hex characters of the SHA-256 of its kind
/// and normalized texts. The same text gets the same id in every run, whatever the agents.
fn group_id(kind: &str, texts: &[&str]) -> String {
    sha256_hex(&format!("{kind}\x1f{}", texts.join("\x1f")))[..12].to_string()
}

// --- overlap ---------------------------------------------------------------------------------------

/// Text that several prompts share, with each prompt's own copy (agent, unit).
pub struct Shared {
    pub id: String,
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
    pub id: String,
    pub variants: BTreeMap<String, Span>,
    /// The words that differ, and where they are in the variant (byte ranges in its text).
    pub differing: BTreeMap<String, String>,
    differing_ranges: BTreeMap<String, Vec<std::ops::Range<usize>>>,
    /// `rule_word` when a differing word changes what a rule says (a number, a negation, `must`
    /// vs `may`): drift. `value` otherwise: a placeholder candidate, named by `variable`.
    pub kind: &'static str,
    pub variable: Option<String>,
    /// Each variant's text between the words every variant starts and ends with: its value.
    values: BTreeMap<String, String>,
    /// The `Variable` this value near-copy belongs to (an index into `Overlap::variables`).
    variable_index: Option<usize>,
}

/// A value that several near-copies share: the same text in each, for the same agents (a
/// restaurant's name in ten sentences is one variable).
pub struct Variable {
    pub id: String,
    pub name: String,
    pub values: BTreeMap<String, String>,
    /// Indexes into `Overlap::near_copies`, largest first.
    pub near_copies: Vec<usize>,
}

/// Text most prompts have (at least half, and at least two) that some don't, in any wording.
pub struct Missing {
    pub id: String,
    pub text: String,
    /// The shared text or near-copy it comes from: (`shared` or `near_copy`, its id).
    pub source: (&'static str, String),
    pub present_in: Vec<String>,
    pub missing_in: Vec<String>,
}

/// What a set of prompts shares.
pub struct Overlap {
    pub agents: Vec<String>,
    pub shared: Vec<Shared>,
    pub near_copies: Vec<NearCopy>,
    pub variables: Vec<Variable>,
    pub missing: Vec<Missing>,
    /// By agent: runs of consecutive sentences no other prompt has, each as one span.
    pub unique: BTreeMap<String, Vec<Span>>,
    /// By agent: how many of its (distinct) sentences no other prompt has.
    only_in_one: BTreeMap<String, usize>,
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

/// Whether the token at `range` in `text` is a plain whole number used as a count or limit
/// ("up to 3", "2 times"). Other numbers (phone numbers, prices, times, addresses, URLs, ids) are
/// values.
fn is_count(text: &str, range: &std::ops::Range<usize>) -> bool {
    let core = token_core(&text[range.clone()]);
    if core.is_empty() || !core.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let before = words(&text[..range.start]);
    let after = words(&text[range.end..]);
    let up_to = before.len() >= 2 && before[before.len() - 2] == "up" && before[before.len() - 1] == "to";
    let limit = before.last().is_some_and(|w| w != "to" && LIMIT_BEFORE.contains(&w.as_str()));
    up_to || limit || after.first().is_some_and(|w| COUNT_AFTER.contains(&w.as_str()))
}

/// Whether the words at `range` in `text` change what a rule says: a count word, a negation or a
/// modal, or a plain whole number used as a count or limit (`is_count`).
fn changes_rule(text: &str, range: &std::ops::Range<usize>) -> bool {
    TOKEN.find_iter(&text[range.clone()]).any(|m| {
        let token = range.start + m.start()..range.start + m.end();
        let core = token_core(m.as_str());
        if core.bytes().any(|b| b.is_ascii_digit()) {
            // A token with digits in it (10pm, $12.99, 555-0142, a URL) is a value unless it's a count.
            return is_count(text, &token);
        }
        words(core).iter().any(|w| is_rule_word(w))
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

/// Each variant's value: its text between the words that every variant starts with and the words
/// that every variant ends with (compared as `differing_words` does), without end punctuation, and
/// where that is in the variant (bytes).
fn slot_values(variants: &[(String, Span)]) -> BTreeMap<String, (String, std::ops::Range<usize>)> {
    let tokens: Vec<(Vec<std::ops::Range<usize>>, Vec<String>)> = variants
        .iter()
        .map(|(_, v)| {
            let ranges: Vec<std::ops::Range<usize>> = TOKEN.find_iter(&v.text).map(|m| m.range()).collect();
            let keys = ranges.iter().map(|r| token_core(&v.text[r.clone()]).to_lowercase()).collect();
            (ranges, keys)
        })
        .collect();
    let first = &tokens[0].1;
    let shortest = tokens.iter().map(|(_, k)| k.len()).min().unwrap_or(0);
    let common = |n: usize, at: &dyn Fn(&Vec<String>, usize) -> String| {
        (0..n).take_while(|&i| tokens.iter().all(|(_, k)| at(k, i) == at(first, i))).count()
    };
    let prefix = common(shortest, &|k, i| k[i].clone());
    let suffix = common(shortest - prefix, &|k, i| k[k.len() - 1 - i].clone());
    variants
        .iter()
        .zip(&tokens)
        .map(|((agent, v), (ranges, _))| {
            let range = if prefix + suffix < ranges.len() {
                trim_punctuation(&v.text, ranges[prefix].start..ranges[ranges.len() - 1 - suffix].end)
            } else {
                0..0
            };
            (agent.clone(), (v.text[range.clone()].to_string(), range))
        })
        .collect()
}

/// Finds what prompts share; sentences at least `near` similar are near-copies.
pub fn overlap(prompts: &BTreeMap<String, String>, near: f64) -> Overlap {
    let agents: Vec<String> = prompts.keys().cloned().collect();
    // Distinct sentences (by `norm`) in first-seen order, each with the agents that have it.
    let mut keys: Vec<String> = vec![];
    let mut found_in: Vec<Vec<(String, Span)>> = vec![];
    let mut key_of: HashMap<String, usize> = HashMap::new();
    // Every unit of every prompt (repeats too), with its key and byte range.
    type Located = (Span, usize, std::ops::Range<usize>);
    let mut units: BTreeMap<&str, Vec<Located>> = BTreeMap::new();
    for (agent, prompt) in prompts {
        let mut seen = HashSet::new();
        let list = units.entry(agent.as_str()).or_default();
        for (span, bytes) in located_units(prompt) {
            let text = norm(&span.text);
            let k = *key_of.entry(text.clone()).or_insert_with(|| {
                keys.push(text);
                found_in.push(vec![]);
                keys.len() - 1
            });
            if seen.insert(k) {
                found_in[k].push((agent.clone(), span.clone()));
            }
            list.push((span, k, bytes));
        }
    }

    // Near-copies are among sentences that some prompts lack.
    let singles: Vec<usize> = (0..keys.len()).filter(|&k| found_in[k].len() < agents.len()).collect();
    let mut ids = WordIds::default();
    let seqs: Vec<Vec<u32>> = singles.iter().map(|&k| ids.of(&keys[k])).collect();
    let neighbors = similar_pairs(&seqs, near);
    let mut used = vec![false; keys.len()];
    let mut names: HashSet<String> = HashSet::new();
    let mut near_copies = vec![];
    let mut groups: Vec<Vec<usize>> = vec![]; // the keys of each near-copy
    for (i, &key) in singles.iter().enumerate() {
        if used[key] {
            continue;
        }
        // Each sentence like this one, unless its prompts are in the group already.
        let mut members = vec![key];
        let mut in_group: HashSet<&str> = found_in[key].iter().map(|(a, _)| a.as_str()).collect();
        for &j in neighbors[i].iter().filter(|&&j| j > i) {
            let other = singles[j];
            if used[other] || found_in[other].iter().any(|(a, _)| in_group.contains(a.as_str())) {
                continue;
            }
            in_group.extend(found_in[other].iter().map(|(a, _)| a.as_str()));
            members.push(other);
            used[other] = true;
        }
        if members.len() < 2 {
            continue;
        }
        used[key] = true;
        // Variants in the order they were merged: it decides which one each is compared with.
        let variants: Vec<(String, Span, usize)> =
            members.iter().flat_map(|&k| found_in[k].iter().map(move |(a, s)| (a.clone(), s.clone(), k))).collect();
        let (mut differing, mut ranges) = (BTreeMap::new(), BTreeMap::new());
        for (agent, span, k) in &variants {
            // Compared with the first variant that words it differently.
            let other = variants.iter().find(|(_, _, o)| o != k).map_or("", |(_, o, _)| &o.text);
            differing.insert(agent.clone(), differing_words(other, &span.text).1);
            ranges.insert(agent.clone(), differing_ranges(other, &span.text));
        }
        let rule = variants.iter().any(|(agent, span, _)| ranges[agent].iter().any(|r| changes_rule(&span.text, r)));
        let pairs: Vec<(String, Span)> = variants.iter().map(|(a, s, _)| (a.clone(), s.clone())).collect();
        let slots = if rule { BTreeMap::new() } else { slot_values(&pairs) };
        let variable = (!rule).then(|| {
            let (agent, span, _) = variants.iter().min_by(|a, b| a.0.cmp(&b.0)).unwrap();
            // Named from the whole value where every variant has one in the same place.
            let slot = slots.get(agent).map(|(_, r)| r.clone()).filter(|r| !r.is_empty());
            let base = slot
                .or_else(|| ranges[agent].first().cloned())
                .map_or("value".into(), |r| variable_name(&span.text, &r));
            let mut name = base.clone();
            for n in 2.. {
                if names.insert(name.clone()) {
                    break;
                }
                name = format!("{base}_{n}");
            }
            name
        });
        let mut texts: Vec<&str> = members.iter().map(|&k| keys[k].as_str()).collect();
        texts.sort_unstable();
        near_copies.push(NearCopy {
            id: group_id("near_copy", &texts),
            values: slots.into_iter().map(|(a, (value, _))| (a, value)).collect(),
            variants: pairs.into_iter().collect(),
            differing,
            differing_ranges: ranges,
            kind: if rule { "rule_word" } else { "value" },
            variable,
            variable_index: None,
        });
        groups.push(members);
    }
    let shared_keys: Vec<usize> = (0..keys.len()).filter(|&k| found_in[k].len() > 1 && !used[k]).collect();
    let shared: Vec<Shared> = shared_keys
        .iter()
        .map(|&k| Shared {
            id: group_id("shared", &[&keys[k]]),
            text: found_in[k][0].1.text.clone(),
            occurrences: found_in[k].clone(),
        })
        .collect();

    let variables = variables(&mut near_copies);

    // Missing: text in most prompts but not all, from the shared sentences and the near-copies.
    // A prompt with a sentence like it (at least `near` alike) has it in other words.
    let single_of: HashMap<usize, usize> = singles.iter().enumerate().map(|(i, &k)| (k, i)).collect();
    let mut missing: Vec<Missing> = vec![];
    let sources =
        shared_keys.iter().zip(&shared).map(|(&k, s)| (vec![k], s.text.clone(), ("shared", s.id.clone()))).chain(
            groups.iter().zip(&near_copies).map(|(members, nc)| {
                // The wording most of its prompts use.
                let most =
                    members.iter().copied().max_by(|a, b| found_in[*a].len().cmp(&found_in[*b].len()).then(b.cmp(a)));
                (members.clone(), found_in[most.unwrap()][0].1.text.clone(), ("near_copy", nc.id.clone()))
            }),
        );
    for (members, text, source) in sources {
        let present: BTreeSet<&str> =
            members.iter().flat_map(|&k| found_in[k].iter().map(|(a, _)| a.as_str())).collect();
        if present.len() < 2 || present.len() * 2 < agents.len() || present.len() == agents.len() {
            continue;
        }
        let mut alike: HashSet<&str> = HashSet::new();
        for &k in &members {
            for &j in &neighbors[single_of[&k]] {
                alike.extend(found_in[singles[j]].iter().map(|(a, _)| a.as_str()));
            }
        }
        let missing_in: Vec<String> =
            agents.iter().filter(|a| !present.contains(a.as_str()) && !alike.contains(a.as_str())).cloned().collect();
        if missing_in.is_empty() {
            continue;
        }
        missing.push(Missing {
            id: group_id("missing", &[source.0, &source.1]),
            text,
            source,
            present_in: present.into_iter().map(String::from).collect(),
            missing_in,
        });
    }
    missing.sort_by(|a, b| b.present_in.len().cmp(&a.present_in.len()));

    // Unique: units whose sentence no other prompt has, in any wording; consecutive ones join.
    let alone = |k: usize| found_in[k].len() == 1 && !used[k];
    let mut unique = BTreeMap::new();
    let mut only_in_one = BTreeMap::new();
    for (agent, list) in &units {
        let prompt = &prompts[*agent];
        // (bytes, chars) of each run of units no other prompt has.
        let mut runs: Vec<(std::ops::Range<usize>, std::ops::Range<usize>)> = vec![];
        let mut previous_alone = false;
        for (span, k, bytes) in list {
            let is_alone = alone(*k);
            match runs.last_mut() {
                Some(run) if is_alone && previous_alone => (run.0.end, run.1.end) = (bytes.end, span.end),
                _ if is_alone => runs.push((bytes.clone(), span.start..span.end)),
                _ => {}
            }
            previous_alone = is_alone;
        }
        let spans = runs
            .into_iter()
            .map(|(bytes, chars)| Span { text: prompt[bytes].to_string(), start: chars.start, end: chars.end })
            .collect::<Vec<_>>();
        let distinct: HashSet<usize> = list.iter().map(|(_, k, _)| *k).filter(|&k| alone(k)).collect();
        only_in_one.insert(agent.to_string(), distinct.len());
        unique.insert(agent.to_string(), spans);
    }
    Overlap { agents, shared, near_copies, variables, missing, unique, only_in_one }
}

/// Groups the value near-copies whose values agree into variables: the same text for each agent,
/// where one near-copy's agents are all among the other's. Larger near-copies come first, and each
/// value near-copy is in exactly one variable.
fn variables(near_copies: &mut [NearCopy]) -> Vec<Variable> {
    let mut order: Vec<usize> = (0..near_copies.len()).filter(|&i| near_copies[i].kind == "value").collect();
    order.sort_by_key(|&i| std::cmp::Reverse(near_copies[i].variants.len()));
    // Each variable's values by agent, normalized, and the variable.
    let mut found: Vec<(BTreeMap<&str, String>, Variable)> = vec![];
    for i in order {
        let nc = &near_copies[i];
        let values: BTreeMap<&str, String> = nc.values.iter().map(|(a, v)| (a.as_str(), norm(v))).collect();
        let fits = |known: &BTreeMap<&str, String>| values.iter().all(|(a, v)| known.get(a) == Some(v));
        if let Some((_, variable)) = found.iter_mut().find(|(known, _)| fits(known)) {
            variable.near_copies.push(i);
            continue;
        }
        let name = nc.variable.clone().unwrap_or_else(|| "value".into());
        let variable = Variable { id: String::new(), name, values: nc.values.clone(), near_copies: vec![i] };
        found.push((values, variable));
    }
    let mut out: Vec<Variable> = found.into_iter().map(|(_, v)| v).collect();
    for (index, variable) in out.iter_mut().enumerate() {
        let mut ids: Vec<&str> = variable.near_copies.iter().map(|&i| near_copies[i].id.as_str()).collect();
        ids.sort_unstable();
        variable.id = group_id("variable", &ids);
        for &i in &variable.near_copies {
            near_copies[i].variable_index = Some(index);
        }
    }
    out
}

/// A unit's position as JSON, in characters: `{"agent", "start", "end", "text"}`.
fn occurrence_json(agent: &str, span: &Span) -> Json {
    json!({"agent": agent, "start": span.start, "end": span.end, "text": span.text})
}

impl Overlap {
    pub fn to_json(&self) -> Json {
        let shared: Vec<Json> = self
            .shared
            .iter()
            .map(|s| {
                let occurrences: Vec<Json> = s.occurrences.iter().map(|(a, u)| occurrence_json(a, u)).collect();
                json!({"id": s.id, "text": s.text, "agents": s.agents(), "occurrences": occurrences})
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
                    "id": nc.id,
                    "kind": nc.kind,
                    "agents_by_variant": by_variant,
                    "differing_tokens": nc.differing,
                    "suggested_variable": nc.variable,
                    "variable_id": nc.variable_index.map(|i| &self.variables[i].id),
                    "occurrences": occurrences,
                })
            })
            .collect();
        let variables: Vec<Json> = self
            .variables
            .iter()
            .map(|v| {
                let occurrences: Vec<&str> = v.near_copies.iter().map(|&i| self.near_copies[i].id.as_str()).collect();
                json!({"id": v.id, "suggested_name": v.name, "values": v.values, "occurrences": occurrences})
            })
            .collect();
        let missing: Vec<Json> = self
            .missing
            .iter()
            .map(|m| {
                json!({
                    "id": m.id,
                    "text": m.text,
                    "present_in": m.present_in,
                    "missing_in": m.missing_in,
                    "source": {"kind": m.source.0, "id": m.source.1},
                })
            })
            .collect();
        let unique: Vec<Json> = self
            .unique
            .iter()
            .map(|(agent, spans)| {
                let spans: Vec<Json> =
                    spans.iter().map(|s| json!({"start": s.start, "end": s.end, "text": s.text})).collect();
                json!({"agent": agent, "spans": spans})
            })
            .collect();
        json!({
            "agents": self.agents,
            "shared": shared,
            "near_copies": near_copies,
            "only_in_one": self.only_in_one,
            "variables": variables,
            "missing": missing,
            "unique": unique,
        })
    }

    pub fn text(&self) -> String {
        let mut out = vec![format!("{} prompts: {}", self.agents.len(), self.agents.join(", ")), String::new()];
        let mut groups: Vec<(Vec<String>, Vec<&str>)> = vec![];
        let mut index: HashMap<Vec<String>, usize> = HashMap::new();
        for s in &self.shared {
            let agents = s.agents();
            match index.get(&agents) {
                Some(&i) => groups[i].1.push(&s.text),
                None => {
                    index.insert(agents.clone(), groups.len());
                    groups.push((agents, vec![&s.text]));
                }
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
        if self.variables.iter().any(|v| v.near_copies.len() > 1) {
            out.push("Variables (the same value in several near-copies):".into());
            for v in self.variables.iter().filter(|v| v.near_copies.len() > 1) {
                let examples: Vec<String> =
                    v.values.iter().take(3).map(|(a, x)| format!("{a}: {}", quote(x))).collect();
                let more = if v.values.len() > 3 { ", ..." } else { "" };
                out.push(format!(
                    "  - {{{{{}}}}} in {} sentences, {} agents ({}{more})",
                    v.name,
                    v.near_copies.len(),
                    v.values.len(),
                    examples.join(", ")
                ));
            }
            out.push(String::new());
        }
        if !self.missing.is_empty() {
            out.push("Missing from some prompts (most have it):".into());
            for m in &self.missing {
                out.push(format!("  - {}", m.text));
                out.push(format!("      missing in: {}", m.missing_in.join(", ")));
            }
            out.push(String::new());
        }
        let only: Vec<String> = self.only_in_one.iter().map(|(a, n)| format!("{a} {n}")).collect();
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

/// Sentences to look units up in, prepared once: their norms, and word ids for `ratio`.
struct Pool<'a> {
    units: &'a [String],
    norms: Vec<String>,
    exact: HashSet<String>,
    ids: Vec<Vec<u32>>,
    sorted: Vec<Vec<u32>>,
}

impl<'a> Pool<'a> {
    fn new(units: &'a [String], word_ids: &mut WordIds) -> Self {
        let norms: Vec<String> = units.iter().map(|u| norm(u)).collect();
        let ids: Vec<Vec<u32>> = units.iter().map(|u| word_ids.of(u)).collect();
        Pool {
            units,
            exact: norms.iter().cloned().collect(),
            sorted: ids.iter().map(|i| sorted(i)).collect(),
            norms,
            ids,
        }
    }

    /// Whether a sentence of the pool is the unit, contains it, or is at least `threshold` alike.
    fn found(&self, unit: &str, word_ids: &mut WordIds, threshold: f64) -> bool {
        let n = norm(unit);
        if self.exact.contains(&n) {
            return true;
        }
        let (ids, sorted_ids) = (word_ids.of(unit), sorted(&word_ids.of(unit)));
        (0..self.units.len()).any(|i| {
            self.norms[i].contains(&n)
                || (ratio_can_reach(&sorted_ids, &self.sorted[i], threshold) && ratio(&ids, &self.ids[i]) >= threshold)
        })
    }

    /// The most alike sentence (the first of equals), and how alike.
    fn closest(&self, unit: &str, word_ids: &mut WordIds) -> Option<(&'a String, f64)> {
        let ids = word_ids.of(unit);
        (0..self.units.len()).map(|i| (&self.units[i], ratio(&ids, &self.ids[i]))).fold(None, |best, (p, s)| match best
        {
            Some((_, b)) if b >= s => best,
            _ => Some((p, s)),
        })
    }
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
    let rendered: BTreeMap<&str, &str> = build.agents.iter().map(|(id, r)| (id.as_str(), r.prompt.as_str())).collect();
    compare_prompts(&rendered, originals, threshold)
}

/// `compare`, given each agent's compiled prompt.
pub fn compare_prompts(
    compiled: &BTreeMap<&str, &str>,
    originals: &BTreeMap<String, String>,
    threshold: f64,
) -> Vec<Comparison> {
    let mut results = vec![];
    for (agent, original) in originals {
        let rendered = compiled.get(agent.as_str()).map(|p| units(p)).unwrap_or_default();
        let original_units = units(original);
        let mut word_ids = WordIds::default();
        let (rendered_pool, original_pool) =
            (Pool::new(&rendered, &mut word_ids), Pool::new(&original_units, &mut word_ids));
        let mut c = Comparison { agent: agent.clone(), total: original_units.len(), ..Comparison::default() };
        for u in &original_units {
            if rendered_pool.found(u, &mut word_ids, threshold) {
                continue;
            }
            let closest = rendered_pool.closest(u, &mut word_ids);
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
                && !original_pool.found(u, &mut word_ids, threshold)
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
    /// Counts and limits (number words as digits; see `rule_numbers`), and the words with each as `#`.
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
        let (numbers, masked) = rule_numbers(text);
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
            numbers,
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
    if numbers_differ_in_same_sentence(a, b) {
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

/// The counts and limits in text, as digits: number words (`once`, `two`), and whole numbers
/// that `is_count` (`up to 3`, `2 times`); and the text's words with each of them as `#`. Other
/// numbers (times, prices, phone numbers, ids) are values, as in `overlap`, and stay words.
fn rule_numbers(text: &str) -> (Vec<String>, String) {
    let (mut numbers, mut masked) = (vec![], vec![]);
    for m in TOKEN.find_iter(text) {
        if is_count(text, &m.range()) {
            numbers.push(token_core(m.as_str()).to_string());
            masked.push("#".to_string());
            continue;
        }
        for w in words(m.as_str()) {
            match NUMBER_WORDS.iter().find(|(n, _)| *n == w) {
                Some((_, digits)) => {
                    numbers.push(digits.to_string());
                    masked.push("#".to_string());
                }
                None => masked.push(w),
            }
        }
    }
    (numbers, masked.join(" "))
}

#[cfg(test)]
fn numbers_differ_in_same_sentence(a: &str, b: &str) -> bool {
    let ((numbers_a, ma), (numbers_b, mb)) = (rule_numbers(a), rule_numbers(b));
    if numbers_a.is_empty() || numbers_b.is_empty() || numbers_a == numbers_b {
        return false;
    }
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
