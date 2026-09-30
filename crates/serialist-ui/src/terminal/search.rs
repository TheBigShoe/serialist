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
