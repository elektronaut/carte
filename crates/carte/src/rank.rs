//! Ranks lines by how well they match a query, with BM25.

use std::collections::HashMap;

/// How quickly repeating a word stops adding to a line's score.
const K1: f64 = 1.2;
/// How much a long line's score is scaled down.
const B: f64 = 0.75;

/// The indexes of the `top` lines that best match `query`, best first. Lines that
/// share no word with the query are left out, and of equally good lines the later
/// one comes first.
pub fn rank(lines: &[&str], query: &str, top: usize) -> Vec<usize> {
    let mut terms = words(query);
    terms.sort();
    terms.dedup();
    if terms.is_empty() || lines.is_empty() {
        return Vec::new();
    }
    let docs: Vec<Vec<String>> = lines.iter().map(|line| words(line)).collect();
    let average = docs.iter().map(Vec::len).sum::<usize>() as f64 / docs.len() as f64;
    let count = docs.len() as f64;
    let idf: HashMap<&str, f64> = terms
        .iter()
        .map(|term| {
            let with = docs.iter().filter(|doc| doc.contains(term)).count() as f64;
            (
                term.as_str(),
                (1.0 + (count - with + 0.5) / (with + 0.5)).ln(),
            )
        })
        .collect();

    let mut scored: Vec<(usize, f64)> = docs
        .iter()
        .enumerate()
        .filter_map(|(i, doc)| {
            let length = doc.len() as f64 / average.max(1.0);
            let score: f64 = terms
                .iter()
                .map(|term| {
                    let tf = doc.iter().filter(|w| *w == term).count() as f64;
                    idf[term.as_str()] * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * length))
                })
                .sum();
            (score > 0.0).then_some((i, score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
    scored.into_iter().take(top).map(|(i, _)| i).collect()
}

/// Lowercased runs of letters, digits and underscores.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_matches_come_first() {
        let lines = [
            "Started GET /users",
            "NoMethodError: undefined method `name' for nil",
            "Completed 200 OK",
            "undefined local variable",
        ];
        assert_eq!(rank(&lines, "undefined method", 10), [1, 3]);
    }

    #[test]
    fn rare_words_count_more_than_common_ones() {
        let lines = [
            "request finished",
            "request finished",
            "request finished",
            "request timeout",
        ];
        assert_eq!(rank(&lines, "request timeout", 1), [3]);
    }

    #[test]
    fn ignores_case_and_punctuation() {
        let lines = ["ActiveRecord::RecordNotFound (id=4)", "nothing here"];
        assert_eq!(rank(&lines, "recordnotfound", 10), [0]);
    }

    #[test]
    fn of_equal_lines_the_latest_comes_first() {
        let lines = ["timeout", "ok", "timeout"];
        assert_eq!(rank(&lines, "timeout", 10), [2, 0]);
    }

    #[test]
    fn keeps_only_the_top_lines() {
        let lines = ["error one", "error two", "error three"];
        assert_eq!(rank(&lines, "error", 2).len(), 2);
    }

    #[test]
    fn nothing_matches_an_empty_query() {
        assert!(rank(&["anything"], " :: ", 10).is_empty());
        assert!(rank(&[], "anything", 10).is_empty());
    }
}
