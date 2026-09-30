//! Text export from a snapshot.

use std::io::{self, Write};
use std::ops::Range;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::snapshot::Snapshot;
use crate::text::{Direction, LineId, LineSource};

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

/// Options for [`Snapshot::text`].
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
    /// Wrap files in a `BufWriter`.
    pub fn write_text<W: Write>(
        &self,
        range: Range<LineId>,
        options: TextOptions,
        out: &mut W,
    ) -> io::Result<()> {
        let start = range.start.max(self.first_line());
        let end = range.end.min(self.end());
        let epoch = self.epoch();
        let mut previous = None;
        let mut id = start;
        while id < end {
            let Some(line) = self.line(id) else {
                id = id.next();
                continue;
            };
            id = id.next();
            let keep = match line.direction {
                Direction::Rx => true,
                Direction::Tx => options.include_tx,
                Direction::Notice => options.include_notices,
            };
            if !keep {
                continue;
            }
            match options.timestamps {
                Timestamps::None => {}
                Timestamps::Absolute => {
                    write!(out, "[{}] ", format_utc(epoch.wall_time(line.received_at)))?;
                }
                Timestamps::Relative => {
                    let since = line.received_at.saturating_duration_since(epoch.instant);
                    write!(out, "[+{}] ", format_secs(since))?;
                }
                Timestamps::Delta => {
                    let since = previous.map_or(Duration::ZERO, |p| {
                        line.received_at.saturating_duration_since(p)
                    });
                    write!(out, "[+{}] ", format_secs(since))?;
                }
            }
            previous = Some(line.received_at);
            out.write_all(line.text.as_bytes())?;
            out.write_all(b"\n")?;
        }
        Ok(())
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
