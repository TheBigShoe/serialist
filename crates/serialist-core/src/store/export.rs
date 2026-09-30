//! Text export from any line source: a snapshot's lines, a hex view's rows, a test
//! double. [`write_lines`] is the one implementation, and the one place the timestamp
//! formats live; [`Snapshot::write_text`] and its counting twin are thin wrappers.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::ops::Range;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::snapshot::Snapshot;
use crate::text::{Direction, Epoch, LineId, LineSource};

/// How each exported line is stamped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Timestamps {
    #[default]
    None,
    /// Wall-clock arrival time in UTC: `[2026-09-29T21:53:44.123456Z] `.
    Absolute,
    /// Seconds since the store's epoch (the session start): `[+12.345678] `.
    Relative,
    /// Seconds since the previous exported line (the first gets zero): `[+0.000123] `.
    Delta,
}

/// Options for [`Snapshot::text`] and [`write_lines`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TextOptions {
    pub timestamps: Timestamps,
    /// Include `Tx` lines (local echoes of what was sent).
    pub include_tx: bool,
    /// Include `Notice` lines (connect, disconnect and other app messages).
    pub include_notices: bool,
}

impl Default for TextOptions {
    /// Everything the view shows, without timestamps.
    fn default() -> Self {
        Self {
            timestamps: Timestamps::None,
            include_tx: true,
            include_notices: true,
        }
    }
}

impl TextOptions {
    /// Received lines only.
    pub fn received_only() -> Self {
        Self {
            timestamps: Timestamps::None,
            include_tx: false,
            include_notices: false,
        }
    }

    pub fn with_timestamps(mut self, timestamps: Timestamps) -> Self {
        self.timestamps = timestamps;
        self
    }
}

/// What an export wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextExportReport {
    /// Lines written: those in the range that the options kept.
    pub lines: usize,
    /// Bytes written, timestamps and line ends included.
    pub bytes: u64,
}

/// Slab of lines fetched from a source at a time, so a source with a faster bulk
/// `lines` is used, and memory stays flat however large the export.
const SLAB: usize = 4096;

/// The lines of `source` in `range` (clipped to what it retains) as text, one per line,
/// each ending in `\n`, stamped and filtered as `options` say, streamed to `out`. Wrap
/// files in a `BufWriter`. Works for anything that is a [`LineSource`]: a hex view's
/// rows get the same stamps as a snapshot's lines, and are all kept (they are `Rx`).
///
/// A stamp is `[2026-09-29T21:53:44.123456Z] ` (absolute UTC), `[+12.345678] ` (seconds
/// since the source's epoch) or `[+0.000123] ` (seconds since the previous line
/// written; the first gets zero). Skipped lines do not move the delta.
pub fn write_lines<W: Write + ?Sized>(
    source: &dyn LineSource,
    range: Range<LineId>,
    options: TextOptions,
    out: &mut W,
) -> io::Result<TextExportReport> {
    let epoch = source.epoch();
    let end = range.end.min(source.end());
    let mut id = range.start.max(source.first_line());
    let mut report = TextExportReport::default();
    let mut previous = None;
    let mut lines = Vec::new();
    let mut stamp = String::new();
    while id < end {
        let slab_end = id.offset(SLAB).min(end);
        lines.clear();
        source.lines(id..slab_end, &mut lines);
        id = slab_end;
        for line in &lines {
            let keep = match line.direction {
                Direction::Rx => true,
                Direction::Tx => options.include_tx,
                Direction::Notice => options.include_notices,
            };
            if !keep {
                continue;
            }
            stamp.clear();
            write_stamp(
                &mut stamp,
                options.timestamps,
                &epoch,
                previous,
                line.received_at,
            );
            previous = Some(line.received_at);
            out.write_all(stamp.as_bytes())?;
            out.write_all(line.text.as_bytes())?;
            out.write_all(b"\n")?;
            report.lines += 1;
            report.bytes += (stamp.len() + line.text.len() + 1) as u64;
        }
    }
    Ok(report)
}

/// The stamp for a line that arrived at `at`, given the previous line written.
fn write_stamp(
    out: &mut String,
    timestamps: Timestamps,
    epoch: &Epoch,
    previous: Option<Instant>,
    at: Instant,
) {
    // Formatting into a String cannot fail.
    let _ = match timestamps {
        Timestamps::None => Ok(()),
        Timestamps::Absolute => write!(out, "[{}] ", format_utc(epoch.wall_time(at))),
        Timestamps::Relative => {
            let since = at.saturating_duration_since(epoch.instant);
            write!(out, "[+{}] ", format_secs(since))
        }
        Timestamps::Delta => {
            let since = previous.map_or(Duration::ZERO, |p| at.saturating_duration_since(p));
            write!(out, "[+{}] ", format_secs(since))
        }
    };
}

impl Snapshot {
    /// The lines in `range` (clipped to what is retained) as text, one per line, each
    /// ending in `\n`. The styled text, not the raw bytes: escapes are gone and CR
    /// overwrites are applied.
    pub fn text(&self, range: Range<LineId>, options: TextOptions) -> String {
        let mut out = Vec::new();
        self.write_text(range, options, &mut out)
            .expect("writing to a Vec cannot fail");
        String::from_utf8(out).expect("line text is UTF-8")
    }

    /// [`Snapshot::text`] streamed to `out`, for exports too large to build in memory.
    /// Wrap files in a `BufWriter`. See [`Snapshot::write_text_counted`] for the counts.
    pub fn write_text<W: Write>(
        &self,
        range: Range<LineId>,
        options: TextOptions,
        out: &mut W,
    ) -> io::Result<()> {
        self.write_text_counted(range, options, out).map(drop)
    }

    /// [`Snapshot::write_text`] that reports how many lines and bytes it wrote, for a
    /// status message ("Exported 1204 lines"). It is [`write_lines`] over this snapshot.
    pub fn write_text_counted<W: Write>(
        &self,
        range: Range<LineId>,
        options: TextOptions,
        out: &mut W,
    ) -> io::Result<TextExportReport> {
        write_lines(self, range, options, out)
    }
}

fn format_secs(d: Duration) -> String {
    format!("{}.{:06}", d.as_secs(), d.subsec_micros())
}

/// `2026-09-29T21:53:44.123456Z`: ISO 8601 in UTC with microseconds.
pub fn format_utc(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (y, m, day) = civil_from_days((secs / 86_400) as i64);
    let sod = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:06}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60,
        d.subsec_micros()
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(UNIX_EPOCH), "1970-01-01T00:00:00.000000Z");
        let t = UNIX_EPOCH + Duration::new(1_790_718_824, 123_456_789);
        assert_eq!(format_utc(t), "2026-09-29T21:53:44.123456Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(format_utc(leap), "2000-02-29T00:00:00.000000Z");
        assert_eq!(format_secs(Duration::new(12, 345_678_000)), "12.345678");
    }
}
