//! The timestamp gutter: which clock to show and how each one is formatted.
//!
//! Absolute stamps are the core's [`format_timestamp`], the same function the export
//! uses, so the gutter and an exported file agree on the time of a line; the format is
//! the `display.timestamp_format` setting. Relative and delta stamps are numeric and
//! fixed-width (`+00:01:23.456`, `+    0.004`) so the gutter never changes width while
//! scrolling; the export writes those as plain seconds. The zone of absolute stamps can
//! be pinned so tests do not depend on the machine's time zone.

use std::time::{Duration, Instant};

use chrono::FixedOffset;
use serialist_core::Epoch;
use serialist_core::store::{Timestamps, format_timestamp, format_timestamp_in};

/// What the gutter shows next to each line: off, wall-clock time the line started
/// arriving (`14:03:07.042`), time since the session started (`+00:01:23.456`), or
/// time since the previous line started (`+    0.004`). The settings type, so the
/// `display.timestamps` setting is used as is.
pub use serialist_core::TimestampMode;

/// What the terminal does with a [`TimestampMode`] beyond what settings need.
pub trait TimestampModeExt: Sized + 'static {
    /// The order the toggle action walks through.
    const ALL: [Self; 4];

    /// The next mode in [`Self::ALL`], wrapping.
    fn next(self) -> Self;

    fn label(self) -> &'static str;

    /// Width of the formatted stamp in cells, excluding the gap before the text. Longer
    /// values (a relative stamp past 99 hours, a delta past 99 999 s) overflow into the
    /// gap rather than widening the gutter, so the gutter never jumps while scrolling.
    /// Absolute stamps are counted in the default format; [`Clock::width`] follows the
    /// `display.timestamp_format` setting.
    fn width(self) -> usize;
}

impl TimestampModeExt for TimestampMode {
    const ALL: [TimestampMode; 4] = [
        TimestampMode::Off,
        TimestampMode::Absolute,
        TimestampMode::Relative,
        TimestampMode::Delta,
    ];

    fn next(self) -> Self {
        let ix = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        Self::ALL[(ix + 1) % Self::ALL.len()]
    }

    fn label(self) -> &'static str {
        match self {
            TimestampMode::Off => "Off",
            TimestampMode::Absolute => "Absolute",
            TimestampMode::Relative => "Relative",
            TimestampMode::Delta => "Delta",
        }
    }

    fn width(self) -> usize {
        match self {
            TimestampMode::Off => 0,
            TimestampMode::Absolute => "HH:MM:SS.mmm".len(),
            TimestampMode::Relative => "+HH:MM:SS.mmm".len(),
            TimestampMode::Delta => "+SSSSS.mmm".len(),
        }
    }
}

/// Everything the formatter needs besides the line itself.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    pub epoch: Epoch,
    /// The zone of absolute stamps: `None` is the machine's local zone, read for every
    /// stamp so a daylight-saving change during a session is followed; `Some(seconds
    /// east of UTC)` pins one, and an offset chrono rejects (a day or more) is local.
    pub utc_offset: Option<i32>,
}

impl Clock {
    /// The session's epoch in the machine's local zone.
    pub fn local(epoch: Epoch) -> Self {
        Self {
            epoch,
            utc_offset: None,
        }
    }

    /// The session's epoch in a fixed zone, `utc_offset` seconds east of UTC.
    pub fn fixed(epoch: Epoch, utc_offset: i32) -> Self {
        Self {
            epoch,
            utc_offset: Some(utc_offset),
        }
    }

    /// The stamp for a line received at `at`, whose predecessor (if retained) was
    /// received at `previous`. `None` when the mode is off. `format` is the `strftime`
    /// string of absolute stamps (`None` for the default, `%H:%M:%S%.3f`); the other
    /// modes ignore it.
    pub fn format(
        &self,
        mode: TimestampMode,
        format: Option<&str>,
        at: Instant,
        previous: Option<Instant>,
    ) -> Option<String> {
        match mode {
            TimestampMode::Off => None,
            TimestampMode::Absolute => Some(self.absolute(format, at)),
            TimestampMode::Relative => Some(format_relative(
                at.saturating_duration_since(self.epoch.instant),
            )),
            TimestampMode::Delta => Some(format_delta(
                previous
                    .map(|previous| at.saturating_duration_since(previous))
                    .unwrap_or_default(),
            )),
        }
    }

    /// Width of the gutter in cells for `mode`, excluding the gap before the text: the
    /// fixed width of the numeric modes, and for absolute stamps the width of the
    /// session's first instant in `format`. A format whose width varies (month names)
    /// overflows into the gap rather than widening the gutter, like a long relative stamp.
    pub fn width(&self, mode: TimestampMode, format: Option<&str>) -> usize {
        match mode {
            TimestampMode::Absolute => self.absolute(format, self.epoch.instant).chars().count(),
            _ => mode.width(),
        }
    }

    fn absolute(&self, format: Option<&str>, at: Instant) -> String {
        let zone = self.utc_offset.and_then(FixedOffset::east_opt);
        match zone {
            Some(zone) => {
                format_timestamp_in(&zone, Timestamps::Absolute, format, &self.epoch, at, None)
            }
            None => format_timestamp(Timestamps::Absolute, format, &self.epoch, at, None),
        }
    }
}

/// `+HH:MM:SS.mmm`; hours keep counting past 99.
pub fn format_relative(elapsed: Duration) -> String {
    let millis = elapsed.as_millis();
    let (hours, minutes) = (millis / 3_600_000, millis / 60_000 % 60);
    let (seconds, millis) = (millis / 1000 % 60, millis % 1000);
    format!("+{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

/// `+SSSSS.mmm`, seconds right-aligned so deltas line up.
pub fn format_delta(delta: Duration) -> String {
    let millis = delta.as_millis();
    format!("+{:>5}.{:03}", millis / 1000, millis % 1000)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use chrono::{DateTime, Local};

    use super::*;

    /// An epoch whose instant maps to 2026-09-29 13:02:03.500 UTC.
    fn clock(utc_offset: i32) -> (Clock, Instant) {
        let instant = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_millis(1_790_686_923_500);
        let epoch = Epoch { instant, wall };
        (Clock::fixed(epoch, utc_offset), instant)
    }

    #[test]
    fn off_formats_nothing() {
        let (clock, start) = clock(0);
        assert_eq!(clock.format(TimestampMode::Off, None, start, None), None);
        assert_eq!(
            clock.format(TimestampMode::Off, Some("%H"), start, None),
            None
        );
        assert_eq!(TimestampMode::Off.width(), 0);
        assert_eq!(clock.width(TimestampMode::Off, Some("%Y-%m-%d %H")), 0);
    }

    #[test]
    fn absolute_is_wall_clock_time_in_the_given_offset() {
        let (clock_utc, start) = clock(0);
        let at = start + Duration::from_millis(1_042);
        let absolute = |clock: &Clock, at| {
            clock
                .format(TimestampMode::Absolute, None, at, None)
                .unwrap()
        };
        assert_eq!(absolute(&clock_utc, at), "13:02:04.542");
        let (eastern, start) = clock(-4 * 3600);
        assert_eq!(absolute(&eastern, start), "09:02:03.500");
        // Past midnight in a zone ahead of UTC wraps to the next day.
        let (tokyo, start) = clock(11 * 3600);
        assert_eq!(absolute(&tokyo, start), "00:02:03.500");
        assert_eq!(
            absolute(&tokyo, start).len(),
            TimestampMode::Absolute.width()
        );
    }

    #[test]
    fn absolute_takes_the_configured_format() {
        let (tokyo, start) = clock(11 * 3600);
        let with = |format: &str| {
            tokyo
                .format(TimestampMode::Absolute, Some(format), start, None)
                .unwrap()
        };
        assert_eq!(with("%H:%M:%S%.3f"), "00:02:03.500");
        assert_eq!(with("%H:%M:%S"), "00:02:03");
        assert_eq!(with("%H:%M:%S.%3f"), "00:02:03.500");
        assert_eq!(with("%H:%M:%S%.6f"), "00:02:03.500000");
        assert_eq!(
            with("%Y-%m-%d %H:%M:%S%.3f"),
            "2026-09-30 00:02:03.500",
            "the date is the local one, which the zone moved to the 30th"
        );
        // A format chrono cannot parse is the default, here and in the width.
        assert_eq!(with("%Q"), "00:02:03.500");
        assert_eq!(tokyo.width(TimestampMode::Absolute, Some("%Q")), 12);
        // The other modes are numeric whatever the setting says.
        let stamp = |mode| {
            tokyo
                .format(mode, Some("%Y-%m-%d"), start + Duration::from_secs(1), None)
                .unwrap()
        };
        assert_eq!(stamp(TimestampMode::Relative), "+00:00:01.000");
        assert_eq!(stamp(TimestampMode::Delta), "+    0.000");
    }

    #[test]
    fn the_gutter_is_as_wide_as_the_formatted_stamp() {
        let (clock, _) = clock(0);
        let width = |format: Option<&str>| clock.width(TimestampMode::Absolute, format);
        assert_eq!(width(None), TimestampMode::Absolute.width());
        assert_eq!(width(Some("%H:%M:%S%.3f")), 12);
        assert_eq!(width(Some("%H:%M")), 5);
        assert_eq!(width(Some("%Y-%m-%d %H:%M:%S%.3f")), 23);
        assert_eq!(width(Some("%H:%M:%S %z")), 14);
        // Characters, not bytes.
        assert_eq!(width(Some("%H\u{2236}%M")), 5);
        // The numeric modes keep their own widths.
        for mode in [TimestampMode::Relative, TimestampMode::Delta] {
            assert_eq!(clock.width(mode, Some("%Y-%m-%d")), mode.width());
        }
    }

    #[test]
    fn a_local_clock_follows_the_machines_zone() {
        // The zone is not under the test's control: compare with chrono's own local
        // conversion of the same instant.
        let epoch = Epoch {
            instant: Instant::now(),
            wall: SystemTime::now(),
        };
        let clock = Clock::local(epoch);
        assert_eq!(clock.utc_offset, None);
        let at = epoch.instant + Duration::from_millis(250);
        for (format, chrono) in [
            (None, "%H:%M:%S%.3f"),
            (Some("%Y-%m-%d %H:%M:%S%.3f"), "%Y-%m-%d %H:%M:%S%.3f"),
        ] {
            let expected = DateTime::<Local>::from(epoch.wall_time(at))
                .format(chrono)
                .to_string();
            assert_eq!(
                clock.format(TimestampMode::Absolute, format, at, None),
                Some(expected)
            );
        }
        // An offset a day wide is not a zone; it is the local one.
        let silly = Clock::fixed(epoch, 90_000);
        assert_eq!(
            silly.format(TimestampMode::Absolute, None, at, None),
            clock.format(TimestampMode::Absolute, None, at, None)
        );
    }

    #[test]
    fn relative_counts_from_the_session_start() {
        let (clock, start) = clock(0);
        let stamp = |ms| {
            clock
                .format(
                    TimestampMode::Relative,
                    None,
                    start + Duration::from_millis(ms),
                    None,
                )
                .unwrap()
        };
        assert_eq!(stamp(0), "+00:00:00.000");
        assert_eq!(stamp(83_456), "+00:01:23.456");
        assert_eq!(stamp(3_600_000 * 5 + 7), "+05:00:00.007");
        assert_eq!(stamp(0).len(), TimestampMode::Relative.width());
        assert_eq!(
            stamp(3_600_000 * 123),
            "+123:00:00.000",
            "hours keep counting"
        );
        // A line from before the epoch (clock skew in a replay) clamps to zero.
        let early = start.checked_sub(Duration::from_secs(1)).unwrap_or(start);
        assert_eq!(
            clock
                .format(TimestampMode::Relative, None, early, None)
                .unwrap(),
            "+00:00:00.000"
        );
    }

    #[test]
    fn delta_is_the_gap_to_the_previous_line() {
        let (clock, start) = clock(0);
        let at = start + Duration::from_millis(1_004);
        let delta = |previous| {
            clock
                .format(TimestampMode::Delta, None, at, previous)
                .unwrap()
        };
        assert_eq!(
            delta(Some(start + Duration::from_millis(1_000))),
            "+    0.004"
        );
        assert_eq!(delta(Some(start)), "+    1.004");
        assert_eq!(
            delta(None),
            "+    0.000",
            "the first retained line has none"
        );
        assert_eq!(
            format_delta(Duration::from_millis(12_345_678)),
            "+12345.678"
        );
        assert_eq!(
            format_delta(Duration::ZERO).len(),
            TimestampMode::Delta.width()
        );
    }

    #[test]
    fn the_toggle_cycles_through_every_mode() {
        let mut mode = TimestampMode::default();
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(mode);
            mode = mode.next();
        }
        assert_eq!(seen, TimestampMode::ALL);
        assert_eq!(mode, TimestampMode::Off);
    }
}
