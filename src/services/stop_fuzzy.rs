//! Fuzzy stop-name search for `/stops/{gtfs_id}/fuzzy/{query}`.
//!
//! The spelled-match rules follow the consumer app's stop search
//! (`searchStationsFast` in consumer/src-v2/multimodal/utils/PublicTransportUtils.ts).
//! On top of them, the search tolerates the ways Indian place names are
//! written in Latin letters, so the same place matches however it is spelled.
//!
//! Index: `StopSearchIndex` is built once per feed when the feed's stops are
//! precomputed, and is replaced together with them on every reload. For each
//! stop it keeps the lower-cased name, its a-z/0-9 form and tokens, the stop
//! code, the phonetically normalized name and words, each word's consonant
//! skeleton, and a trigram posting list over the normalized names. A search
//! therefore prepares only the query.
//!
//! Normalization (`normalize`), applied to every feed alike: non-alphanumerics
//! become spaces; zh becomes l, ph becomes f, w and v become b; an h after a
//! consonant is dropped; sh, ck, c, q, x, z and y are folded to s, k, k, k, ks,
//! s and i; long vowels (ee, ii, oo, uu, aa) are shortened; the -pettai and
//! -ore / -oor endings are unified; repeated letters are collapsed. The
//! consonant skeleton (`skeleton`) also removes vowels and folds the voiced
//! consonants b, g, d and j into p, k, t and s, since these are not
//! distinguished in several Indian scripts. One rule set serves every city:
//! measured per city it matched or beat language-specific variants.
//!
//! Scoring, highest first; each stop keeps its best score:
//! - 1.0: the name equals the query.
//! - 0.9: the name starts with the query, or its first tokens equal the
//!   query's tokens, ignoring punctuation between words.
//! - 0.85: the query's tokens appear as consecutive tokens later in the name.
//! - 0.8: the name contains the query, starting inside a word.
//! - 0.75: every query word starts a word of the name, in any order.
//! - 0.7: the name contains the query once both are reduced to a-z/0-9.
//! - 0.695 to 0.67: the same checks on the normalized name and query: equal,
//!   starts with, consecutive words later in the name, all words in any
//!   order, and contained once spaces are removed.
//! - 0.6 to 0.66: consecutive words of the name have the query's consonant
//!   skeleton and start with the same letter, scaled by how close those words
//!   are to the query.
//! - 0.35 to 0.59: typo match on the normalized strings (the app's
//!   `calculateSimilarityRatio`), kept only at a similarity of 0.7 or more and
//!   scaled so closer names rank higher.
//! - Stop codes match as well: 1.0 for an exact code, 0.8 for a partial one.
//!
//! Candidates: the typo pass only scores stops that share at least half of the
//! normalized query's trigrams. This keeps the best match and adds nothing; it
//! can only drop weak typo matches.
//!
//! Queries shorter than two characters match nothing. The contains, phonetic
//! and typo tiers need at least three characters. Ties are ordered by shorter
//! name first, then name, then code, and each stop code appears once.
//!
//! Extra words: when a multi-word query matches nothing, its last word is
//! dropped and the search repeated until something matches or one word is
//! left, so a trailing landmark or abbreviation the feed does not use does not
//! hide the place itself.
//!
//! Limit: with a limit, a lower tier is skipped once at least `limit` stops
//! already score above anything that tier can produce, so the result is always
//! the first `limit` entries of the ranking. Without a limit every match is
//! returned, because callers filter and page the list themselves.
//!
//! Differences from the app: the app has no token, phonetic or skeleton tiers
//! and no trigram filter, accepts typo matches from a similarity of 0.5 and
//! scores them all alike, keeps one result per name (this keeps every stop
//! code, since stops on opposite sides of a road share a name), and caps
//! results at 20 by stopping inside a tier, so it can miss better matches.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::models::RouteStopMapping;

const TYPO_THRESHOLD: f64 = 0.7;
const TRIGRAM_SHARE: f64 = 0.5;
const GENERIC_WORD_SHARE: f64 = 0.01;

static NON_ALNUM_SPACE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^a-z0-9 ]").expect("valid regex"));
static ASPIRATED: Lazy<Regex> = Lazy::new(|| Regex::new(r"([bcdgjkpt])h").expect("valid regex"));
static LONG_I: Lazy<Regex> = Lazy::new(|| Regex::new(r"ee|ii").expect("valid regex"));
static LONG_U: Lazy<Regex> = Lazy::new(|| Regex::new(r"oo|uu").expect("valid regex"));
static PET_SUFFIX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"pett?(ai|ah|a)?\b").expect("valid regex"));
static UR_SUFFIX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(ore|oor|ure|uru)\b").expect("valid regex"));
static SPACES: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").expect("valid regex"));

fn collapse_repeats(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = None;
    for c in s.chars() {
        if c.is_ascii_lowercase() && last == Some(c) {
            continue;
        }
        out.push(c);
        last = Some(c);
    }
    out
}

fn normalize(name: &str) -> String {
    let mut s = NON_ALNUM_SPACE
        .replace_all(&name.to_lowercase(), " ")
        .into_owned();
    s = s
        .replace("zh", "l")
        .replace("ph", "f")
        .replace(['w', 'v'], "b")
        .replace("ch", "c");
    s = ASPIRATED.replace_all(&s, "$1").into_owned();
    s = s
        .replace("sh", "s")
        .replace("ck", "k")
        .replace(['c', 'q'], "k")
        .replace('x', "ks")
        .replace('z', "s")
        .replace('y', "i");
    s = LONG_I.replace_all(&s, "i").into_owned();
    s = LONG_U.replace_all(&s, "u").into_owned();
    s = s.replace("aa", "a");
    s = PET_SUFFIX.replace_all(&s, "pet").into_owned();
    s = UR_SUFFIX.replace_all(&s, "ur").into_owned();
    s = collapse_repeats(&s);
    SPACES.replace_all(&s, " ").trim().to_string()
}

fn skeleton(text: &str) -> String {
    let joined: String = normalize(text).chars().filter(|c| *c != ' ').collect();
    let consonants: String = joined
        .chars()
        .filter(|c| !matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'h'))
        .map(|c| match c {
            'b' => 'p',
            'g' => 'k',
            'd' => 't',
            'j' => 's',
            c => c,
        })
        .collect();
    collapse_repeats(&consonants)
}

fn split_whitespace_js(s: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = 0;
    let mut in_space = false;
    for (i, c) in s.char_indices() {
        if c.is_whitespace() {
            if !in_space {
                words.push(&s[start..i]);
                in_space = true;
            }
        } else if in_space {
            start = i;
            in_space = false;
        }
    }
    words.push(if in_space { "" } else { &s[start..] });
    words
}

fn alnum_ascii(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}

fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

fn trigrams(normalized: &str) -> HashSet<u32> {
    let padded = format!("  {} ", normalized.replace(' ', "  "));
    padded
        .as_bytes()
        .windows(3)
        .map(|w| u32::from(w[0]) << 16 | u32::from(w[1]) << 8 | u32::from(w[2]))
        .collect()
}

#[derive(Debug, Default)]
struct Prepared {
    text: String,
    chars: Vec<char>,
    words: Vec<String>,
    has_space: bool,
}

impl Prepared {
    fn new(lower: &str) -> Self {
        Prepared {
            text: lower.to_string(),
            chars: lower.chars().collect(),
            words: split_whitespace_js(lower)
                .into_iter()
                .map(str::to_string)
                .collect(),
            has_space: lower.contains(' '),
        }
    }
}

#[derive(Default)]
struct Scratch {
    previous: Vec<usize>,
    current: Vec<usize>,
}

fn levenshtein_bounded(a: &[char], b: &[char], max: usize, scratch: &mut Scratch) -> usize {
    if a.len().abs_diff(b.len()) > max {
        return max + 1;
    }
    if a == b {
        return 0;
    }
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let (a, b) = if b.len() > a.len() { (b, a) } else { (a, b) };
    let Scratch { previous, current } = scratch;
    previous.clear();
    previous.extend(0..=b.len());
    current.clear();
    current.resize(b.len() + 1, 0);
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        let mut row_min = current[0];
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != cb);
            let value = (previous[j + 1] + 1).min(current[j] + 1).min(substitution);
            current[j + 1] = value;
            row_min = row_min.min(value);
        }
        std::mem::swap(previous, current);
        if row_min > max {
            return max + 1;
        }
    }
    previous[b.len()]
}

fn similarity_prepared(s1: &Prepared, s2: &Prepared, scratch: &mut Scratch) -> f64 {
    if s1.text.is_empty() && s2.text.is_empty() {
        return 1.0;
    }
    if s1.text.is_empty() || s2.text.is_empty() {
        return 0.0;
    }
    let prefix_bonus = if s1.text.starts_with(&s2.text) || s2.text.starts_with(&s1.text) {
        0.3
    } else {
        0.0
    };
    let word_prefix_bonus = if (s1.has_space || s2.has_space)
        && s1.words.iter().any(|x| {
            s2.words
                .iter()
                .any(|y| x.starts_with(y.as_str()) || y.starts_with(x.as_str()))
        }) {
        0.2
    } else {
        0.0
    };
    let max_len = s1.chars.len().max(s2.chars.len());
    let max_distance = (max_len as f64 * 0.6).floor() as usize;
    let distance = levenshtein_bounded(&s1.chars, &s2.chars, max_distance, scratch);
    if distance > max_distance {
        return prefix_bonus + word_prefix_bonus;
    }
    let base = 1.0 - distance as f64 / max_len as f64;
    (base + prefix_bonus + word_prefix_bonus).min(1.0)
}

pub fn similarity_ratio(a: &str, b: &str) -> f64 {
    similarity_prepared(
        &Prepared::new(&a.to_lowercase()),
        &Prepared::new(&b.to_lowercase()),
        &mut Scratch::default(),
    )
}

fn token_run(name_tokens: &[String], query_tokens: &[String]) -> Option<usize> {
    let k = query_tokens.len();
    if k == 0 || name_tokens.len() < k {
        return None;
    }
    (0..=name_tokens.len() - k).find(|&i| {
        query_tokens[..k - 1]
            .iter()
            .zip(&name_tokens[i..i + k - 1])
            .all(|(q, n)| q == n)
            && name_tokens[i + k - 1].starts_with(query_tokens[k - 1].as_str())
    })
}

fn all_words_any_order(name_tokens: &[String], query_tokens: &[String]) -> bool {
    query_tokens.len() > 1
        && query_tokens
            .iter()
            .all(|q| name_tokens.iter().any(|n| n.starts_with(q.as_str())))
}

fn skeleton_run(
    word_skeletons: &[String],
    query_skeleton: &str,
    query_words: usize,
) -> Option<(usize, usize)> {
    (0..word_skeletons.len()).find_map(|i| {
        let end = (i + query_words).min(word_skeletons.len());
        (word_skeletons[i..end].concat() == query_skeleton).then_some((i, end))
    })
}

#[derive(Debug)]
struct IndexedStop {
    stop: Arc<RouteStopMapping>,
    name: String,
    alnum: String,
    tokens: Vec<String>,
    code: String,
    normalized: Prepared,
    normalized_alnum: String,
    normalized_words: Vec<Prepared>,
    word_skeletons: Vec<String>,
}

struct Query {
    term: String,
    len: usize,
    alnum: String,
    tokens: Vec<String>,
    normalized: Prepared,
    normalized_words: Vec<Prepared>,
    normalized_len: usize,
    normalized_alnum: String,
    skeleton: String,
}

#[derive(Debug, Default)]
pub struct StopSearchIndex {
    entries: Vec<IndexedStop>,
    trigram_postings: HashMap<u32, Vec<u32>>,
    word_counts: HashMap<String, usize>,
    generic_min_count: usize,
}

impl StopSearchIndex {
    pub fn build(stops: &[Arc<RouteStopMapping>]) -> Self {
        let mut trigram_postings: HashMap<u32, Vec<u32>> = HashMap::new();
        let entries: Vec<IndexedStop> = stops
            .iter()
            .enumerate()
            .map(|(i, stop)| {
                let lower = stop.stop_name.to_lowercase();
                let normalized_text = normalize(&stop.stop_name);
                for gram in trigrams(&normalized_text) {
                    trigram_postings.entry(gram).or_default().push(i as u32);
                }
                let normalized = Prepared::new(&normalized_text);
                IndexedStop {
                    stop: stop.clone(),
                    alnum: alnum_ascii(&lower),
                    tokens: tokens(&lower),
                    code: stop.stop_code.to_lowercase(),
                    normalized_alnum: normalized_text.replace(' ', ""),
                    normalized_words: normalized.words.iter().map(|w| Prepared::new(w)).collect(),
                    word_skeletons: normalized.words.iter().map(|w| skeleton(w)).collect(),
                    normalized,
                    name: lower,
                }
            })
            .collect();
        let mut word_counts: HashMap<&str, usize> = HashMap::new();
        for entry in &entries {
            let words: HashSet<&str> = entry.normalized.words.iter().map(String::as_str).collect();
            for word in words {
                *word_counts.entry(word).or_default() += 1;
            }
        }
        let generic_min_count =
            ((GENERIC_WORD_SHARE * entries.len() as f64).ceil() as usize).max(2);
        let word_counts = word_counts
            .into_iter()
            .map(|(word, count)| (word.to_string(), count))
            .collect();
        StopSearchIndex {
            entries,
            trigram_postings,
            word_counts,
            generic_min_count,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn heap_bytes(&self) -> usize {
        let prepared = |p: &Prepared| {
            p.text.len() + p.chars.len() * 4 + p.words.iter().map(|w| w.len() + 24).sum::<usize>()
        };
        let entries: usize = self
            .entries
            .iter()
            .map(|e| {
                e.name.len()
                    + e.alnum.len()
                    + e.tokens.iter().map(|t| t.len() + 24).sum::<usize>()
                    + e.code.len()
                    + prepared(&e.normalized)
                    + e.normalized_alnum.len()
                    + e.normalized_words.iter().map(prepared).sum::<usize>()
                    + e.word_skeletons.iter().map(|w| w.len() + 24).sum::<usize>()
                    + 8
            })
            .sum();
        let postings: usize = self
            .trigram_postings
            .values()
            .map(|v| v.len() * 4 + 32)
            .sum();
        let generic: usize = self.word_counts.keys().map(|w| w.len() + 32).sum();
        entries + postings + generic
    }

    fn candidates(&self, q: &Query) -> Vec<bool> {
        let grams = trigrams(&q.normalized.text);
        let need = ((TRIGRAM_SHARE * grams.len() as f64).ceil() as usize).max(1);
        let mut shared = vec![0u16; self.entries.len()];
        let mut keep = vec![false; self.entries.len()];
        for gram in &grams {
            if let Some(posting) = self.trigram_postings.get(gram) {
                for &i in posting {
                    let i = i as usize;
                    shared[i] += 1;
                    if usize::from(shared[i]) >= need {
                        keep[i] = true;
                    }
                }
            }
        }
        keep
    }

    fn required_words<'q>(&self, q: &'q Query) -> Vec<&'q Prepared> {
        let count = |w: &Prepared| self.word_counts.get(&w.text).copied().unwrap_or(0);
        let distinctive: Vec<&Prepared> = q
            .normalized_words
            .iter()
            .filter(|w| count(w) < self.generic_min_count)
            .collect();
        if !distinctive.is_empty() {
            return distinctive;
        }
        q.normalized_words
            .iter()
            .min_by(|a, b| {
                count(a)
                    .cmp(&count(b))
                    .then_with(|| b.chars.len().cmp(&a.chars.len()))
            })
            .into_iter()
            .collect()
    }

    fn closeness(
        entry: &IndexedStop,
        q: &Query,
        required: &[&Prepared],
        scratch: &mut Scratch,
    ) -> Option<f64> {
        let name = &entry.normalized;
        let term = &q.normalized.text;
        if name.text.contains(term.as_str()) {
            return Some(1.0);
        }
        if q.normalized_len <= 2 {
            return name.text.starts_with(term.as_str()).then_some(1.0);
        }
        if name.words.iter().any(|w| w.starts_with(term.as_str())) {
            return Some(1.0);
        }
        if q.normalized_words.len() > 1
            && !required.iter().all(|query_word| {
                entry.normalized_words.iter().any(|name_word| {
                    name_word.text.starts_with(query_word.text.as_str())
                        || similarity_prepared(name_word, query_word, scratch) >= TYPO_THRESHOLD
                })
            })
        {
            return None;
        }
        let mut best = similarity_prepared(name, &q.normalized, scratch);
        let query_words = q.normalized_words.len();
        if query_words == 1 || q.normalized_len < 8 {
            for word in &entry.normalized_words {
                best = best.max(similarity_prepared(word, &q.normalized, scratch));
            }
        }
        if query_words > 1 && entry.normalized_words.len() > query_words {
            for run in name.words.windows(query_words) {
                let run = Prepared::new(&run.join(" "));
                best = best.max(similarity_prepared(&run, &q.normalized, scratch));
            }
        }
        Some(best)
    }

    pub fn search(&self, query: &str, limit: Option<usize>) -> Vec<(Arc<RouteStopMapping>, f64)> {
        let mut words: Vec<&str> = query.split_whitespace().collect();
        loop {
            let found = self.search_with(&words.join(" "), limit, true);
            if !found.is_empty() || words.len() <= 1 {
                return found;
            }
            words.pop();
        }
    }

    fn search_with(
        &self,
        query: &str,
        limit: Option<usize>,
        prefilter: bool,
    ) -> Vec<(Arc<RouteStopMapping>, f64)> {
        let term = query.trim().to_lowercase();
        let len = term.chars().count();
        if len <= 1 || limit == Some(0) {
            return Vec::new();
        }
        let normalized_text = normalize(query);
        let q = Query {
            alnum: alnum_ascii(&term),
            tokens: tokens(&term),
            normalized_len: normalized_text.chars().count(),
            normalized_alnum: normalized_text.replace(' ', ""),
            skeleton: skeleton(query),
            normalized_words: split_whitespace_js(&normalized_text)
                .into_iter()
                .map(Prepared::new)
                .collect(),
            normalized: Prepared::new(&normalized_text),
            len,
            term,
        };
        let normalized_tokens: Vec<String> = q.normalized.words.clone();

        let n = self.entries.len();
        let mut best = vec![0.0f64; n];
        let mut named = vec![false; n];

        for (i, e) in self.entries.iter().enumerate() {
            if e.code == q.term {
                best[i] = 1.0;
            } else if e.code.contains(&q.term) {
                best[i] = 0.8;
            }
        }

        let enough_above = |best: &[f64], floor: f64| match limit {
            Some(l) => best.iter().filter(|&&s| s > floor).count() >= l,
            None => false,
        };

        for (i, e) in self.entries.iter().enumerate() {
            let score = if e.name == q.term {
                1.0
            } else if e.name.starts_with(&q.term) || token_run(&e.tokens, &q.tokens) == Some(0) {
                0.9
            } else {
                continue;
            };
            named[i] = true;
            best[i] = best[i].max(score);
        }

        if q.len > 2 && !enough_above(&best, 0.85) {
            let mut scratch = Scratch::default();
            for (i, e) in self.entries.iter().enumerate() {
                if named[i] {
                    continue;
                }
                let spelled = if token_run(&e.tokens, &q.tokens).is_some() {
                    0.85
                } else if e.name.contains(&q.term) {
                    0.8
                } else if all_words_any_order(&e.tokens, &q.tokens) {
                    0.75
                } else if !q.alnum.is_empty() && e.alnum.contains(&q.alnum) {
                    0.7
                } else {
                    0.0
                };
                let score = if spelled > 0.0 || q.normalized_len <= 2 {
                    spelled
                } else if e.normalized.text == q.normalized.text {
                    0.695
                } else if e.normalized.text.starts_with(&q.normalized.text)
                    || token_run(&e.normalized.words, &normalized_tokens) == Some(0)
                {
                    0.685
                } else if token_run(&e.normalized.words, &normalized_tokens).is_some() {
                    0.68
                } else if all_words_any_order(&e.normalized.words, &normalized_tokens) {
                    0.675
                } else if !q.normalized_alnum.is_empty()
                    && e.normalized_alnum.contains(&q.normalized_alnum)
                {
                    0.67
                } else if let Some((start, end)) = (q.skeleton.chars().count() >= 3)
                    .then(|| skeleton_run(&e.word_skeletons, &q.skeleton, normalized_tokens.len()))
                    .flatten()
                    .filter(|&(start, _)| {
                        e.normalized.words[start].chars().next() == q.normalized.text.chars().next()
                    })
                {
                    let run = Prepared::new(&e.normalized.words[start..end].join(" "));
                    let closeness = similarity_prepared(&run, &q.normalized, &mut scratch);
                    0.6 + 0.06 * ((closeness - 0.5) / 0.5).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if score > 0.0 {
                    named[i] = true;
                    best[i] = best[i].max(score);
                }
            }

            if !enough_above(&best, 0.59) {
                let keep = prefilter.then(|| self.candidates(&q));
                let required = self.required_words(&q);
                for (i, e) in self.entries.iter().enumerate() {
                    if named[i] || keep.as_ref().is_some_and(|k| !k[i]) {
                        continue;
                    }
                    if let Some(c) = Self::closeness(e, &q, &required, &mut scratch) {
                        if c >= TYPO_THRESHOLD {
                            best[i] = best[i].max(0.35 + 0.24 * ((c - 0.5) / 0.5).clamp(0.0, 1.0));
                        }
                    }
                }
            }
        }

        let mut results: Vec<(usize, f64)> = best
            .iter()
            .enumerate()
            .filter(|(_, &s)| s > 0.0)
            .map(|(i, &s)| (i, s))
            .collect();
        results.sort_by(|&(a, sa), &(b, sb)| {
            let (a, b) = (&self.entries[a].stop, &self.entries[b].stop);
            sb.total_cmp(&sa)
                .then_with(|| {
                    a.stop_name
                        .chars()
                        .count()
                        .cmp(&b.stop_name.chars().count())
                })
                .then_with(|| a.stop_name.cmp(&b.stop_name))
                .then_with(|| a.stop_code.cmp(&b.stop_code))
        });
        let mut seen = HashSet::new();
        let mut out = Vec::with_capacity(limit.unwrap_or(results.len()).min(results.len()));
        for (i, score) in results {
            let stop = &self.entries[i].stop;
            if seen.insert(stop.stop_code.clone()) {
                out.push((stop.clone(), score));
                if limit.is_some_and(|l| out.len() >= l) {
                    break;
                }
            }
        }
        out
    }
}

pub fn fuzzy_search_stops(
    stops: &[Arc<RouteStopMapping>],
    query: &str,
) -> Vec<(Arc<RouteStopMapping>, f64)> {
    StopSearchIndex::build(stops).search(query, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::LatLong;

    fn stop(code: &str, name: &str) -> Arc<RouteStopMapping> {
        Arc::new(RouteStopMapping {
            estimated_travel_time_from_previous_stop: None,
            provider_code: Arc::from("GTFS"),
            route_code: Arc::from("R1"),
            sequence_num: 1,
            stop_code: Arc::from(code),
            stop_name: Arc::from(name),
            stop_point: LatLong {
                lat: 13.0,
                lon: 80.2,
            },
            vehicle_type: Arc::from("BUS"),
            geo_json: None,
            gates: None,
            hindi_name: None,
            regional_name: None,
            platform: None,
            parent_stop_code: None,
            location_type: "0".to_string(),
            cluster_id: None,
            stage_number: None,
            is_stage_stop: None,
        })
    }

    fn chennai() -> Vec<Arc<RouteStopMapping>> {
        [
            "GUINDY",
            "GUINDY BUS TERMINUS",
            "LITTLE MOUNT GUINDY",
            "SAIDAPET",
            "VADAPALANI",
            "VADAPALANI BUS STAND",
            "T. NAGAR",
            "TAMBARAM WEST",
            "TAMBARAM",
            "KOYAMBEDU",
        ]
        .iter()
        .enumerate()
        .map(|(i, name)| stop(&format!("C{i}"), name))
        .collect()
    }

    fn search(query: &str) -> Vec<(String, f64)> {
        fuzzy_search_stops(&chennai(), query)
            .into_iter()
            .map(|(s, score)| (s.stop_name.to_string(), (score * 10000.0).round() / 10000.0))
            .collect()
    }

    fn named(rows: &[(&str, f64)]) -> Vec<(String, f64)> {
        rows.iter().map(|(n, s)| (n.to_string(), *s)).collect()
    }

    #[test]
    fn exact_then_prefix_then_contains() {
        assert_eq!(
            search("guindy"),
            named(&[
                ("GUINDY", 1.0),
                ("GUINDY BUS TERMINUS", 0.9),
                ("LITTLE MOUNT GUINDY", 0.85),
            ])
        );
    }

    #[test]
    fn a_typo_matches_below_every_spelled_match() {
        let in_typo_band = |rows: &[(String, f64)]| rows.iter().all(|(_, s)| *s > 0.35 && *s < 0.7);
        let gindy = search("gindy");
        assert_eq!(
            gindy.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["GUINDY", "GUINDY BUS TERMINUS", "LITTLE MOUNT GUINDY"]
        );
        assert!(in_typo_band(&gindy), "{gindy:?}");
        let saidapt = search("saidapt");
        assert_eq!(saidapt.len(), 1);
        assert_eq!(saidapt[0].0, "SAIDAPET");
        assert!(in_typo_band(&saidapt));
        let tambram = search("tambram");
        assert_eq!(
            tambram.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["TAMBARAM", "TAMBARAM WEST"]
        );
        assert!(in_typo_band(&tambram));
    }

    #[test]
    fn a_longer_query_prefers_the_name_it_starts() {
        assert_eq!(
            search("vadapalani bus"),
            named(&[("VADAPALANI BUS STAND", 0.9), ("VADAPALANI", 0.59)])
        );
        assert_eq!(search("koyambedu bus"), named(&[("KOYAMBEDU", 0.59)]));
    }

    #[test]
    fn punctuation_between_words_is_ignored() {
        assert_eq!(search("t nagar"), named(&[("T. NAGAR", 0.9)]));
        assert_eq!(search("t. nagar"), named(&[("T. NAGAR", 1.0)]));
    }

    #[test]
    fn a_word_inside_names_matches_by_contains() {
        assert_eq!(
            search("bus"),
            named(&[
                ("GUINDY BUS TERMINUS", 0.85),
                ("VADAPALANI BUS STAND", 0.85)
            ])
        );
    }

    #[test]
    fn one_character_or_unrelated_queries_match_nothing() {
        assert!(search("g").is_empty());
        assert!(search("xyz").is_empty());
        assert!(search("   ").is_empty());
    }

    #[test]
    fn a_tamil_query_does_not_match_every_stop() {
        assert!(search("கிண்டி").is_empty());
    }

    #[test]
    fn every_stop_with_a_matching_name_is_kept() {
        let stops = vec![stop("KERB_B", "GUINDY"), stop("KERB_A", "GUINDY")];
        let codes: Vec<String> = fuzzy_search_stops(&stops, "guindy")
            .into_iter()
            .map(|(s, _)| s.stop_code.to_string())
            .collect();
        assert_eq!(codes, vec!["KERB_A", "KERB_B"]);
    }

    #[test]
    fn similarity_ratio_matches_the_app() {
        assert!((similarity_ratio("guindy", "gindy") - 0.833_333_333_333_333_4).abs() < 1e-12);
    }

    #[test]
    fn stop_codes_still_match_as_the_endpoint_always_did() {
        let stops = vec![
            stop("6f72a539f2", "PORUR"),
            stop("6f72a5aaaa", "VALASARAVAKKAM"),
        ];
        let found: Vec<(String, f64)> = fuzzy_search_stops(&stops, "6F72A539F2")
            .into_iter()
            .map(|(s, score)| (s.stop_code.to_string(), score))
            .collect();
        assert_eq!(found, vec![("6f72a539f2".to_string(), 1.0)]);
        let found: Vec<String> = fuzzy_search_stops(&stops, "6f72a5")
            .into_iter()
            .map(|(s, _)| s.stop_code.to_string())
            .collect();
        assert_eq!(found, vec!["6f72a539f2", "6f72a5aaaa"]);
    }

    fn ranked(names: &[&str], query: &str) -> Vec<String> {
        let stops: Vec<Arc<RouteStopMapping>> = names
            .iter()
            .enumerate()
            .map(|(i, n)| stop(&format!("S{i}"), n))
            .collect();
        StopSearchIndex::build(&stops)
            .search(query, None)
            .into_iter()
            .map(|(s, _)| s.stop_name.to_string())
            .collect()
    }

    #[test]
    fn a_whole_word_match_beats_a_mid_word_contains() {
        assert_eq!(
            ranked(
                &[
                    "BESANT NAGAR",
                    "POWER HOUSE T NAGAR",
                    "T.NAGAR PONDY BAZAAR",
                    "THYAGARAYA NAGAR"
                ],
                "t nagar"
            ),
            vec![
                "T.NAGAR PONDY BAZAAR",
                "POWER HOUSE T NAGAR",
                "BESANT NAGAR",
                "THYAGARAYA NAGAR"
            ]
        );
    }

    #[test]
    fn typo_matches_rank_by_closeness_not_by_name() {
        let found = ranked(
            &["KADAPERI", "PADAPPAI", "VADAPALANI", "VADAPALANI BUS STAND"],
            "vadaplani",
        );
        assert_eq!(found.first().map(String::as_str), Some("VADAPALANI"));
    }

    #[test]
    fn a_limit_keeps_exactly_the_top_of_the_full_ranking() {
        let stops: Vec<Arc<RouteStopMapping>> = [
            "GUINDY",
            "GUINDY BUS TERMINUS",
            "GUINDY B.T",
            "LITTLE MOUNT GUINDY",
            "GUINDY RLY STN",
            "GINDY",
            "SAIDAPET",
            "T. NAGAR",
            "BESANT NAGAR",
            "T.NAGAR BUS TERMINUS",
            "VADAPALANI",
        ]
        .iter()
        .enumerate()
        .map(|(i, n)| stop(&format!("L{i}"), n))
        .collect();
        let index = StopSearchIndex::build(&stops);
        for query in ["guindy", "gindy", "t nagar", "nagar", "gu", "vadaplani"] {
            let full: Vec<String> = index
                .search(query, None)
                .into_iter()
                .map(|(s, _)| s.stop_code.to_string())
                .collect();
            for limit in 0..=full.len() + 1 {
                let limited: Vec<String> = index
                    .search(query, Some(limit))
                    .into_iter()
                    .map(|(s, _)| s.stop_code.to_string())
                    .collect();
                assert_eq!(
                    limited,
                    full[..limit.min(full.len())],
                    "query {query} limit {limit}"
                );
            }
        }
    }

    #[test]
    fn a_code_match_does_not_hide_a_better_name_match() {
        let stops = vec![stop("GUINDY_N", "GUINDY")];
        let found: Vec<f64> = fuzzy_search_stops(&stops, "guindy")
            .into_iter()
            .map(|(_, score)| score)
            .collect();
        assert_eq!(found, vec![1.0]);
    }

    fn ranked_with(names: &[&str], query: &str) -> Vec<String> {
        let stops: Vec<Arc<RouteStopMapping>> = names
            .iter()
            .enumerate()
            .map(|(i, n)| stop(&format!("P{i}"), n))
            .collect();
        StopSearchIndex::build(&stops)
            .search(query, None)
            .into_iter()
            .map(|(s, _)| s.stop_name.to_string())
            .collect()
    }

    #[test]
    fn tamil_spelling_variants_find_the_place() {
        let names = [
            "POONAMALLEE B.T",
            "PONNERI",
            "KONIMEDU",
            "KODAMBAKKAM",
            "KADAPAKKAM",
            "MYLAPORE",
            "KILPAUK",
        ];
        for (query, expected) in [
            ("ponamely", "POONAMALLEE B.T"),
            ("punamalli", "POONAMALLEE B.T"),
            ("kodampakkam", "KODAMBAKKAM"),
            ("mailapur", "MYLAPORE"),
            ("keelpauk", "KILPAUK"),
        ] {
            let found = ranked_with(&names, query);
            assert_eq!(
                found.first().map(String::as_str),
                Some(expected),
                "{query}: {found:?}"
            );
        }
    }

    #[test]
    fn koil_and_kovil_find_each_other() {
        let names = ["AMMAN KOIL", "ALADI AMMAN KOVIL", "AMBATTUR"];
        for query in ["amman kovil", "amman koil"] {
            let found = ranked_with(&names, query);
            assert_eq!(found.len(), 2, "{query}: {found:?}");
            assert!(
                found.iter().all(|n| n.contains("AMMAN")),
                "{query}: {found:?}"
            );
        }
    }

    #[test]
    fn b_v_and_w_are_the_same_letter() {
        let names = [
            "NETAJI BHAVAN",
            "NETAJI NAGAR",
            "ADIBASHI BHABAN",
            "BELGACHIA",
        ];
        for (query, expected) in [
            ("netaji bhaban", "NETAJI BHAVAN"),
            ("netaji bhawan", "NETAJI BHAVAN"),
            ("adibasi bhavan", "ADIBASHI BHABAN"),
        ] {
            let found = ranked_with(&names, query);
            assert_eq!(
                found.first().map(String::as_str),
                Some(expected),
                "{query}: {found:?}"
            );
        }
    }

    #[test]
    fn distant_names_are_no_longer_typo_matches() {
        assert_eq!(
            ranked_with(&["KADAPERI", "PADAPPAI", "VADAPALANI"], "vadaplani"),
            vec!["VADAPALANI"]
        );
    }

    #[test]
    fn the_trigram_prefilter_keeps_the_best_match_and_adds_nothing() {
        let names: Vec<String> = [
            "POONAMALLEE B.T",
            "PONNERI",
            "KONIMEDU",
            "KODAMBAKKAM",
            "KADAPAKKAM",
            "MYLAPORE",
            "KILPAUK",
            "GUINDY",
            "LITTLE MOUNT GUINDY",
            "VADAPALANI",
            "SAIDAPET",
            "TAMBARAM",
            "AMMAN KOIL",
            "ALADI AMMAN KOVIL",
            "THIRUVANMIYUR",
            "KOYAMBEDU",
            "VELACHERY",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let stops: Vec<Arc<RouteStopMapping>> = names
            .iter()
            .enumerate()
            .map(|(i, n)| stop(&format!("T{i}"), n))
            .collect();
        {
            let index = StopSearchIndex::build(&stops);
            for query in [
                "ponamely",
                "gindy",
                "tiruvanmiyur",
                "koyembedu",
                "velacheri",
                "saidapt",
                "tambram",
                "amman kovil",
                "kodampakkam",
                "zzqx",
                "mailapur",
            ] {
                let codes = |prefilter: bool| -> Vec<(String, u64)> {
                    index
                        .search_with(query, None, prefilter)
                        .into_iter()
                        .map(|(s, sc)| (s.stop_code.to_string(), (sc * 1e6) as u64))
                        .collect()
                };
                let (filtered, full) = (codes(true), codes(false));
                assert_eq!(filtered.first(), full.first(), "{query}");
                let mut rest = full.iter();
                assert!(
                    filtered.iter().all(|row| rest.any(|r| r == row)),
                    "{query}: {filtered:?} not an ordered subset of {full:?}"
                );
            }
        }
    }

    #[test]
    fn every_word_of_a_multi_word_query_must_match() {
        let names = [
            "ROYAPETTAH POLICE STATION",
            "SAIDAPET POLICE STATION",
            "KORUKKUPET POLICE STATION",
            "KANNAGI NAGAR POLICE STATION",
            "ADAMBAKKAM POLICE SATION",
        ];
        assert_eq!(
            ranked_with(&names, "royapetah police station"),
            vec!["ROYAPETTAH POLICE STATION"]
        );
        assert_eq!(
            ranked_with(
                &["ALADI AMMAN KOVIL", "AMMAN KOIL", "AMBATTUR"],
                "aladi amman koil"
            ),
            vec!["ALADI AMMAN KOVIL"]
        );
    }

    #[test]
    fn a_consonant_skeleton_match_keeps_the_first_letter_and_ranks_by_closeness() {
        assert_eq!(
            ranked_with(
                &[
                    "AYURVEDA ASHRAMAM",
                    "ROYAPETTA HOSPITAL",
                    "ROYAPETTAH AJANTHA"
                ],
                "royepetah"
            ),
            vec!["ROYAPETTA HOSPITAL", "ROYAPETTAH AJANTHA"]
        );
    }

    #[test]
    fn a_long_misspelled_word_is_found_inside_longer_names() {
        let found = ranked_with(
            &[
                "OLD PERUNGALATHUR",
                "NEW PERUNGALATHUR SRIRAM GATE",
                "PERUNGUDI",
                "TAMBARAM",
            ],
            "perungalatgur",
        );
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            found.iter().all(|n| n.contains("PERUNGALATHUR")),
            "{found:?}"
        );
        let found = ranked_with(
            &["CHROMEPET RAILWAY STATION", "NEW PERUNGALATHUR SRIRAM GATE"],
            "perungalatgur sriram",
        );
        assert_eq!(found, vec!["NEW PERUNGALATHUR SRIRAM GATE"]);
    }

    #[test]
    fn all_query_words_in_any_order_match() {
        assert_eq!(
            ranked_with(
                &[
                    "TAMBARAM TB HOSPITAL",
                    "OTTERI TB HOSPITAL",
                    "TAMBARAM WEST"
                ],
                "tb hospital tambaram"
            ),
            vec!["TAMBARAM TB HOSPITAL"]
        );
    }

    #[test]
    fn trailing_words_are_dropped_when_nothing_matches() {
        assert_eq!(
            ranked_with(
                &["BESANT NAGAR", "SONARPUR JUNCTION", "ADYAR DEPOT"],
                "besant nagar beach"
            ),
            vec!["BESANT NAGAR"]
        );
        assert_eq!(
            ranked_with(
                &["BESANT NAGAR", "SONARPUR JUNCTION", "ADYAR DEPOT"],
                "sonarpur jn"
            ),
            vec!["SONARPUR JUNCTION"]
        );
    }

    #[test]
    fn among_equal_matches_the_shorter_name_comes_first() {
        let found = ranked_with(
            &[
                "MYLAPORE TANK",
                "ISABELLA HOSPITAL MYLAPORE",
                "MYLAPORE NEW WOODLANDS",
                "MYLAPORE",
            ],
            "mylapur",
        );
        assert_eq!(
            found.first().map(String::as_str),
            Some("MYLAPORE"),
            "{found:?}"
        );
        let found = ranked_with(
            &[
                "SHOLINGANALLUR ACCENTURE",
                "GOVERNMENT SCHOOL SHOLINGANALLUR",
                "SHOLINGANALLUR",
            ],
            "sholinganalur",
        );
        assert_eq!(
            found.first().map(String::as_str),
            Some("SHOLINGANALLUR"),
            "{found:?}"
        );
    }
}
