//! Spacing between episode downloads, and exponential retry waits after a
//! rate-limit response. Port of backoff.go.

use anyhow::{anyhow, Result};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Starting retry delay for a rate-limited episode when no delay is configured.
pub const DEFAULT_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);
/// Cap on exponential rate-limit waits.
pub const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug)]
pub struct Backoff {
    delay: Duration,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    last_done: Option<Instant>,
    rate_limit_hits: u32,
}

impl Backoff {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// How much longer to wait, measured at `now`, before the next download may
    /// start. Zero before the first download, or when no delay is configured.
    pub fn remaining_at(&self, now: Instant) -> Duration {
        if self.delay.is_zero() {
            return Duration::ZERO;
        }
        let g = self.inner.lock().unwrap();
        match g.last_done {
            Some(t) => self.delay.saturating_sub(now.saturating_duration_since(t)),
            None => Duration::ZERO,
        }
    }

    /// Sleeps until the configured delay has passed since the previous download.
    pub async fn wait(&self) {
        let remaining = self.remaining_at(Instant::now());
        if !remaining.is_zero() {
            crate::say!("Waiting {} before the next download (rate-limit backoff)...", fmt_duration(remaining));
            crate::progress::pause(remaining).await;
        }
    }

    /// Marks a download as finished, starting the clock for the next wait.
    pub fn done(&self) {
        if self.delay.is_zero() {
            return;
        }
        self.inner.lock().unwrap().last_done = Some(Instant::now());
    }

    /// Wait before retrying a rate-limited episode. Doubles per consecutive hit,
    /// starts at the configured delay if that is larger, and is capped.
    pub fn rate_limit_wait(&self) -> Duration {
        let base = DEFAULT_RATE_LIMIT_WAIT.max(self.delay);
        let hits = {
            let mut g = self.inner.lock().unwrap();
            let h = g.rate_limit_hits;
            g.rate_limit_hits += 1;
            h
        };
        let mut wait = base;
        for _ in 0..hits {
            if wait >= MAX_RATE_LIMIT_WAIT {
                break;
            }
            wait *= 2;
        }
        wait.min(MAX_RATE_LIMIT_WAIT)
    }

    /// Clears the consecutive-hit counter after a successful download.
    pub fn reset_rate_limit(&self) {
        self.inner.lock().unwrap().rate_limit_hits = 0;
    }
}

pub fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs_f64().round() as u64;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m{s}s"),
        _ => format!("{h}h{m}m{s}s"),
    }
}

/// Parses a Go-style duration: a sequence of number+unit pairs such as "30s",
/// "2m", "1h30m", "300ms". A bare "0" is accepted.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    if s.is_empty() {
        return Err(anyhow!("invalid duration {s:?}"));
    }
    let mut total = Duration::ZERO;
    let mut rest = s;
    while !rest.is_empty() {
        let num_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if num_end == 0 {
            return Err(anyhow!("invalid duration {s:?}"));
        }
        let value: f64 = rest[..num_end].parse().map_err(|_| anyhow!("invalid duration {s:?}"))?;
        rest = &rest[num_end..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let unit = &rest[..unit_end];
        rest = &rest[unit_end..];
        let nanos_per_unit: f64 = match unit {
            "ns" => 1.0,
            "us" | "µs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            _ => return Err(anyhow!("unknown unit {unit:?} in duration {s:?}")),
        };
        total += Duration::from_nanos((value * nanos_per_unit) as u64);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed reference instant so the tests never depend on the real clock.
    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn first_wait_is_noop() {
        let b = Backoff::new(Duration::from_secs(5));
        assert_eq!(b.remaining_at(t0()), Duration::ZERO);
    }

    #[test]
    fn waits_remaining_delay() {
        let b = Backoff::new(Duration::from_secs(5));
        let start = t0();
        {
            let mut g = b.inner.lock().unwrap();
            g.last_done = Some(start);
        }
        // Only 2s of the 5s delay has elapsed.
        assert_eq!(b.remaining_at(start + Duration::from_secs(2)), Duration::from_secs(3));
    }

    #[test]
    fn skips_wait_when_delay_elapsed() {
        let b = Backoff::new(Duration::from_secs(5));
        let start = t0();
        b.inner.lock().unwrap().last_done = Some(start);
        assert_eq!(b.remaining_at(start + Duration::from_secs(10)), Duration::ZERO);
    }

    #[test]
    fn disabled_is_noop() {
        let b = Backoff::new(Duration::ZERO);
        b.done();
        assert_eq!(b.remaining_at(t0()), Duration::ZERO);
        assert!(b.inner.lock().unwrap().last_done.is_none());
    }

    #[test]
    fn rate_limit_wait_grows_and_caps() {
        let b = Backoff::new(Duration::ZERO);
        assert_eq!(b.rate_limit_wait(), DEFAULT_RATE_LIMIT_WAIT);
        assert_eq!(b.rate_limit_wait(), 2 * DEFAULT_RATE_LIMIT_WAIT);
        assert_eq!(b.rate_limit_wait(), 4 * DEFAULT_RATE_LIMIT_WAIT);
        let mut last = Duration::ZERO;
        for _ in 0..20 {
            last = b.rate_limit_wait();
        }
        assert_eq!(last, MAX_RATE_LIMIT_WAIT);
    }

    #[test]
    fn rate_limit_wait_uses_configured_delay_as_floor() {
        let b = Backoff::new(Duration::from_secs(5 * 60));
        assert_eq!(b.rate_limit_wait(), Duration::from_secs(5 * 60));
    }

    #[test]
    fn rate_limit_wait_resets() {
        let b = Backoff::new(Duration::ZERO);
        b.rate_limit_wait();
        b.rate_limit_wait();
        b.reset_rate_limit();
        assert_eq!(b.rate_limit_wait(), DEFAULT_RATE_LIMIT_WAIT);
    }

    #[test]
    fn durations_parse_like_go() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("300ms").unwrap(), Duration::from_millis(300));
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("5").is_err());
    }
}
