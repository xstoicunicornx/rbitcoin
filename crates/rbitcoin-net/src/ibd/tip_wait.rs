//! Diagnostic trace of the body that is holding the tip.
//!
//! Confirm is strictly ordered, so when tip+1 is not in hand every body behind
//! it waits. The stale re-get log cannot show this: arrival clears inflight and
//! re-marks the body pending, so bodies queued behind a hole look identical to
//! unanswered requests there, and the one body that matters is usually absent.
//!
//! This follows tip+1 only. Tracking starts when the tip reaches a height whose
//! successor is not in hand; if it is in hand the tip never waited and nothing
//! is recorded, so a healthy sync stays silent. One line is emitted when the tip
//! moves past it, with request and arrival history from
//! [`super::wire_diag`]. A single extra line fires if the wait crosses
//! [`STILL_WAITING_AFTER`], so a wait that never resolves is not invisible.

use bitcoin::BlockHash;
use std::time::{Duration, Instant};

/// Waits shorter than this are ordinary pipelining and are not logged.
pub(crate) const MIN_LOGGED_WAIT: Duration = Duration::from_secs(5);
/// One "still waiting" line per blocker once it has waited this long.
pub(crate) const STILL_WAITING_AFTER: Duration = Duration::from_secs(60);

struct TipWait {
    height: u32,
    hash: BlockHash,
    since: Instant,
    delivered: Option<(usize, Duration)>,
    warned: bool,
}

/// A wait worth logging. Request and arrival history for `hash` comes from
/// [`super::wire_diag::WireDiag::describe`].
pub(crate) struct TipWaitReport {
    /// True for the one-off line at [`STILL_WAITING_AFTER`].
    pub still: bool,
    pub height: u32,
    pub hash: BlockHash,
    pub since: Instant,
    pub waited: Duration,
    pub delivered: Option<(usize, Duration)>,
}

impl TipWaitReport {
    pub(crate) fn head(&self) -> String {
        if self.still {
            return format!(
                "ibd: tip+1 still waiting h={} hash={} waited={}s",
                self.height,
                self.hash,
                self.waited.as_secs(),
            );
        }
        let delivered = match self.delivered {
            Some((peer, at)) => format!("{peer}@{:.1}s", at.as_secs_f64()),
            None => "none".into(),
        };
        format!(
            "ibd: tip+1 wait h={} hash={} waited={}s delivered={delivered}",
            self.height,
            self.hash,
            self.waited.as_secs(),
        )
    }
}

#[derive(Default)]
pub(crate) struct TipWaitTracker {
    cur: Option<TipWait>,
    /// Tip+1 height last found in hand, so the probe is not repeated while
    /// confirm works through it.
    checked: Option<u32>,
}

impl TipWaitTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Call once per loop with the current tip+1, when its header is known.
    ///
    /// `in_hand` is only evaluated when a new tip+1 appears.
    pub(crate) fn observe(
        &mut self,
        next: Option<(u32, BlockHash)>,
        in_hand: impl FnOnce(u32, &BlockHash) -> bool,
        now: Instant,
    ) -> Option<TipWaitReport> {
        let mut out = None;
        if let Some(w) = self.cur.as_mut() {
            if next.map(|(h, _)| h) == Some(w.height) {
                let waited = now.saturating_duration_since(w.since);
                if !w.warned && w.delivered.is_none() && waited >= STILL_WAITING_AFTER {
                    w.warned = true;
                    return Some(report(w, true, waited));
                }
                return None;
            }
            out = finish(w, now);
            self.cur = None;
        }
        let Some((height, hash)) = next else {
            return out;
        };
        if self.checked == Some(height) {
            return out;
        }
        self.checked = Some(height);
        if in_hand(height, &hash) {
            return out;
        }
        self.cur = Some(TipWait {
            height,
            hash,
            since: now,
            delivered: None,
            warned: false,
        });
        out
    }

    pub(crate) fn note_delivered(&mut self, hash: &BlockHash, peer: usize, now: Instant) {
        let Some(w) = self.cur.as_mut() else { return };
        if w.hash == *hash && w.delivered.is_none() {
            w.delivered = Some((peer, now.saturating_duration_since(w.since)));
        }
    }
}

fn report(w: &TipWait, still: bool, waited: Duration) -> TipWaitReport {
    TipWaitReport {
        still,
        height: w.height,
        hash: w.hash,
        since: w.since,
        waited,
        delivered: w.delivered,
    }
}

fn finish(w: &TipWait, now: Instant) -> Option<TipWaitReport> {
    let waited = match w.delivered {
        Some((_, at)) => at,
        None => now.saturating_duration_since(w.since),
    };
    if waited < MIN_LOGGED_WAIT {
        return None;
    }
    Some(report(w, false, waited))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;

    fn h(n: u32) -> BlockHash {
        let mut b = [0u8; 32];
        b[0..4].copy_from_slice(&n.to_le_bytes());
        BlockHash::from_byte_array(b)
    }

    fn secs(t0: Instant, s: u64) -> Instant {
        t0 + Duration::from_secs(s)
    }

    #[test]
    fn in_hand_successor_is_never_tracked() {
        let t0 = Instant::now();
        let mut tr = TipWaitTracker::new();
        assert!(tr.observe(Some((10, h(10))), |_, _| true, t0).is_none());
        assert!(tr.observe(Some((11, h(11))), |_, _| true, secs(t0, 30)).is_none());
    }

    #[test]
    fn short_wait_stays_silent() {
        let t0 = Instant::now();
        let mut tr = TipWaitTracker::new();
        tr.observe(Some((10, h(10))), |_, _| false, t0);
        tr.note_delivered(&h(10), 3, secs(t0, 2));
        assert!(tr.observe(Some((11, h(11))), |_, _| true, secs(t0, 2)).is_none());
    }

    #[test]
    fn long_wait_reports_deliverer() {
        let t0 = Instant::now();
        let mut tr = TipWaitTracker::new();
        tr.observe(Some((10, h(10))), |_, _| false, t0);
        tr.note_delivered(&h(99), 1, secs(t0, 1)); // other hash: ignored
        tr.note_delivered(&h(10), 7, secs(t0, 56));
        let r = tr
            .observe(Some((11, h(11))), |_, _| true, secs(t0, 57))
            .expect("56s wait is logged");
        assert!(!r.still);
        let line = r.head();
        assert!(line.contains("h=10 "), "{line}");
        assert!(line.contains("waited=56s"), "{line}");
        assert!(line.contains("delivered=7@56.0s"), "{line}");
    }

    #[test]
    fn undelivered_blocker_says_so() {
        let t0 = Instant::now();
        let mut tr = TipWaitTracker::new();
        tr.observe(Some((10, h(10))), |_, _| false, t0);
        let r = tr
            .observe(Some((11, h(11))), |_, _| true, secs(t0, 20))
            .expect("20s wait is logged");
        assert!(r.head().contains("delivered=none"));
    }

    #[test]
    fn still_waiting_fires_once() {
        let t0 = Instant::now();
        let mut tr = TipWaitTracker::new();
        tr.observe(Some((10, h(10))), |_, _| false, t0);
        assert!(tr.observe(Some((10, h(10))), |_, _| false, secs(t0, 59)).is_none());
        let r = tr
            .observe(Some((10, h(10))), |_, _| false, secs(t0, 60))
            .expect("crosses the threshold");
        assert!(r.still);
        assert!(r.head().starts_with("ibd: tip+1 still waiting h=10 "));
        assert!(tr.observe(Some((10, h(10))), |_, _| false, secs(t0, 300)).is_none());
    }

    #[test]
    fn in_hand_probe_runs_once_per_height() {
        let t0 = Instant::now();
        let mut tr = TipWaitTracker::new();
        let mut probes = 0;
        for i in 0..5 {
            tr.observe(
                Some((10, h(10))),
                |_, _| {
                    probes += 1;
                    true
                },
                secs(t0, i),
            );
        }
        assert_eq!(probes, 1);
    }
}
