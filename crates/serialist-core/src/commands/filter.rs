//! Fuzzy filtering for the Commands panel: a small subsequence scorer with no
//! dependency behind it.
//!
//! A query is split at whitespace, and every word has to match something in the command
//! (its name, group, collection or description) or the command is out. A word matches a
//! text when its letters appear in order, ignoring case. Runs of letters that sit
//! together, start a word or start the text score higher, and a word that appears whole
//! beats one spread out, so `ver` ranks `Version` above `Save all events, reset`.

use super::model::{Command, CommandCollection, CommandGroup, CommandRef};
use super::store::CommandStore;

/// How well a query matched one text, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuzzyMatch {
    /// Higher is better. Only comparable between matches of the same query.
    pub score: i32,
    /// The matched characters, as indices into the text's `chars()`, for highlighting.
    pub positions: Vec<usize>,
}

const CHAR: i32 = 10;
const AT_START: i32 = 30;
const AT_WORD: i32 = 20;
const ADJACENT: i32 = 15;
const GAP: i32 = 3;
const MAX_GAP: usize = 5;
const PREFIX: i32 = 25;
const EXACT: i32 = 50;

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn starts_word(text: &[char], at: usize) -> bool {
    let Some(before) = at.checked_sub(1).map(|i| text[i]) else {
        return true;
    };
    let cur = text[at];
    !before.is_alphanumeric()
        || (before.is_lowercase() && cur.is_uppercase())
        || (before.is_alphabetic() != cur.is_alphabetic())
}

/// Scores matched `positions` (strictly increasing) in `text`.
fn score(text: &[char], positions: &[usize]) -> i32 {
    let mut total = 0;
    for (k, &at) in positions.iter().enumerate() {
        total += CHAR;
        if at == 0 {
            total += AT_START;
        } else if starts_word(text, at) {
            total += AT_WORD;
        }
        if k > 0 {
            let gap = at - positions[k - 1] - 1;
            if gap == 0 {
                total += ADJACENT;
            } else {
                total -= GAP * gap.min(MAX_GAP) as i32;
            }
        }
    }
    let first = positions[0];
    total -= first.min(10) as i32;
    // A shorter text is a closer match for the same letters.
    total -= (text.len() - positions.len()).min(20) as i32 / 4;
    let contiguous = positions.windows(2).all(|w| w[1] == w[0] + 1);
    if contiguous && first == 0 {
        total += if positions.len() == text.len() {
            EXACT
        } else {
            PREFIX
        };
    }
    total
}

/// Matches `query` against `text`: the query's characters must appear in `text` in
/// order, ignoring case. `None` when they do not. An empty query matches everything with
/// a score of 0.
///
/// A contiguous occurrence is preferred (the one that starts a word, if there is one);
/// otherwise the tightest span that holds the letters in order is scored.
pub fn fuzzy_match(query: &str, text: &str) -> Option<FuzzyMatch> {
    let query: Vec<char> = query.chars().map(fold).collect();
    if query.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }
    let chars: Vec<char> = text.chars().collect();
    let folded: Vec<char> = chars.iter().copied().map(fold).collect();
    if query.len() > folded.len() {
        return None;
    }

    // A contiguous occurrence: the best-scoring start.
    let best_whole = (0..=folded.len() - query.len())
        .filter(|&start| folded[start..start + query.len()] == query[..])
        .map(|start| {
            let positions: Vec<usize> = (start..start + query.len()).collect();
            (score(&chars, &positions), positions)
        })
        .max_by_key(|(score, _)| *score);
    if let Some((score, positions)) = best_whole {
        return Some(FuzzyMatch { score, positions });
    }

    // Forward to the earliest place the letters end, then backward to the latest place
    // they can start from there: the tightest span ending at that first end.
    let mut at = 0;
    let mut end = None;
    for (i, &c) in folded.iter().enumerate() {
        if c == query[at] {
            at += 1;
            if at == query.len() {
                end = Some(i);
                break;
            }
        }
    }
    let end = end?;
    let mut positions = vec![0; query.len()];
    let mut want = query.len();
    for i in (0..=end).rev() {
        if folded[i] == query[want - 1] {
            want -= 1;
            positions[want] = i;
            if want == 0 {
                break;
            }
        }
    }
    Some(FuzzyMatch {
        score: score(&chars, &positions),
        positions,
    })
}

/// The command's score for `words`, or `None` if a word matches nothing about it.
fn score_command(
    words: &[&str],
    collection: &CommandCollection,
    group: &CommandGroup,
    command: &Command,
) -> Option<i32> {
    // The name counts in full; where a word matched counts for less the further it is
    // from what the user reads first.
    let fields: [(&str, i32); 4] = [
        (&command.name, 10),
        (&group.name, 5),
        (&collection.name, 5),
        (&command.description, 4),
    ];
    let mut total = 0;
    for word in words {
        let best = fields
            .iter()
            .filter(|(text, _)| !text.is_empty())
            .filter_map(|(text, weight)| Some(fuzzy_match(word, text)?.score * weight / 10))
            .max()?;
        total += best;
    }
    Some(total)
}

impl CommandStore {
    /// The commands that match `query`, best first, then in the store's order. An empty
    /// query matches every command with a score of 0, in the store's order.
    pub fn filter(&self, query: &str) -> Vec<(CommandRef, i32)> {
        let words: Vec<&str> = query.split_whitespace().collect();
        let mut hits = Vec::new();
        for collection in self.collections() {
            for (group, command) in collection.commands() {
                let score = if words.is_empty() {
                    Some(0)
                } else {
                    score_command(&words, collection, group, command)
                };
                if let Some(score) = score {
                    hits.push((collection.command_ref(group, command), score));
                }
            }
        }
        // Stable, so equal scores keep the store's order.
        hits.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
        hits
    }
}
