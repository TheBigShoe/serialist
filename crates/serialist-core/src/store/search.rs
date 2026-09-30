//! Regex search over line text.
//!
//! Most lines are plain, and their text is their raw bytes minus the line ending, so a
//! run of consecutive plain lines inside one page is searched as a single haystack with
//! one regex call, which lets the regex engine's literal prefilters run at memory speed.
//! A hit there only nominates a line: that line's text is then searched on its own, which
//! is also what every other line (decoded, local, spanning two pages, in progress) gets.
//!
//! This is exact. The bulk regex is the same pattern in multi-line CRLF mode, so `^`,
//! `$`, `\b` and `.` see a line ending exactly where the line's own text would end, and
//! any match inside one line's text is also a match at the same offset in the bulk
//! haystack. The bulk pass therefore never misses a line; a bulk hit that is not a real
//! match (one straddling two lines, say) is discarded by the per-line pass. Patterns
//! that anchor to the whole haystack (`\A`, `\z`) or turn off multi-line or CRLF mode
//! inline skip the bulk pass.

use std::sync::atomic::{AtomicBool, Ordering};

use regex::bytes::{Regex, RegexBuilder};

use super::index::LineFlags;
use super::snapshot::Snapshot;
use super::{B, P};
use crate::text::{LineId, SearchMatch, Searcher};

/// Smart case: a pattern with no uppercase letters matches case-insensitively. Letters
/// inside escapes (`\S`, `\W`, `\p{Lu}`, `\x4A`) and group names do not count, so
/// `\Sfoo` is still case-insensitive while `Foo` is case-sensitive. An inline `(?i)` or
/// `(?-i)` in the pattern wins either way.
pub fn smart_case_insensitive(pattern: &str) -> bool {
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('p' | 'P' | 'x' | 'u' | 'U') if chars.peek() == Some(&'{') => {
                    for c in chars.by_ref() {
                        if c == '}' {
                            break;
                        }
                    }
                }
                Some('p' | 'P') => {
                    chars.next();
                }
                Some(esc @ ('x' | 'u' | 'U')) => {
                    let digits = match esc {
                        'x' => 2,
                        'u' => 4,
                        _ => 8,
                    };
                    for _ in 0..digits {
                        if chars.peek().is_some_and(char::is_ascii_hexdigit) {
                            chars.next();
                        }
                    }
                }
                _ => {}
            },
            '(' if chars.peek() == Some(&'?') => {
                chars.next();
                // Skip a group name: (?P<name>...) or (?<name>...).
                if chars.peek() == Some(&'P') {
                    chars.next();
                }
                if chars.peek() == Some(&'<') {
                    for c in chars.by_ref() {
                        if c == '>' {
                            break;
                        }
                    }
                }
            }
            c if c.is_uppercase() => return false,
            _ => {}
        }
    }
    true
}

pub(crate) struct Query {
    line: Regex,
    bulk: Option<Regex>,
}

impl Query {
    pub fn new(pattern: &str) -> Result<Self, String> {
        let insensitive = smart_case_insensitive(pattern);
        let line = RegexBuilder::new(pattern)
            .case_insensitive(insensitive)
            .build()
            .map_err(|e| e.to_string())?;
        let bulk_safe = !["\\A", "\\z", "-m", "-R"]
            .iter()
            .any(|needle| pattern.contains(needle));
        let bulk = if bulk_safe {
            RegexBuilder::new(pattern)
                .case_insensitive(insensitive)
                .multi_line(true)
                .crlf(true)
                .build()
                .ok()
        } else {
            None
        };
        Ok(Self { line, bulk })
    }

    fn match_line(
        &self,
        text: &[u8],
        id: u64,
        backward: bool,
        limit: usize,
        out: &mut Vec<SearchMatch>,
    ) {
        if backward {
            let mut found: Vec<_> = self.line.find_iter(text).map(|m| m.range()).collect();
            while out.len() < limit
                && let Some(range) = found.pop()
            {
                out.push(SearchMatch {
                    line: LineId(id),
                    range,
                });
            }
        } else {
            for m in self.line.find_iter(text) {
                if out.len() >= limit {
                    break;
                }
                out.push(SearchMatch {
                    line: LineId(id),
                    range: m.range(),
                });
            }
        }
    }
}

/// A run of consecutive complete plain received lines inside one page.
struct Segment {
    first: u64,
    /// Start offset and flag byte of each line.
    lines: Vec<(u64, u8)>,
    raw_end: u64,
}

impl Segment {
    fn end_id(&self) -> u64 {
        self.first + self.lines.len() as u64
    }

    fn line_end(&self, i: usize) -> u64 {
        self.lines.get(i + 1).map_or(self.raw_end, |l| l.0)
    }
}

impl Snapshot {
    fn eligible(flags: LineFlags) -> bool {
        flags.is_plain_rx() && flags.complete()
    }

    /// The longest segment starting at committed line `id` and ending before `until`,
    /// within the page where `id` starts.
    fn segment_from(&self, id: u64, until: u64, seg: &mut Segment) -> bool {
        let p = &*self.p;
        seg.first = id;
        seg.lines.clear();
        let until = until.min(p.committed_end);
        if id >= until {
            return false;
        }
        let page_end = (p.start_of(id) / P + 1) * P;
        let mut next = id;
        'blocks: while next < until {
            let (block, local0) = p.block(next);
            let starts = block.starts.as_slice();
            let flags = block.flags.as_slice();
            let block_until = (block.first + B).min(until);
            for local in local0..(block_until - block.first) as usize {
                let f = LineFlags(flags[local]);
                if !Self::eligible(f) {
                    break 'blocks;
                }
                let start = block.base_raw + u64::from(starts[local]);
                let line_id = block.first + local as u64;
                let end = if local + 1 < starts.len() && line_id + 1 < p.committed_end {
                    block.base_raw + u64::from(starts[local + 1])
                } else {
                    p.raw_end_of(line_id)
                };
                if end > page_end {
                    break 'blocks;
                }
                seg.lines.push((start, f.0));
                seg.raw_end = end;
            }
            next = block_until;
        }
        !seg.lines.is_empty()
    }

    /// The first line of the segment that ends at committed line `id` (inclusive).
    fn segment_start_back(&self, id: u64) -> Option<u64> {
        let p = &*self.p;
        if id >= p.committed_end {
            return None;
        }
        let page_start = (p.start_of(id) / P) * P;
        let mut first = None;
        let mut l = id;
        loop {
            let (block, local) = p.block(l);
            if !Self::eligible(block.flags(local))
                || block.start(local) < page_start
                || p.raw_end_of(l) > page_start + P
            {
                break;
            }
            first = Some(l);
            if l == p.first_line {
                break;
            }
            l -= 1;
        }
        first
    }

    /// Search a segment's haystack; push every match, in forward order, into `out`.
    fn scan_segment(
        &self,
        q: &Query,
        bulk: &regex::bytes::Regex,
        seg: &Segment,
        limit: usize,
        out: &mut Vec<SearchMatch>,
    ) {
        let base = seg.lines[0].0;
        let hay = self.p.page_slice(base..seg.raw_end);
        let mut pos = 0;
        while pos <= hay.len() && out.len() < limit {
            let Some(m) = bulk.find_at(hay, pos) else {
                break;
            };
            let abs = base + m.start() as u64;
            let i = seg.lines.partition_point(|l| l.0 <= abs).saturating_sub(1);
            let (start, flags) = seg.lines[i];
            let flags = LineFlags(flags);
            let text_start = (start + flags.lead() - base) as usize;
            let text_end = (seg.line_end(i) - flags.trail() - base) as usize;
            q.match_line(
                &hay[text_start..text_end],
                seg.first + i as u64,
                false,
                limit,
                out,
            );
            if i + 1 == seg.lines.len() {
                break;
            }
            pos = (seg.lines[i + 1].0 - base) as usize;
        }
    }

    fn search_forward(
        &self,
        q: &Query,
        from: u64,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Vec<SearchMatch> {
        let p = &*self.p;
        let mut out = Vec::new();
        let mut scratch = Vec::new();
        let mut seg = Segment {
            first: 0,
            lines: Vec::new(),
            raw_end: 0,
        };
        let mut id = from.max(p.first_line);
        let mut steps = 0u32;
        while id < p.end_line && out.len() < limit {
            steps = steps.wrapping_add(1);
            if steps % 64 == 1 && cancel.load(Ordering::Relaxed) {
                break;
            }
            if let Some(bulk) = &q.bulk
                && self.segment_from(id, p.end_line, &mut seg)
            {
                self.scan_segment(q, bulk, &seg, limit, &mut out);
                id = seg.end_id();
                continue;
            }
            if let Some(text) = self.line_text(id, &mut scratch) {
                q.match_line(text, id, false, limit, &mut out);
            }
            id += 1;
        }
        out
    }

    fn search_backward(
        &self,
        q: &Query,
        from: u64,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Vec<SearchMatch> {
        let p = &*self.p;
        let mut out = Vec::new();
        if p.end_line == p.first_line || from < p.first_line {
            return out;
        }
        let mut scratch = Vec::new();
        let mut found = Vec::new();
        let mut seg = Segment {
            first: 0,
            lines: Vec::new(),
            raw_end: 0,
        };
        let mut id = from.min(p.end_line - 1);
        let mut steps = 0u32;
        loop {
            steps = steps.wrapping_add(1);
            if out.len() >= limit || (steps % 64 == 1 && cancel.load(Ordering::Relaxed)) {
                break;
            }
            let mut first = id;
            let bulk_segment = q
                .bulk
                .as_ref()
                .zip(self.segment_start_back(id))
                .filter(|(_, start)| self.segment_from(*start, id + 1, &mut seg));
            if let Some((bulk, start)) = bulk_segment {
                found.clear();
                self.scan_segment(q, bulk, &seg, usize::MAX, &mut found);
                while out.len() < limit
                    && let Some(m) = found.pop()
                {
                    out.push(m);
                }
                first = start;
            } else if let Some(text) = self.line_text(id, &mut scratch) {
                q.match_line(text, id, true, limit, &mut out);
            }
            if first <= p.first_line {
                break;
            }
            id = first - 1;
        }
        out
    }
}

impl Searcher for Snapshot {
    /// Regex search in bytes mode over line text, smart case (see
    /// [`smart_case_insensitive`]). Forward searches lines `from..end` in order, backward
    /// searches `first_line..=from` newest first; both include `from`, clamped to the
    /// retained lines. Within a line, matches come in the search direction. Stops after
    /// `limit` matches or when `cancel` is set, returning what it found so far.
    fn search(
        &self,
        pattern: &str,
        from: LineId,
        backward: bool,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<SearchMatch>, String> {
        let query = Query::new(pattern)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        Ok(if backward {
            self.search_backward(&query, from.0, limit, cancel)
        } else {
            self.search_forward(&query, from.0, limit, cancel)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_case_rules() {
        assert!(smart_case_insensitive("error"));
        assert!(!smart_case_insensitive("Error"));
        assert!(smart_case_insensitive(r"\Serr\W\d\B"));
        assert!(smart_case_insensitive(r"\p{Lu}x\pL"));
        assert!(smart_case_insensitive(r"\x4A\u{1F600}"));
        assert!(smart_case_insensitive(r"(?P<Name>x)(?<Other>y)"));
        assert!(!smart_case_insensitive(r"\d+ OK"));
        assert!(!smart_case_insensitive("É"));
    }
}
