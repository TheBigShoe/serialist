//! Regex search over line text.
//!
//! Most lines are plain, and their text is their raw bytes minus the line ending, so a
//! run of consecutive plain lines inside one page is searched as a single haystack with
//! one regex call, which lets the regex engine's literal prefilters run at memory speed.
//! Decoded lines get the same treatment: their texts sit in text pages between `\n`
//! bytes, so a run of them in one text page is a haystack too. A hit only nominates a
//! line: that line's text is then searched on its own, which is also what the remaining
//! lines (spanning two raw pages, the line in progress) get. Backward search scans
//! windows of lines forward, newest window first, and reports each window in reverse.
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

use super::index::{self, LineFlags};
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

    /// Push the matches of `text` (line `id`), in order, until `out` holds `limit`.
    fn match_line(&self, text: &[u8], id: u64, limit: usize, out: &mut Vec<SearchMatch>) {
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

/// Lines searched as one haystack: consecutive complete plain received lines inside
/// one raw page, or consecutive decoded lines inside one text page.
struct Segment {
    first: u64,
    /// Where each line's bytes begin: its raw start (for raw segments) or its text's
    /// offset in the text page. With a raw line's flag byte.
    lines: Vec<(u64, u8)>,
    /// End of the haystack: the last line's raw end, or its text's end in the page.
    end: u64,
    /// `None` for raw segments, else the index of the text page in the snapshot.
    text_page: Option<usize>,
}

impl Segment {
    fn new() -> Self {
        Self {
            first: 0,
            lines: Vec::new(),
            end: 0,
            text_page: None,
        }
    }

    fn end_id(&self) -> u64 {
        self.first + self.lines.len() as u64
    }
}

/// Lines per window when searching backward: each window is scanned forward and its
/// matches reported in reverse.
const BACKWARD_WINDOW: u64 = 4096;

impl Snapshot {
    /// The longest raw segment starting at committed line `id` and ending before
    /// `until`, within the page where `id` starts.
    fn raw_segment(&self, id: u64, until: u64, seg: &mut Segment) -> bool {
        let p = &*self.p;
        seg.first = id;
        seg.lines.clear();
        seg.text_page = None;
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
                if !(f.is_plain_rx() && f.complete()) {
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
                seg.end = end;
            }
            next = block_until;
        }
        !seg.lines.is_empty()
    }

    /// The longest text segment starting at committed line `id` and ending before
    /// `until`: decoded lines whose records sit in one text page.
    fn text_segment(&self, id: u64, until: u64, seg: &mut Segment) -> bool {
        let p = &*self.p;
        seg.first = id;
        seg.lines.clear();
        let until = until.min(p.committed_end);
        let mut page = None;
        let mut next = id;
        'blocks: while next < until {
            let (block, local0) = p.block(next);
            // Only decoded lines have a `DecRef`, so consecutive refs for consecutive
            // lines mean consecutive decoded lines.
            let decs = block.decs.as_slice();
            let Ok(di) = decs.binary_search_by_key(&(local0 as u32), |d| d.local) else {
                break;
            };
            let block_until = (block.first + B).min(until);
            let want = (block_until - block.first) as usize - local0;
            let mut taken = 0;
            for (local, dec) in (local0..).zip(&decs[di..]).take(want) {
                if dec.local as usize != local || *page.get_or_insert(dec.page) != dec.page {
                    break;
                }
                seg.lines.push((u64::from(dec.off), 0));
                taken += 1;
            }
            if taken < want {
                break 'blocks;
            }
            next = block_until;
        }
        let Some(page) = page.filter(|_| !seg.lines.is_empty()) else {
            return false;
        };
        let index = page.wrapping_sub(p.first_text_seq as u32) as usize;
        let Some(bytes) = p.text_pages.get(index).map(|b| b.as_slice()) else {
            return false;
        };
        let last = seg.lines.last().expect("not empty").0 as usize;
        seg.end = (last + index::record_text(&bytes[last..]).len()) as u64;
        seg.text_page = Some(index);
        true
    }

    /// Search a segment's haystack once, confirm each hit on its line alone, and push
    /// every match in forward order into `out`.
    fn scan_segment(
        &self,
        q: &Query,
        bulk: &Regex,
        seg: &Segment,
        limit: usize,
        out: &mut Vec<SearchMatch>,
    ) {
        let p = &*self.p;
        let base = seg.lines[0].0;
        let hay = match seg.text_page {
            None => p.page_slice(base..seg.end),
            Some(index) => &p.text_pages[index].as_slice()[base as usize..seg.end as usize],
        };
        let mut pos = 0;
        while pos <= hay.len() && out.len() < limit {
            let Some(m) = bulk.find_at(hay, pos) else {
                break;
            };
            let abs = base + m.start() as u64;
            let i = seg.lines.partition_point(|l| l.0 <= abs).saturating_sub(1);
            let (start, flags) = seg.lines[i];
            let from = (start - base) as usize;
            let text = match seg.text_page {
                None => {
                    let flags = LineFlags(flags);
                    let end = seg.lines.get(i + 1).map_or(seg.end, |l| l.0);
                    &hay[from + flags.lead() as usize..(end - flags.trail() - base) as usize]
                }
                Some(_) => index::record_text(&hay[from..]),
            };
            q.match_line(text, seg.first + i as u64, limit, out);
            match seg.lines.get(i + 1) {
                Some(next) => pos = (next.0 - base) as usize,
                None => break,
            }
        }
    }

    /// Matches in lines `from..until`, in order, until `out` holds `limit`.
    fn search_range(
        &self,
        q: &Query,
        from: u64,
        until: u64,
        limit: usize,
        cancel: &AtomicBool,
        out: &mut Vec<SearchMatch>,
    ) {
        let mut scratch = Vec::new();
        let mut seg = Segment::new();
        let mut id = from;
        let mut steps = 0u32;
        while id < until && out.len() < limit {
            steps = steps.wrapping_add(1);
            if steps % 64 == 1 && cancel.load(Ordering::Relaxed) {
                return;
            }
            if let Some(bulk) = &q.bulk
                && (self.raw_segment(id, until, &mut seg) || self.text_segment(id, until, &mut seg))
            {
                self.scan_segment(q, bulk, &seg, limit, out);
                id = seg.end_id();
                continue;
            }
            if let Some(text) = self.line_text(id, &mut scratch) {
                q.match_line(text, id, limit, out);
            }
            id += 1;
        }
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
        let mut window = Vec::new();
        let mut until = from.min(p.end_line - 1) + 1;
        while until > p.first_line && out.len() < limit && !cancel.load(Ordering::Relaxed) {
            let start = until.saturating_sub(BACKWARD_WINDOW).max(p.first_line);
            window.clear();
            self.search_range(q, start, until, usize::MAX, cancel, &mut window);
            // Newest line first, and within a line the last match first.
            while out.len() < limit
                && let Some(m) = window.pop()
            {
                out.push(m);
            }
            until = start;
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
        if backward {
            return Ok(self.search_backward(&query, from.0, limit, cancel));
        }
        let mut out = Vec::new();
        let start = from.0.max(self.p.first_line);
        self.search_range(&query, start, self.p.end_line, limit, cancel, &mut out);
        Ok(out)
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
