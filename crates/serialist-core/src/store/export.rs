//! Text export from any line source: a snapshot's lines, a hex view's rows, a test
//! double. [`write_lines`] is the one implementation, and [`format_timestamp`] the one
//! place the timestamp formats live, for the export and for the terminal's gutter;
//! [`Snapshot::write_text`] and its counting twin are thin wrappers.

use std::fmt::{self, Write as _};
use std::io::{self, Write};
use std::ops::Range;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, Local, TimeZone, Utc};
use parking_lot::Mutex;

use super::snapshot::Snapshot;
use crate::text::{Direction, Epoch, LineId, LineSource};

/// The `strftime` format of an absolute stamp when [`TextOptions::timestamp_format`] is
/// `None` or is not a valid format: hours, minutes, seconds and milliseconds.
pub const DEFAULT_TIMESTAMP_FORMAT: &str = "%H:%M:%S%.3f";

/// How each exported line is stamped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Timestamps {
    #[default]
    None,
    /// Wall-clock arrival time in the machine's local time zone, formatted with
    /// [`TextOptions::timestamp_format`] (default [`DEFAULT_TIMESTAMP_FORMAT`]):
    /// `[21:53:44.123] `.
    Absolute,
    /// Wall-clock arrival time in UTC, always as ISO 8601 with microseconds whatever the
    /// format: `[2026-09-29T21:53:44.123456Z] `. What `Absolute` was before it went local.
    AbsoluteUtc,
    /// Seconds since the store's epoch (the session start): `[+12.345678] `.
    Relative,
    /// Seconds since the previous exported line (the first gets zero): `[+0.000123] `.
    Delta,
}

/// Options for [`Snapshot::text`] and [`write_lines`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TextOptions {
    pub timestamps: Timestamps,
    /// The `strftime` format of [`Timestamps::Absolute`] stamps, with chrono's fractional
    /// seconds (`%.3f` for `.123`, `%3f` for `123`). `None`, or a string chrono cannot
    /// parse, means [`DEFAULT_TIMESTAMP_FORMAT`]; the second case is logged once.
    pub timestamp_format: Option<String>,
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
            timestamp_format: None,
            include_tx: true,
            include_notices: true,
        }
    }
}

impl TextOptions {
    /// Received lines only.
    pub fn received_only() -> Self {
        Self {
            include_tx: false,
            include_notices: false,
            ..Self::default()
        }
    }

    pub fn with_timestamps(mut self, timestamps: Timestamps) -> Self {
        self.timestamps = timestamps;
        self
    }

    /// Stamp [`Timestamps::Absolute`] lines with this `strftime` format.
    pub fn with_timestamp_format(mut self, format: impl Into<String>) -> Self {
        self.timestamp_format = Some(format.into());
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
/// A stamp is [`format_timestamp`]'s text in brackets and a space: `[21:53:44.123] `
/// (local time, in `options.timestamp_format`), `[2026-09-29T21:53:44.123456Z] `
/// (UTC), `[+12.345678] ` (seconds since the source's epoch) or `[+0.000123] ` (seconds
/// since the previous line written; the first gets zero). Skipped lines do not move the
/// delta.
pub fn write_lines<W: Write + ?Sized>(
    source: &dyn LineSource,
    range: Range<LineId>,
    options: TextOptions,
    out: &mut W,
) -> io::Result<TextExportReport> {
    let epoch = source.epoch();
    let end = range.end.min(source.end());
    let mut id = range.start.max(source.first_line());
    let format = match options.timestamps {
        Timestamps::Absolute => usable_format(options.timestamp_format.as_deref()),
        _ => DEFAULT_TIMESTAMP_FORMAT,
    };
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
            if options.timestamps != Timestamps::None {
                stamp.push('[');
                write_stamp(
                    &mut stamp,
                    &Local,
                    options.timestamps,
                    format,
                    &epoch,
                    line.received_at,
                    previous,
                );
                stamp.push_str("] ");
            }
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

/// The stamp of a line that arrived at `at`, without the brackets an export puts round
/// it; empty for [`Timestamps::None`]. `previous` is the arrival of the line before it
/// (`None` for the first, which gets a zero delta).
///
/// - [`Timestamps::Absolute`]: the arrival's wall-clock time in the machine's local zone,
///   by the `strftime` string `format`, [`DEFAULT_TIMESTAMP_FORMAT`] (`14:03:07.042`) if
///   it is `None`. chrono parses the format, so `%.3f` is `.042`, `%3f` is `042`, and
///   `%Y-%m-%d %H:%M:%S%.6f` a full date. A string chrono rejects (an unknown `%Q`, a
///   trailing `%`) or an empty one is replaced by the default and logged once per
///   distinct string, not once per line.
/// - [`Timestamps::AbsoluteUtc`]: `2026-09-29T21:53:44.123456Z`, ignoring `format`.
/// - [`Timestamps::Relative`]: `+12.345678`, seconds since `epoch`.
/// - [`Timestamps::Delta`]: `+0.000123`, seconds since `previous`.
///
/// The local zone is read from the operating system for every call, so a stamp after a
/// daylight-saving change is right. [`format_timestamp_in`] takes the zone as an
/// argument, for tests and for callers that pin one.
pub fn format_timestamp(
    mode: Timestamps,
    format: Option<&str>,
    epoch: &Epoch,
    at: Instant,
    previous: Option<Instant>,
) -> String {
    format_timestamp_in(&Local, mode, format, epoch, at, previous)
}

/// [`format_timestamp`] in `zone` instead of the machine's local zone. Only
/// [`Timestamps::Absolute`] depends on it.
pub fn format_timestamp_in<Tz>(
    zone: &Tz,
    mode: Timestamps,
    format: Option<&str>,
    epoch: &Epoch,
    at: Instant,
    previous: Option<Instant>,
) -> String
where
    Tz: TimeZone,
    Tz::Offset: fmt::Display,
{
    let mut out = String::new();
    if mode != Timestamps::None {
        let format = match mode {
            Timestamps::Absolute => usable_format(format),
            _ => DEFAULT_TIMESTAMP_FORMAT,
        };
        write_stamp(&mut out, zone, mode, format, epoch, at, previous);
    }
    out
}

/// Append the stamp; `format` has been through [`usable_format`].
fn write_stamp<Tz>(
    out: &mut String,
    zone: &Tz,
    mode: Timestamps,
    format: &str,
    epoch: &Epoch,
    at: Instant,
    previous: Option<Instant>,
) where
    Tz: TimeZone,
    Tz::Offset: fmt::Display,
{
    // Formatting into a String cannot fail.
    match mode {
        Timestamps::None => {}
        Timestamps::Absolute => {
            let time = datetime_of(epoch.wall_time(at)).with_timezone(zone);
            let start = out.len();
            // A format chrono parsed can still fail to print; the default cannot.
            if write!(out, "{}", time.format(format)).is_err() {
                out.truncate(start);
                let _ = write!(out, "{}", time.format(DEFAULT_TIMESTAMP_FORMAT));
            }
        }
        Timestamps::AbsoluteUtc => {
            let _ = write!(out, "{}", format_utc(epoch.wall_time(at)));
        }
        Timestamps::Relative => {
            let since = at.saturating_duration_since(epoch.instant);
            let _ = write!(out, "+{}", format_secs(since));
        }
        Timestamps::Delta => {
            let since = previous.map_or(Duration::ZERO, |p| at.saturating_duration_since(p));
            let _ = write!(out, "+{}", format_secs(since));
        }
    }
}

/// `format` if chrono can parse it, else [`DEFAULT_TIMESTAMP_FORMAT`], with a warning the
/// first time a string is refused.
fn usable_format(format: Option<&str>) -> &str {
    match format {
        None => DEFAULT_TIMESTAMP_FORMAT,
        Some(format) if !format.is_empty() && !has_format_error(format) => format,
        Some(format) => {
            warn_invalid_format(format);
            DEFAULT_TIMESTAMP_FORMAT
        }
    }
}

fn has_format_error(format: &str) -> bool {
    StrftimeItems::new(format).any(|item| matches!(item, Item::Error))
}

/// Strings already warned about, so a bad setting is one log line, not one per stamp.
/// Bounded: past the limit the oldest is forgotten.
static WARNED: Mutex<Vec<String>> = Mutex::new(Vec::new());
const WARNED_LIMIT: usize = 8;

fn warn_invalid_format(format: &str) {
    {
        let mut warned = WARNED.lock();
        if warned.iter().any(|seen| seen == format) {
            return;
        }
        if warned.len() >= WARNED_LIMIT {
            warned.remove(0);
        }
        warned.push(format.to_owned());
    }
    tracing::warn!(
        format,
        default = DEFAULT_TIMESTAMP_FORMAT,
        "invalid timestamp format; using the default"
    );
}

/// `t` as a UTC date and time. A time chrono cannot represent (a billion years out)
/// becomes the Unix epoch rather than a panic.
fn datetime_of(t: SystemTime) -> DateTime<Utc> {
    let (secs, nanos) = match t.duration_since(UNIX_EPOCH) {
        Ok(after) => (
            i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
            after.subsec_nanos(),
        ),
        Err(error) => {
            let before = error.duration();
            let secs = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
            match before.subsec_nanos() {
                0 => (-secs, 0),
                nanos => (-secs - 1, 1_000_000_000 - nanos),
            }
        }
    };
    DateTime::from_timestamp(secs, nanos).unwrap_or(DateTime::UNIX_EPOCH)
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
    datetime_of(t).format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;
    use std::sync::Arc;

    use chrono::FixedOffset;
    use tracing::field::{Field, Visit};

    use super::*;

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(UNIX_EPOCH), "1970-01-01T00:00:00.000000Z");
        let t = UNIX_EPOCH + Duration::new(1_790_718_824, 123_456_789);
        assert_eq!(format_utc(t), "2026-09-29T21:53:44.123456Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(format_utc(leap), "2000-02-29T00:00:00.000000Z");
        assert_eq!(format_secs(Duration::new(12, 345_678_000)), "12.345678");
        // Before the epoch, with a fraction: a second earlier, the fraction complemented.
        let before = UNIX_EPOCH - Duration::from_millis(1500);
        assert_eq!(format_utc(before), "1969-12-31T23:59:58.500000Z");
    }

    /// 2026-09-29 21:53:44.123456789 UTC at the returned instant.
    fn epoch() -> (Epoch, Instant) {
        let instant = Instant::now();
        let wall = UNIX_EPOCH + Duration::new(1_790_718_824, 123_456_789);
        (Epoch { instant, wall }, instant)
    }

    /// Five and a half hours east of UTC (India): 03:23:44.123 on the 30th.
    fn india() -> FixedOffset {
        FixedOffset::east_opt(5 * 3600 + 1800).expect("a valid offset")
    }

    fn absolute(format: Option<&str>) -> String {
        let (epoch, start) = epoch();
        format_timestamp_in(&india(), Timestamps::Absolute, format, &epoch, start, None)
    }

    #[test]
    fn absolute_is_local_time_in_the_default_format() {
        assert_eq!(DEFAULT_TIMESTAMP_FORMAT, "%H:%M:%S%.3f");
        assert_eq!(absolute(None), "03:23:44.123");
        let (epoch, start) = epoch();
        let later = start + Duration::from_millis(1500);
        assert_eq!(
            format_timestamp_in(&india(), Timestamps::Absolute, None, &epoch, later, None),
            "03:23:45.623"
        );
        // The zone moves the wall clock and nothing else.
        let utc = format_timestamp_in(&Utc, Timestamps::Absolute, None, &epoch, start, None);
        assert_eq!(utc, "21:53:44.123");
        let west = FixedOffset::west_opt(4 * 3600).unwrap();
        let eastern = format_timestamp_in(&west, Timestamps::Absolute, None, &epoch, start, None);
        assert_eq!(eastern, "17:53:44.123");
    }

    #[test]
    fn absolute_takes_any_strftime_format() {
        assert_eq!(absolute(Some("%H:%M:%S")), "03:23:44", "no fraction");
        assert_eq!(absolute(Some("%H:%M:%S%.3f")), "03:23:44.123");
        assert_eq!(
            absolute(Some("%H:%M:%S.%3f")),
            "03:23:44.123",
            "%3f has no dot"
        );
        assert_eq!(absolute(Some("%H:%M:%S%.6f")), "03:23:44.123456");
        // Windows system time has 100 ns ticks, so only the first seven fraction digits
        // are portable.
        assert!(absolute(Some("%H:%M:%S%.9f")).starts_with("03:23:44.1234567"));
        assert!(absolute(Some("%H:%M:%S%.f")).starts_with("03:23:44.1234567"));
        assert_eq!(
            absolute(Some("%Y-%m-%d %H:%M:%S%.3f")),
            "2026-09-30 03:23:44.123",
            "the date is the local one: the offset crossed midnight"
        );
        assert_eq!(absolute(Some("%I:%M %p")), "03:23 AM");
        assert_eq!(absolute(Some("%H:%M:%S %z")), "03:23:44 +0530");
        assert_eq!(absolute(Some("%s")), "1790718824", "epoch seconds");
        assert_eq!(absolute(Some("t=%H%%")), "t=03%");
    }

    #[test]
    fn a_local_stamp_matches_chromes_own_local_conversion() {
        // The machine's zone is not under the test's control, so compare with the same
        // conversion done by hand rather than with a literal.
        let (epoch, start) = epoch();
        for format in [None, Some("%Y-%m-%d %H:%M:%S%.3f %z")] {
            let at = start + Duration::from_millis(250);
            let expected = DateTime::<Local>::from(epoch.wall_time(at))
                .format(format.unwrap_or(DEFAULT_TIMESTAMP_FORMAT))
                .to_string();
            assert_eq!(
                format_timestamp(Timestamps::Absolute, format, &epoch, at, None),
                expected
            );
        }
    }

    #[test]
    fn utc_absolute_keeps_the_iso_form_whatever_the_format() {
        let (epoch, start) = epoch();
        let at = start + Duration::from_millis(1500);
        for format in [None, Some("%H:%M"), Some("%Q")] {
            assert_eq!(
                format_timestamp_in(&india(), Timestamps::AbsoluteUtc, format, &epoch, at, None),
                "2026-09-29T21:53:45.623456Z"
            );
        }
    }

    #[test]
    fn relative_and_delta_are_seconds_with_microseconds() {
        let (epoch, start) = epoch();
        let stamp = |mode, at, previous| {
            format_timestamp_in(&india(), mode, Some("%H"), &epoch, at, previous)
        };
        let at = start + Duration::new(12, 345_678_000);
        assert_eq!(stamp(Timestamps::Relative, at, None), "+12.345678");
        assert_eq!(stamp(Timestamps::Relative, start, None), "+0.000000");
        let previous = start + Duration::new(12, 345_555_000);
        assert_eq!(stamp(Timestamps::Delta, at, Some(previous)), "+0.000123");
        assert_eq!(
            stamp(Timestamps::Delta, at, None),
            "+0.000000",
            "the first line has no predecessor"
        );
        assert_eq!(
            stamp(Timestamps::Delta, at, Some(start)),
            "+12.345678",
            "the format is for absolute stamps only"
        );
        // Times before the epoch or the previous line clamp to zero.
        let early = start.checked_sub(Duration::from_secs(1)).unwrap_or(start);
        assert_eq!(stamp(Timestamps::Relative, early, None), "+0.000000");
        assert_eq!(stamp(Timestamps::Delta, early, Some(at)), "+0.000000");
        assert_eq!(stamp(Timestamps::None, at, Some(previous)), "");
    }

    #[test]
    fn an_invalid_format_falls_back_to_the_default() {
        for bad in ["%Q", "100%", "%H:%M:%Q", "%", ""] {
            assert_eq!(absolute(Some(bad)), "03:23:44.123", "{bad:?}");
        }
        // Valid strings that only look odd are not refused.
        assert_eq!(absolute(Some("no specifiers")), "no specifiers");
        assert_eq!(absolute(Some("%%")), "%");
    }

    /// Collects the events a closure logs, as `name=value` strings.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl tracing::Subscriber for Recorder {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Fields(String);
            impl Visit for Fields {
                fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
                    let _ = write!(self.0, "{}={value:?} ", field.name());
                }
            }
            let mut fields = Fields(format!("{} ", event.metadata().level()));
            event.record(&mut fields);
            self.0.lock().push(fields.0);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// The recorder, installed as the process's subscriber the first time it is asked
    /// for. A scoped one would race the tests that hit the same callsite on other
    /// threads: tracing caches whether anyone listens by the thread that got there first.
    fn recorder() -> Recorder {
        static RECORDER: std::sync::OnceLock<Recorder> = std::sync::OnceLock::new();
        RECORDER
            .get_or_init(|| {
                let recorder = Recorder::default();
                tracing::subscriber::set_global_default(recorder.clone())
                    .expect("no other test installs a subscriber");
                recorder
            })
            .clone()
    }

    #[test]
    fn an_invalid_format_is_warned_about_once() {
        let recorder = recorder();
        let (epoch, start) = epoch();
        let stamp = |format| {
            format_timestamp_in(
                &Utc,
                Timestamps::Absolute,
                Some(format),
                &epoch,
                start,
                None,
            )
        };
        // Strings no other test uses: the warning is once per process per string.
        for _ in 0..3 {
            assert_eq!(stamp("%H:%M:%S %Q-warned-once"), "21:53:44.123");
        }
        assert_eq!(stamp("%H:%M:%S %Q-warned-too"), "21:53:44.123");
        // Modes that ignore the format never look at it, so never warn about it.
        let relative = format_timestamp_in(
            &Utc,
            Timestamps::Relative,
            Some("%Q-never-looked-at"),
            &epoch,
            start,
            None,
        );
        assert_eq!(relative, "+0.000000");
        // A good format is silent.
        assert_eq!(stamp("%H:%M:%S %p-fine"), "21:53:44 PM-fine");
        let logged = recorder.0.lock().clone();
        let about = |needle: &str| -> Vec<&String> {
            logged.iter().filter(|line| line.contains(needle)).collect()
        };
        let once = about("%Q-warned-once");
        assert_eq!(once.len(), 1, "{logged:?}");
        assert!(once[0].starts_with("WARN "), "{once:?}");
        assert!(
            once[0].contains("%H:%M:%S%.3f"),
            "names the default: {once:?}"
        );
        assert_eq!(about("%Q-warned-too").len(), 1, "{logged:?}");
        assert!(about("never-looked-at").is_empty(), "{logged:?}");
        assert!(about("-fine").is_empty(), "{logged:?}");
    }

    #[test]
    fn export_stamps_are_bracketed_and_use_the_options_format() {
        use crate::store::Store;

        let base = Instant::now();
        let epoch = Epoch {
            instant: base,
            wall: UNIX_EPOCH + Duration::new(1_790_718_824, 123_456_789),
        };
        let mut store = Store::new(crate::store::StoreConfig {
            epoch: Some(epoch),
            ..Default::default()
        });
        store.append(b"first\r\n", base + Duration::from_millis(1500));
        store.append(b"second\r\n", base + Duration::from_millis(2000));
        let snapshot = store.snapshot();
        let all = snapshot.first_line()..snapshot.end();
        let text = |options: TextOptions| snapshot.text(all.clone(), options);

        let local = |format: &str, at_ms: u64| {
            let at = base + Duration::from_millis(at_ms);
            format_timestamp(Timestamps::Absolute, Some(format), &epoch, at, None)
        };
        let dated = "%Y-%m-%d %H:%M:%S%.3f";
        let options = TextOptions::default()
            .with_timestamps(Timestamps::Absolute)
            .with_timestamp_format(dated);
        assert_eq!(
            text(options),
            format!(
                "[{}] first\n[{}] second\n",
                local(dated, 1500),
                local(dated, 2000)
            )
        );
        let default = TextOptions::default().with_timestamps(Timestamps::Absolute);
        assert_eq!(
            text(default),
            format!(
                "[{}] first\n[{}] second\n",
                local(DEFAULT_TIMESTAMP_FORMAT, 1500),
                local(DEFAULT_TIMESTAMP_FORMAT, 2000)
            )
        );
        // An unusable format exports with the default rather than failing the export.
        let bad = TextOptions::default()
            .with_timestamps(Timestamps::Absolute)
            .with_timestamp_format("%Q-export");
        assert_eq!(
            text(bad),
            text(TextOptions::default().with_timestamps(Timestamps::Absolute))
        );
        // The format is ignored by the other modes.
        let relative = TextOptions::default()
            .with_timestamps(Timestamps::Relative)
            .with_timestamp_format(dated);
        assert_eq!(text(relative), "[+1.500000] first\n[+2.000000] second\n");
        let utc = TextOptions::default().with_timestamps(Timestamps::AbsoluteUtc);
        assert_eq!(
            text(utc),
            "[2026-09-29T21:53:45.623456Z] first\n[2026-09-29T21:53:46.123456Z] second\n"
        );
    }
}
