//! The timestamp gutter: which clock to show and how each one is formatted.
//!
//! Formatting is plain Rust over [`Instant`]s and the session's [`Epoch`], with the
//! UTC offset passed in so tests do not depend on the machine's time zone.

use std::time::{Duration, Instant, UNIX_EPOCH};

use serialist_core::Epoch;

/// What the gutter shows next to each line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TimestampMode {
    #[default]
    Off,
    /// Wall-clock time the line started arriving: `14:03:07.042`.
    Absolute,
    /// Time since the session started: `+00:01:23.456`.
    Relative,
    /// Time since the previous line started: `+    0.004`.
    Delta,
}

impl TimestampMode {
    /// The order the toggle action walks through.
    pub const ALL: [TimestampMode; 4] = [
        TimestampMode::Off,
        TimestampMode::Absolute,
        TimestampMode::Relative,
        TimestampMode::Delta,
    ];

    pub fn next(self) -> Self {
        let ix = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        Self::ALL[(ix + 1) % Self::ALL.len()]
    }

    pub fn label(self) -> &'static str {
        match self {
            TimestampMode::Off => "Off",
            TimestampMode::Absolute => "Absolute",
            TimestampMode::Relative => "Relative",
            TimestampMode::Delta => "Delta",
        }
    }

    /// Width of the formatted stamp in cells, excluding the gap before the text. Longer
    /// values (a relative stamp past 99 hours, a delta past 99 999 s) overflow into the
    /// gap rather than widening the gutter, so the gutter never jumps while scrolling.
    pub fn width(self) -> usize {
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
    /// Seconds east of UTC for absolute stamps.
    pub utc_offset: i32,
}

impl Clock {
    /// The session's epoch with this machine's current UTC offset.
    pub fn local(epoch: Epoch) -> Self {
        let utc_offset = chrono::Local::now().offset().local_minus_utc();
        Self { epoch, utc_offset }
    }

    /// The stamp for a line received at `at`, whose predecessor (if retained) was
    /// received at `previous`. `None` when the mode is off.
    pub fn format(
        &self,
        mode: TimestampMode,
        at: Instant,
        previous: Option<Instant>,
    ) -> Option<String> {
        match mode {
            TimestampMode::Off => None,
            TimestampMode::Absolute => Some(self.absolute(at)),
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

    fn absolute(&self, at: Instant) -> String {
        let wall = self.epoch.wall_time(at);
        let since_unix = wall
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i128;
        let local = since_unix + i128::from(self.utc_offset) * 1000;
        let of_day = local.rem_euclid(86_400_000) as u64;
        let (hours, minutes) = (of_day / 3_600_000, of_day / 60_000 % 60);
        let (seconds, millis) = (of_day / 1000 % 60, of_day % 1000);
        format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
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
    use std::time::SystemTime;

    use super::*;

    /// An epoch whose instant maps to 2026-09-29 13:02:03.500 UTC.
    fn clock(utc_offset: i32) -> (Clock, Instant) {
        let instant = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_millis(1_790_686_923_500);
        let epoch = Epoch { instant, wall };
        (Clock { epoch, utc_offset }, instant)
    }

    #[test]
    fn off_formats_nothing() {
        let (clock, start) = clock(0);
        assert_eq!(clock.format(TimestampMode::Off, start, None), None);
        assert_eq!(TimestampMode::Off.width(), 0);
    }

    #[test]
    fn absolute_is_wall_clock_time_in_the_given_offset() {
        let (clock_utc, start) = clock(0);
        let at = start + Duration::from_millis(1_042);
        assert_eq!(
            clock_utc.format(TimestampMode::Absolute, at, None).unwrap(),
            "13:02:04.542"
        );
        let (eastern, start) = clock(-4 * 3600);
        assert_eq!(
            eastern
                .format(TimestampMode::Absolute, start, None)
                .unwrap(),
            "09:02:03.500"
        );
        // Past midnight in a zone ahead of UTC wraps to the next day.
        let (tokyo, start) = clock(11 * 3600);
        assert_eq!(
            tokyo.format(TimestampMode::Absolute, start, None).unwrap(),
            "00:02:03.500"
        );
        let stamp = tokyo.format(TimestampMode::Absolute, start, None).unwrap();
        assert_eq!(stamp.len(), TimestampMode::Absolute.width());
    }

    #[test]
    fn relative_counts_from_the_session_start() {
        let (clock, start) = clock(0);
        let stamp = |ms| {
            clock
                .format(
                    TimestampMode::Relative,
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
            clock.format(TimestampMode::Relative, early, None).unwrap(),
            "+00:00:00.000"
        );
    }

    #[test]
    fn delta_is_the_gap_to_the_previous_line() {
        let (clock, start) = clock(0);
        let at = start + Duration::from_millis(1_004);
        assert_eq!(
            clock
                .format(
                    TimestampMode::Delta,
                    at,
                    Some(start + Duration::from_millis(1_000))
                )
                .unwrap(),
            "+    0.004"
        );
        assert_eq!(
            clock.format(TimestampMode::Delta, at, Some(start)).unwrap(),
            "+    1.004"
        );
        assert_eq!(
            clock.format(TimestampMode::Delta, at, None).unwrap(),
            "+    0.000",
            "the first retained line has no predecessor"
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

    #[test]
    fn local_clock_uses_a_sane_offset() {
        let clock = Clock::local(Epoch {
            instant: Instant::now(),
            wall: SystemTime::now(),
        });
        assert!(clock.utc_offset.abs() <= 14 * 3600);
    }
}
