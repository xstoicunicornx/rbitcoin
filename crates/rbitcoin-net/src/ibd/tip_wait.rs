//! Diagnostic trace of the body that is holding the tip.
//!
//! Confirm is strictly ordered, so when tip+1 is not in hand every body behind
//! it waits. The stale re-get log cannot show this: arrival clears inflight and
//! re-marks the body pending, so bodies queued behind a hole look identical to
//! unanswered requests there, and the one body that matters is usually absent.
//!
//! This follows tip+1 only. Tracking starts when the tip reaches a height whose
//! successor is not in hand; if it is in hand the tip never waited and nothing
//! is recorded, so a healthy sync stays silent. Every getdata for that hash is
//! recorded along with any request made before it became the blocker, and one
//! line is emitted when the tip moves past it, carrying request and outcome
//! together. A single extra line fires if the wait crosses
//! [`STILL_WAITING_AFTER`], so a wait that never resolves is not invisible.

use bitcoin::BlockHash;
use std::time::{Duration, Instant};

/// Waits shorter than this are ordinary pipelining and are not logged.
pub(crate) const MIN_LOGGED_WAIT: Duration = Duration::from_secs(5);
/// One "still waiting" line per blocker once it has waited this long.
pub(crate) const STILL_WAITING_AFTER: Duration = Duration::from_secs(60);
/// Requests recorded per blocker; the rest are counted, not listed.
const MAX_ASKS: usize = 16;

struct TipWait {
    height: u32,
    hash: BlockHash,
    since: Instant,
    /// Asked before it became the blocker: how long before, and by whom.
    before: Option<(Duration, Vec<usize>)>,
    /// Asked after it became the blocker: peer and offset from `since`.
    asks: Vec<(usize, Duration)>,
    more_asks: usize,
    delivered: Option<(usize, Duration)>,
    warned: bool,
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
    /// `in_hand` is only evaluated when a new tip+1 appears. `prior` returns
    /// when the hash was first requested and by which peers, for a request
    /// made before it became the blocker.
    pub(crate) fn observe(
        &mut self,
        next: Option<(u32, BlockHash)>,
        in_hand: impl FnOnce(u32, &BlockHash) -> bool,
        prior: impl FnOnce(&BlockHash) -> Option<(Instant, Vec<usize>)>,
        now: Instant,
    ) -> Option<String> {
        let mut out = None;
        if let Some(w) = self.cur.as_mut() {
            if next.map(|(h, _)| h) == Some(w.height) {
                let waited = now.saturating_duration_since(w.since);
                if !w.warned && w.delivered.is_none() && waited >= STILL_WAITING_AFTER {
                    w.warned = true;
                    return Some(still_waiting_line(w, waited));
                }
                return None;
            }
            out = finish_line(w, now);
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
        let before = prior(&hash).map(|(t, mut peers)| {
            peers.sort_unstable();
            (now.saturating_duration_since(t), peers)
        });
        self.cur = Some(TipWait {
            height,
            hash,
            since: now,
            before,
            asks: Vec::new(),
            more_asks: 0,
            delivered: None,
            warned: false,
        });
        out
    }

    pub(crate) fn note_ask(&mut self, hash: &BlockHash, peer: usize, now: Instant) {
        let Some(w) = self.cur.as_mut() else { return };
        if w.hash != *hash {
            return;
        }
        if w.asks.len() < MAX_ASKS {
            w.asks.push((peer, now.saturating_duration_since(w.since)));
        } else {
            w.more_asks += 1;
        }
    }

    pub(crate) fn note_delivered(&mut self, hash: &BlockHash, peer: usize, now: Instant) {
        let Some(w) = self.cur.as_mut() else { return };
        if w.hash == *hash && w.delivered.is_none() {
            w.delivered = Some((peer, now.saturating_duration_since(w.since)));
        }
    }
}

fn finish_line(w: &TipWait, now: Instant) -> Option<String> {
    let waited = match w.delivered {
        Some((_, at)) => at,
        None => now.saturating_duration_since(w.since),
    };
    if waited < MIN_LOGGED_WAIT {
        return None;
    }
    let delivered = match w.delivered {
        Some((peer, at)) => format!("{peer}@{}s", at.as_secs()),
        None => "none".into(),
    };
    Some(format!(
        "ibd: tip+1 wait h={} hash={} waited={}s {} asked={} delivered={delivered}",
        w.height,
        w.hash,
        waited.as_secs(),
        before_field(w),
        asks_field(w),
    ))
}

fn still_waiting_line(w: &TipWait, waited: Duration) -> String {
    format!(
        "ibd: tip+1 still waiting h={} hash={} waited={}s {} asked={}",
        w.height,
        w.hash,
        waited.as_secs(),
        before_field(w),
        asks_field(w),
    )
}

fn before_field(w: &TipWait) -> String {
    match &w.before {
        Some((ago, peers)) => format!("asked_before={}s by={peers:?}", ago.as_secs()),
        None => "asked_before=none".into(),
    }
}

fn asks_field(w: &TipWait) -> String {
    if w.asks.is_empty() {
        return "none".into();
    }
    let mut s: Vec<String> = w
        .asks
        .iter()
        .map(|(p, at)| format!("{p}@{}s", at.as_secs()))
        .collect();
    if w.more_asks > 0 {
        s.push(format!("+{}", w.more_asks));
    }
    format!("[{}]", s.join(","))
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
        let out = tr.observe(Some((10, h(10))), |_, _| true, |_| None, t0);
        assert!(out.is_none());
        // The tip then moves on; nothing was tracked so nothing is logged.
        let out = tr.observe(Some((11, h(11))), |_, _| true, |_| None, secs(t0, 30));
        assert!(out.is_none());
    }

    #[test]
    fn short_wait_stays_silent() {
        let t0 = Instant::now();
        let mut tr = TipWaitTracker::new();
        tr.observe(Some((10, h(10))), |_, _| false, |_| None, t0);
        tr.note_delivered(&h(10), 3, secs(t0, 2));
        let out = tr.observe(Some((11, h(11))), |_, _| true, |_| None, secs(t0, 2));
        assert!(out.is_none());
    }

    #[test]
    fn long_wait_logs_request_history_and_deliverer() {
        let t0 = Instant::now();
        let mut tr = TipWaitTracker::new();
        let prior = |_: &BlockHash| Some((t0, vec![9, 5]));
        tr.observe(Some((10, h(10))), |_, _| false, prior, secs(t0, 83));
        let start = secs(t0, 83);
        tr.note_ask(&h(99), 1, secs(start, 1)); // other hash: ignored
        tr.note_ask(&h(10), 2, secs(start, 45));
        tr.note_ask(&h(10), 7, secs(start, 45));
        tr.note_delivered(&h(10), 7, secs(start, 56));
        let line = tr
            .observe(Some((11, h(11))), |_, _| true, |_| None, secs(start, 57))
            .expect("56s wait is logged");
        assert!(line.contains("h=10 "), "{line}");
        assert!(line.contains("waited=56s"), "{line}");
        assert!(line.contains("asked_before=83s by=[5, 9]"), "{line}");
        assert!(line.contains("asked=[2@45s,7@45s]"), "{line}");
        assert!(line.contains("delivered=7@56s"), "{line}");
    }

    #[test]
    fn unrequested_blocker_says_so() {
        let t0 = Instant::now();
        let mut tr = TipWaitTracker::new();
        tr.observe(Some((10, h(10))), |_, _| false, |_| None, t0);
        let line = tr
            .observe(Some((11, h(11))), |_, _| true, |_| None, secs(t0, 20))
            .expect("20s wait is logged");
        assert!(line.contains("asked_before=none"), "{line}");
        assert!(line.contains("asked=none"), "{line}");
        assert!(line.contains("delivered=none"), "{line}");
    }

    #[test]
    fn still_waiting_fires_once() {
        let t0 = Instant::now();
        let mut tr = TipWaitTracker::new();
        tr.observe(Some((10, h(10))), |_, _| false, |_| None, t0);
        assert!(tr
            .observe(Some((10, h(10))), |_, _| false, |_| None, secs(t0, 59))
            .is_none());
        let line = tr
            .observe(Some((10, h(10))), |_, _| false, |_| None, secs(t0, 60))
            .expect("crosses the threshold");
        assert!(line.starts_with("ibd: tip+1 still waiting h=10 "), "{line}");
        assert!(tr
            .observe(Some((10, h(10))), |_, _| false, |_| None, secs(t0, 300))
            .is_none());
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
                |_| None,
                secs(t0, i),
            );
        }
        assert_eq!(probes, 1);
    }
}
