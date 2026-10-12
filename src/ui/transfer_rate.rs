//! Live speed and time left of a running transfer, measured over the last few
//! seconds of its progress ticks.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How far back the speed looks. Long enough to smooth the gaps between small
/// files, short enough to follow a link that slows down.
const SPEED_WINDOW: Duration = Duration::from_secs(3);

/// A speed taken over less than this is mostly noise.
const MIN_SPEED_SPAN: Duration = Duration::from_secs(1);

/// The time left swings harder than the speed, so it waits a little longer.
const MIN_TIME_LEFT_SPAN: Duration = Duration::from_secs(2);

/// Recent `(when, bytes done)` progress ticks of one transfer.
#[derive(Default)]
pub struct TransferRate {
    samples: VecDeque<(Instant, u64)>,
}

impl TransferRate {
    pub fn record(&mut self, now: Instant, done: u64) {
        self.samples.push_back((now, done));
        // Keep one tick from before the window: it is where the window starts.
        while self.samples.len() > 2 && is_before_window(self.samples[1].0, now) {
            self.samples.pop_front();
        }
    }

    pub fn reset(&mut self) {
        self.samples.clear();
    }

    /// Bytes per second over the window, and how long the measured span is.
    /// `None` until the first tick.
    fn measure(&self, now: Instant) -> Option<(f64, Duration)> {
        let (_, done) = *self.samples.back()?;
        // The newest tick that is a full window old. When even the last tick
        // is that old the transfer stands still, and the speed comes out as 0.
        let (start, done_at_start) = self
            .samples
            .iter()
            .rev()
            .find(|(at, _)| is_before_window(*at, now))
            .or(self.samples.front())
            .copied()?;
        let span = now.saturating_duration_since(start);
        if span.is_zero() {
            return None;
        }
        Some((
            done.saturating_sub(done_at_start) as f64 / span.as_secs_f64(),
            span,
        ))
    }

    pub fn bytes_per_sec(&self, now: Instant) -> Option<u64> {
        let (speed, span) = self.measure(now)?;
        (span >= MIN_SPEED_SPAN).then_some(speed as u64)
    }

    /// Seconds until `remaining` bytes are through at the current speed.
    /// `None` while the speed is unknown or zero.
    pub fn seconds_left(&self, now: Instant, remaining: u64) -> Option<u64> {
        let (speed, span) = self.measure(now)?;
        if span < MIN_TIME_LEFT_SPAN || speed < 1.0 {
            return None;
        }
        Some((remaining as f64 / speed).ceil() as u64)
    }
}

fn is_before_window(at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(at) >= SPEED_WINDOW
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    /// Ticks every 100 ms at `mb_per_sec`, from `from_ms` up to `to_ms`.
    fn record_steady(
        rate: &mut TransferRate,
        start: Instant,
        from_ms: u64,
        to_ms: u64,
        mb_per_sec: u64,
    ) {
        let mut done = rate.samples.back().map(|(_, done)| *done).unwrap_or(0);
        for millis in (from_ms..=to_ms).step_by(100) {
            rate.record(at(start, millis), done);
            done += mb_per_sec * MB / 10;
        }
    }

    #[test]
    fn nothing_is_known_before_the_first_second() {
        let start = Instant::now();
        let mut rate = TransferRate::default();
        assert_eq!(rate.bytes_per_sec(start), None);

        record_steady(&mut rate, start, 0, 500, 10);

        assert_eq!(rate.bytes_per_sec(at(start, 500)), None);
        assert_eq!(rate.seconds_left(at(start, 500), 100 * MB), None);
    }

    #[test]
    fn speed_shows_before_the_time_left() {
        let start = Instant::now();
        let mut rate = TransferRate::default();

        record_steady(&mut rate, start, 0, 1500, 10);

        assert_eq!(rate.bytes_per_sec(at(start, 1500)), Some(10 * MB));
        assert_eq!(rate.seconds_left(at(start, 1500), 100 * MB), None);
    }

    #[test]
    fn steady_transfer_gives_its_speed_and_time_left() {
        let start = Instant::now();
        let mut rate = TransferRate::default();

        record_steady(&mut rate, start, 0, 2000, 10);

        assert_eq!(rate.bytes_per_sec(at(start, 2000)), Some(10 * MB));
        assert_eq!(rate.seconds_left(at(start, 2000), 100 * MB), Some(10));
    }

    #[test]
    fn speed_follows_the_last_seconds_not_the_whole_transfer() {
        let start = Instant::now();
        let mut rate = TransferRate::default();

        record_steady(&mut rate, start, 0, 10_000, 10);
        record_steady(&mut rate, start, 10_100, 20_000, 5);

        assert_eq!(rate.bytes_per_sec(at(start, 20_000)), Some(5 * MB));
        assert!(rate.samples.len() <= 32, "old ticks must be dropped");
    }

    #[test]
    fn stalled_transfer_drops_to_zero_and_hides_the_time_left() {
        let start = Instant::now();
        let mut rate = TransferRate::default();

        record_steady(&mut rate, start, 0, 5000, 10);

        assert_eq!(rate.bytes_per_sec(at(start, 9000)), Some(0));
        assert_eq!(rate.seconds_left(at(start, 9000), 100 * MB), None);
    }

    #[test]
    fn far_apart_ticks_still_give_an_average() {
        let start = Instant::now();
        let mut rate = TransferRate::default();

        rate.record(start, 0);
        rate.record(at(start, 10_000), 100 * MB);

        assert_eq!(rate.bytes_per_sec(at(start, 10_000)), Some(10 * MB));
    }

    #[test]
    fn reset_forgets_the_ticks() {
        let start = Instant::now();
        let mut rate = TransferRate::default();
        record_steady(&mut rate, start, 0, 3000, 10);

        rate.reset();

        assert_eq!(rate.bytes_per_sec(at(start, 3000)), None);
    }
}
