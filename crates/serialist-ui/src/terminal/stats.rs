//! Per-frame costs of the terminal element over the last 60 frames, for the frame-time
//! overlay and for tests that hold the element to its budget.

use std::collections::VecDeque;
use std::time::Duration;

/// Frames kept.
pub const WINDOW: usize = 60;

/// What one frame of the element cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameSample {
    pub prepaint: Duration,
    pub paint: Duration,
    /// Lines the element asked the source for, visible or needed for wrap counts and
    /// delta timestamps.
    pub lines_fetched: usize,
    /// Visible lines that missed the shaped-line cache and were shaped.
    pub lines_shaped: usize,
    /// Visible lines found in the shaped-line cache.
    pub cache_hits: usize,
}

impl FrameSample {
    pub fn total(&self) -> Duration {
        self.prepaint + self.paint
    }
}

#[derive(Clone, Debug, Default)]
pub struct FrameStats {
    samples: VecDeque<FrameSample>,
    /// Frames recorded since creation, for tests.
    frames: u64,
}

/// A summary of the kept frames.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameSummary {
    pub frames: usize,
    pub mean_prepaint_ms: f64,
    pub mean_paint_ms: f64,
    pub max_total_ms: f64,
    pub lines_shaped: usize,
    /// Share of visible lines served from the cache, 0.0 to 1.0.
    pub hit_rate: f64,
}

impl FrameStats {
    pub fn record(&mut self, sample: FrameSample) {
        if self.samples.len() == WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
        self.frames += 1;
    }

    /// Paint happens after prepaint recorded the frame; it fills in the newest sample.
    pub fn record_paint(&mut self, paint: Duration) {
        if let Some(last) = self.samples.back_mut() {
            last.paint = paint;
        }
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn last(&self) -> Option<FrameSample> {
        self.samples.back().copied()
    }

    pub fn samples(&self) -> impl DoubleEndedIterator<Item = &FrameSample> {
        self.samples.iter()
    }

    pub fn summary(&self) -> FrameSummary {
        let frames = self.samples.len();
        if frames == 0 {
            return FrameSummary::default();
        }
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let sum = |f: fn(&FrameSample) -> Duration| -> f64 {
            self.samples.iter().map(|s| ms(f(s))).sum::<f64>() / frames as f64
        };
        let hits: usize = self.samples.iter().map(|s| s.cache_hits).sum();
        let shaped: usize = self.samples.iter().map(|s| s.lines_shaped).sum();
        FrameSummary {
            frames,
            mean_prepaint_ms: sum(|s| s.prepaint),
            mean_paint_ms: sum(|s| s.paint),
            max_total_ms: self
                .samples
                .iter()
                .map(|s| ms(s.total()))
                .fold(0.0, f64::max),
            lines_shaped: shaped,
            hit_rate: if hits + shaped == 0 {
                1.0
            } else {
                hits as f64 / (hits + shaped) as f64
            },
        }
    }
}

impl FrameSummary {
    /// The overlay's lines.
    pub fn lines(&self) -> [String; 4] {
        [
            format!("prepaint {:.2} ms", self.mean_prepaint_ms),
            format!(
                "paint    {:.2} ms (max {:.2})",
                self.mean_paint_ms, self.max_total_ms
            ),
            format!(
                "shaped   {} lines / {} frames",
                self.lines_shaped, self.frames
            ),
            format!("cache    {:.0}% hits", self.hit_rate * 100.0),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_last_sixty_frames() {
        let mut stats = FrameStats::default();
        for i in 0..100 {
            stats.record(FrameSample {
                prepaint: Duration::from_millis(i),
                lines_shaped: 1,
                cache_hits: 3,
                ..FrameSample::default()
            });
            stats.record_paint(Duration::from_millis(1));
        }
        let summary = stats.summary();
        assert_eq!(summary.frames, WINDOW);
        assert_eq!(stats.frames(), 100);
        // Frames 40..100: mean prepaint 69.5 ms.
        assert!((summary.mean_prepaint_ms - 69.5).abs() < 1e-9);
        assert!((summary.mean_paint_ms - 1.0).abs() < 1e-9);
        assert!((summary.max_total_ms - 100.0).abs() < 1e-9);
        assert_eq!(summary.lines_shaped, 60);
        assert!((summary.hit_rate - 0.75).abs() < 1e-9);
        assert_eq!(summary.lines()[3], "cache    75% hits");
    }

    #[test]
    fn empty_stats_summarise_to_zero() {
        let summary = FrameStats::default().summary();
        assert_eq!(summary.frames, 0);
        assert_eq!(summary.hit_rate, 0.0);
    }
}
