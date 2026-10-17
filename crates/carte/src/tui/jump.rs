//! Ctrl-P: jump to any node of the tree by typing part of its name.

use super::target;
use super::tree::{Node, NodeId};

/// The jump prompt: what was typed and which match is picked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Jump {
    pub query: String,
    pub cursor: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: NodeId,
    /// The node as the CLI would address it, with its project when that isn't obvious.
    pub name: String,
}

/// Every level of the tree, merged levels included, in tree order.
pub fn candidates(roots: &[Node]) -> Vec<Candidate> {
    let mut out = Vec::new();
    for node in roots {
        collect(node, &mut out);
    }
    out
}

fn collect(node: &Node, out: &mut Vec<Candidate>) {
    for id in &node.ids {
        let name = match id {
            NodeId::Stack { project, .. } => format!("{project} {}", target::label(id)),
            _ => target::label(id),
        };
        if !out.iter().any(|c| c.id == *id) {
            out.push(Candidate {
                id: id.clone(),
                name,
            });
        }
    }
    for child in &node.children {
        collect(child, out);
    }
}

/// The candidates whose names hold the query's characters in order, best first:
/// fewer gaps, then an earlier start, then shorter names.
pub fn matches<'a>(candidates: &'a [Candidate], query: &str) -> Vec<&'a Candidate> {
    let query: Vec<char> = query.to_lowercase().chars().collect();
    let mut scored: Vec<(usize, usize, usize, &Candidate)> = candidates
        .iter()
        .filter_map(|c| {
            let (gaps, start) = fit(&c.name.to_lowercase(), &query)?;
            Some((gaps, start, c.name.len(), c))
        })
        .collect();
    scored.sort_by_key(|(gaps, start, len, _)| (*gaps, *start, *len));
    scored.into_iter().map(|(.., c)| c).collect()
}

/// Where `query` fits in `name` as a subsequence: the fewest characters skipped
/// between its first and last match, and where that match starts.
fn fit(name: &str, query: &[char]) -> Option<(usize, usize)> {
    if query.is_empty() {
        return Some((0, 0));
    }
    let chars: Vec<char> = name.chars().collect();
    (0..chars.len())
        .filter(|&start| chars[start] == query[0])
        .filter_map(|start| {
            let mut at = start;
            let mut gaps = 0;
            for q in &query[1..] {
                let next = chars[at + 1..].iter().position(|c| c == q)?;
                gaps += next;
                at += next + 1;
            }
            Some((gaps, start))
        })
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::fixtures::sample;
    use crate::tui::tree;

    fn names(found: Vec<&Candidate>) -> Vec<&str> {
        found.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn lists_every_level() {
        let all = candidates(&tree::build(&sample()));
        assert_eq!(
            names(all.iter().collect()),
            [
                "nova",
                "nova @main",
                "nova-billing@main",
                "nova-billing@main/web",
                "nova-billing@main/worker",
                "nova-frontend@main",
                "nova @feature-x",
                "nova-billing@feature-x",
                "nova-frontend@feature-x",
            ]
        );
    }

    #[test]
    fn ranks_tight_matches_first() {
        let all = candidates(&tree::build(&sample()));
        assert_eq!(names(matches(&all, "frontfx")), ["nova-frontend@feature-x"]);
        assert_eq!(
            names(matches(&all, "bill")),
            [
                "nova-billing@main",
                "nova-billing@main/web",
                "nova-billing@feature-x",
                "nova-billing@main/worker",
            ]
        );
        assert_eq!(names(matches(&all, "BWork")), ["nova-billing@main/worker"]);
        assert!(matches(&all, "zzz").is_empty());
        assert_eq!(matches(&all, "").len(), all.len());
    }
}
