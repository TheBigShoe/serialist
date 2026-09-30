//! Search results and navigation, without GPUI. The view runs the [`Searcher`] on the
//! background executor and hands the outcome here.
//!
//! [`Searcher`]: serialist_core::Searcher

use std::sync::Arc;

use serialist_core::{LineId, SearchMatch};

/// The most matches one search keeps. Searches run from the newest line backwards, so
/// a truncated result holds the newest matches.
pub const MAX_MATCHES: usize = 10_000;

#[derive(Clone, Debug, Default)]
pub struct SearchResults {
    pub query: String,
    /// Oldest first: by line, then by start.
    pub matches: Arc<Vec<SearchMatch>>,
    pub active: Option<usize>,
    /// The search stopped at [`MAX_MATCHES`]; older matches were not collected.
    pub truncated: bool,
    /// The searcher rejected the pattern.
    pub error: Option<String>,
    /// A search for `query` is running.
    pub pending: bool,
}

impl SearchResults {
    /// Take a finished search's matches, found newest first, and make the first match
    /// at or below `top` active, or the newest one if there is none below.
    pub fn finish(&mut self, mut newest_first: Vec<SearchMatch>, top: LineId) {
        newest_first.reverse();
        self.truncated = newest_first.len() >= MAX_MATCHES;
        self.active = if newest_first.is_empty() {
            None
        } else {
            let below = newest_first.partition_point(|m| m.line < top);
            Some(below.min(newest_first.len() - 1))
        };
        self.matches = Arc::new(newest_first);
        self.error = None;
        self.pending = false;
    }

    /// A search of the lines from `from` on (the new lines and the one that was still
    /// arriving) found `oldest_first`: they replace every match at or after `from`, and
    /// the oldest matches go if that takes the total past [`MAX_MATCHES`]. The active
    /// match stays on the same match while it exists.
    pub fn extend(&mut self, from: LineId, oldest_first: Vec<SearchMatch>) {
        let active = self.active_match().cloned();
        let kept = self.matches.partition_point(|m| m.line < from);
        let mut matches = Vec::with_capacity(kept + oldest_first.len());
        matches.extend_from_slice(&self.matches[..kept]);
        matches.extend(oldest_first);
        if matches.len() > MAX_MATCHES {
            matches.drain(..matches.len() - MAX_MATCHES);
            self.truncated = true;
        }
        self.matches = Arc::new(matches);
        self.reselect(active);
        self.error = None;
        self.pending = false;
    }

    /// A fresh search of everything displayed, newest first, found `newest_first`.
    /// Unlike [`Self::finish`] the active match stays on the same match while it exists.
    pub fn rescanned(&mut self, mut newest_first: Vec<SearchMatch>) {
        let active = self.active_match().cloned();
        newest_first.reverse();
        self.truncated = newest_first.len() >= MAX_MATCHES;
        self.matches = Arc::new(newest_first);
        self.reselect(active);
        self.error = None;
        self.pending = false;
    }

    /// Drop matches outside `lines`: evicted, cleared, or past a paused view's end.
    pub fn retain_lines(&mut self, lines: std::ops::Range<LineId>) {
        let start = self.matches.partition_point(|m| m.line < lines.start);
        let end = self.matches.partition_point(|m| m.line < lines.end);
        if start == 0 && end == self.matches.len() {
            return;
        }
        let active = self.active_match().cloned();
        self.matches = Arc::new(self.matches[start..end].to_vec());
        self.reselect(active);
    }

    /// Point `active` at `previous` if it is still a match, else at the nearest match
    /// after it, else the newest.
    fn reselect(&mut self, previous: Option<SearchMatch>) {
        if self.matches.is_empty() {
            self.active = None;
            return;
        }
        let newest = self.matches.len() - 1;
        self.active = Some(match previous {
            Some(previous) => {
                let key = |m: &SearchMatch| (m.line, m.range.start);
                let at = self.matches.partition_point(|m| key(m) < key(&previous));
                at.min(newest)
            }
            None => newest,
        });
    }

    pub fn fail(&mut self, error: String) {
        self.matches = Arc::default();
        self.active = None;
        self.truncated = false;
        self.error = Some(error);
        self.pending = false;
    }

    /// Forget the matches, keeping the query.
    pub fn clear(&mut self) {
        let query = std::mem::take(&mut self.query);
        *self = Self {
            query,
            ..Self::default()
        };
    }

    pub fn active_match(&self) -> Option<&SearchMatch> {
        self.matches.get(self.active?)
    }

    /// Step to the next match (toward newer lines), wrapping around.
    pub fn select_next(&mut self) -> Option<&SearchMatch> {
        let len = self.matches.len();
        if len == 0 {
            return None;
        }
        self.active = Some(self.active.map_or(0, |ix| (ix + 1) % len));
        self.active_match()
    }

    /// Step to the previous match (toward older lines), wrapping around.
    pub fn select_previous(&mut self) -> Option<&SearchMatch> {
        let len = self.matches.len();
        if len == 0 {
            return None;
        }
        self.active = Some(self.active.map_or(len - 1, |ix| (ix + len - 1) % len));
        self.active_match()
    }

    /// The `n/N` label next to the search field.
    pub fn count_label(&self) -> String {
        if self.query.is_empty() {
            return String::new();
        }
        if self.error.is_some() {
            return "Invalid pattern".into();
        }
        if self.pending && self.matches.is_empty() {
            return "Searching…".into();
        }
        let more = if self.truncated { "+" } else { "" };
        match self.active {
            Some(ix) => format!("{}/{}{more}", ix + 1, self.matches.len()),
            None => "0/0".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: u64, start: usize) -> SearchMatch {
        SearchMatch {
            line: LineId(line),
            range: start..start + 1,
        }
    }

    #[test]
    fn finishing_orders_matches_and_picks_the_first_below_the_top() {
        let mut results = SearchResults {
            query: "x".into(),
            pending: true,
            ..SearchResults::default()
        };
        results.finish(vec![at(9, 0), at(5, 2), at(5, 0), at(1, 0)], LineId(4));
        let lines: Vec<_> = results
            .matches
            .iter()
            .map(|m| (m.line.0, m.range.start))
            .collect();
        assert_eq!(lines, [(1, 0), (5, 0), (5, 2), (9, 0)]);
        assert_eq!(results.active, Some(1));
        assert_eq!(results.count_label(), "2/4");
        assert!(!results.pending);

        results.finish(vec![at(3, 0), at(1, 0)], LineId(50));
        assert_eq!(results.active, Some(1), "nothing below the top: the newest");
    }

    #[test]
    fn next_and_previous_wrap_around() {
        let mut results = SearchResults {
            query: "x".into(),
            ..SearchResults::default()
        };
        assert!(results.select_next().is_none());
        results.finish(vec![at(3, 0), at(2, 0), at(1, 0)], LineId(0));
        assert_eq!(results.active, Some(0));
        assert_eq!(results.select_next().unwrap().line, LineId(2));
        assert_eq!(results.select_next().unwrap().line, LineId(3));
        assert_eq!(results.select_next().unwrap().line, LineId(1));
        assert_eq!(results.select_previous().unwrap().line, LineId(3));
        assert_eq!(results.count_label(), "3/3");
    }

    fn lines_of(results: &SearchResults) -> Vec<u64> {
        results.matches.iter().map(|m| m.line.0).collect()
    }

    #[test]
    fn extending_replaces_from_the_changed_line_and_keeps_the_active_match() {
        let mut results = SearchResults {
            query: "x".into(),
            ..SearchResults::default()
        };
        results.finish(vec![at(9, 0), at(5, 0), at(1, 0)], LineId(4));
        assert_eq!(results.active_match().unwrap().line, LineId(5));

        // Line 9 was still arriving; the new search covers it again and finds more.
        results.extend(LineId(9), vec![at(9, 0), at(9, 4), at(12, 0)]);
        assert_eq!(lines_of(&results), [1, 5, 9, 9, 12]);
        assert_eq!(results.active_match().unwrap().line, LineId(5));
        assert_eq!(results.count_label(), "2/5");

        // Eviction takes lines below 6, the active match with them: the next one is active.
        results.retain_lines(LineId(6)..LineId(100));
        assert_eq!(lines_of(&results), [9, 9, 12]);
        assert_eq!(results.active, Some(0));

        // A pause cuts the display at line 10.
        results.retain_lines(LineId(6)..LineId(10));
        assert_eq!(lines_of(&results), [9, 9]);

        // With nothing active before, the newest match becomes active.
        let mut empty = SearchResults::default();
        empty.finish(Vec::new(), LineId(0));
        empty.extend(LineId(0), vec![at(3, 0), at(4, 0)]);
        assert_eq!(empty.active, Some(1));
    }

    #[test]
    fn extending_past_the_cap_drops_the_oldest() {
        let mut results = SearchResults::default();
        results.finish(
            (0..MAX_MATCHES as u64 - 1)
                .rev()
                .map(|l| at(l, 0))
                .collect(),
            LineId(0),
        );
        assert!(!results.truncated);
        results.extend(
            LineId(MAX_MATCHES as u64),
            vec![at(20_000, 0), at(20_001, 0)],
        );
        assert_eq!(results.matches.len(), MAX_MATCHES);
        assert!(results.truncated);
        assert_eq!(results.matches[0].line, LineId(1));
        assert_eq!(results.matches.last().unwrap().line, LineId(20_001));
    }

    #[test]
    fn a_rescan_keeps_the_active_match() {
        let mut results = SearchResults::default();
        results.finish(vec![at(9, 0), at(5, 0), at(1, 0)], LineId(4));
        results.rescanned(vec![at(11, 0), at(9, 0), at(5, 0)]);
        assert_eq!(lines_of(&results), [5, 9, 11]);
        assert_eq!(results.active, Some(0), "still line 5");
    }

    #[test]
    fn labels_for_every_state() {
        let mut results = SearchResults::default();
        assert_eq!(results.count_label(), "");
        results.query = "err".into();
        results.pending = true;
        assert_eq!(results.count_label(), "Searching…");
        results.finish(Vec::new(), LineId(0));
        assert_eq!(results.count_label(), "0/0");
        results.fail("unclosed group".into());
        assert_eq!(results.count_label(), "Invalid pattern");
        results.finish(vec![at(1, 0); MAX_MATCHES], LineId(0));
        assert!(results.truncated);
        assert_eq!(results.count_label(), format!("1/{MAX_MATCHES}+"));
        results.clear();
        assert_eq!(results.query, "err");
        assert!(results.matches.is_empty());
    }
}
