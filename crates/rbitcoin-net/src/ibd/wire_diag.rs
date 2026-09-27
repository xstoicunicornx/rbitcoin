//! Diagnostic: what each peer's getdata queue really holds, and where block
//! bytes go.
//!
//! Getdata cannot be cancelled and is not deduplicated on the wire, so a peer
//! answers every request it was sent, in order. Our bookkeeping
//! (`PeerSlot::in_flight`, `IbdWorkState::inflight`) forgets a request when an
//! owner is dropped from a hash, so it can undercount what the peer is still
//! working through. This keeps a shadow count per peer (hashes sent minus
//! blocks and notfound received) and a short history per hash (who was asked,
//! how many times, how deep their queue was at the first ask, and every
//! arrival with what we did with it).
//!
//! Output, all at debug:
//! - detail appended to the tip+1 wait lines ([`WireDiag::describe`]);
//! - one `ibd: tip+1 aftermath` line [`AFTERMATH_AFTER`] after a logged
//!   blocker, counting copies of it that arrived once it was no longer needed;
//! - one `ibd: wire` line every [`EMIT_EVERY`] splitting received blocks and
//!   bytes by outcome, with the shadow queue total against the counted one.
//!
//! No behaviour change: nothing here is read by assignment or confirm.

use super::peer_io::PeerSlot;
use bitcoin::BlockHash;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

const EMIT_EVERY: Duration = Duration::from_secs(5);
/// Hash records untouched this long are pruned.
const HASH_IDLE_PRUNE: Duration = Duration::from_secs(600);
pub(crate) const AFTERMATH_AFTER: Duration = Duration::from_secs(60);
/// Arrivals listed per hash; the rest are counted.
const MAX_ARRIVALS: usize = 12;
const MAX_AFTERMATH: usize = 32;

/// Which assign path sent a getdata.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AskSite {
    ReorgNeed,
    Densify,
    StealHung,
    TipHole,
    #[cfg(test)]
    Test,
}

impl AskSite {
    fn tag(self) -> &'static str {
        match self {
            AskSite::ReorgNeed => "reorg_need",
            AskSite::Densify => "densify",
            AskSite::StealHung => "steal_hung",
            AskSite::TipHole => "tip_hole",
            #[cfg(test)]
            AskSite::Test => "test",
        }
    }
}

/// Why our record that a peer was asked for a hash was last erased. A re-ask
/// to the same peer is only possible after one of these (or an untracked
/// clear or prune).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Erase {
    /// `drop_hash_owner` from `cover_tip_holes`.
    TipHoleDrop,
    /// `drop_hash_owner` from `steal_hung_densify`.
    StealDrop,
    NotFound,
    /// A requested copy arrived (cleared for every peer).
    Delivered,
}

impl Erase {
    fn tag(self) -> &'static str {
        match self {
            Erase::TipHoleDrop => "tip_hole_drop",
            Erase::StealDrop => "steal_drop",
            Erase::NotFound => "notfound",
            Erase::Delivered => "delivered",
        }
    }
}

fn erase_tag(e: Option<Erase>) -> &'static str {
    e.map(Erase::tag).unwrap_or("untracked")
}

/// What `apply_block_framed` did with a received body (or notfound).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Outcome {
    /// Not in inflight, height at or below tip: a copy of a block we have.
    StaleCopy,
    /// Not in inflight, height above tip or unknown.
    Unsolicited,
    Rejected,
    HaveBlock,
    BadHeader,
    HeaderErr,
    NoHeight,
    TooFar,
    Reorg,
    Ready,
    InQueue,
    QueueErr,
    Queued,
    DecodeFailed,
    NotFound,
}

impl Outcome {
    fn tag(self) -> &'static str {
        match self {
            Outcome::StaleCopy => "stale_copy",
            Outcome::Unsolicited => "unsolicited",
            Outcome::Rejected => "rejected",
            Outcome::HaveBlock => "have_block",
            Outcome::BadHeader => "bad_header",
            Outcome::HeaderErr => "header_err",
            Outcome::NoHeight => "no_height",
            Outcome::TooFar => "too_far",
            Outcome::Reorg => "reorg",
            Outcome::Ready => "ready",
            Outcome::InQueue => "in_queue",
            Outcome::QueueErr => "queue_err",
            Outcome::Queued => "queued",
            Outcome::DecodeFailed => "decode_failed",
            Outcome::NotFound => "notfound",
        }
    }
}

#[derive(Default)]
struct PeerWire {
    /// Block hashes sent in getdata.
    asked: u64,
    /// Block, decode-failed, and notfound entries received.
    answered: u64,
}

impl PeerWire {
    fn outstanding(&self) -> u64 {
        self.asked.saturating_sub(self.answered)
    }
}

struct PeerAsk {
    peer: usize,
    first: Instant,
    n: u32,
    /// Shadow queue depth of this peer when it was first asked for the hash.
    ahead: u64,
    /// Last recorded erase of this (hash, peer) since the last ask.
    erased: Option<Erase>,
}

struct Arrival {
    peer: usize,
    at: Instant,
    outcome: Outcome,
}

struct HashRec {
    first_ask: Instant,
    last: Instant,
    asks: Vec<PeerAsk>,
    total_asks: u32,
    arrivals: Vec<Arrival>,
    total_arrivals: u32,
    arrival_bytes: u64,
    /// Re-asks by (site, erase) tag.
    reasks: Vec<((&'static str, &'static str), u32)>,
}

fn bump(v: &mut Vec<((&'static str, &'static str), u32)>, k: (&'static str, &'static str)) {
    match v.iter_mut().find(|(x, _)| *x == k) {
        Some((_, n)) => *n += 1,
        None => v.push((k, 1)),
    }
}

impl HashRec {
    fn new(now: Instant) -> Self {
        Self {
            first_ask: now,
            last: now,
            asks: Vec::new(),
            total_asks: 0,
            arrivals: Vec::new(),
            total_arrivals: 0,
            arrival_bytes: 0,
            reasks: Vec::new(),
        }
    }

    fn note_arrival(&mut self, peer: usize, outcome: Outcome, bytes: usize, now: Instant) {
        self.last = now;
        self.total_arrivals += 1;
        self.arrival_bytes += bytes as u64;
        if self.arrivals.len() < MAX_ARRIVALS {
            self.arrivals.push(Arrival {
                peer,
                at: now,
                outcome,
            });
        }
    }
}

struct Aftermath {
    height: u32,
    hash: BlockHash,
    finished: Instant,
    asks: u32,
    arrivals: u32,
    bytes: u64,
}

#[derive(Default)]
struct Interval {
    asks: u64,
    /// Asks to a peer already asked for the same hash.
    reasks: u64,
    /// First asks by site.
    firsts_by_site: HashMap<&'static str, u64>,
    /// Re-asks by (site, erase).
    reasks_by: HashMap<(&'static str, &'static str), u64>,
    by_outcome: HashMap<Outcome, (u64, u64)>,
}

pub(crate) struct WireDiag {
    peers: HashMap<usize, PeerWire>,
    hashes: HashMap<BlockHash, HashRec>,
    aftermath: Vec<Aftermath>,
    iv: Interval,
    iv_start: Instant,
}

impl WireDiag {
    pub(crate) fn new() -> Self {
        Self {
            peers: HashMap::new(),
            hashes: HashMap::new(),
            aftermath: Vec::new(),
            iv: Interval::default(),
            iv_start: Instant::now(),
        }
    }

    /// One getdata batch sent to `peer` by `site`, in wire order.
    pub(crate) fn note_asks(
        &mut self,
        peer: usize,
        hashes: &[BlockHash],
        site: AskSite,
        now: Instant,
    ) {
        let pw = self.peers.entry(peer).or_default();
        for h in hashes {
            let ahead = pw.outstanding();
            pw.asked += 1;
            self.iv.asks += 1;
            let rec = self.hashes.entry(*h).or_insert_with(|| HashRec::new(now));
            rec.last = now;
            rec.total_asks += 1;
            if let Some(a) = rec.asks.iter_mut().find(|a| a.peer == peer) {
                a.n += 1;
                let k = (site.tag(), erase_tag(a.erased.take()));
                self.iv.reasks += 1;
                *self.iv.reasks_by.entry(k).or_default() += 1;
                bump(&mut rec.reasks, k);
            } else {
                *self.iv.firsts_by_site.entry(site.tag()).or_default() += 1;
                rec.asks.push(PeerAsk {
                    peer,
                    first: now,
                    n: 1,
                    ahead,
                    erased: None,
                });
            }
        }
    }

    pub(crate) fn note_block(
        &mut self,
        peer: usize,
        hash: BlockHash,
        bytes: usize,
        outcome: Outcome,
        now: Instant,
    ) {
        self.peers.entry(peer).or_default().answered += 1;
        let e = self.iv.by_outcome.entry(outcome).or_default();
        e.0 += 1;
        e.1 += bytes as u64;
        if let Some(rec) = self.hashes.get_mut(&hash) {
            rec.note_arrival(peer, outcome, bytes, now);
            if !matches!(outcome, Outcome::StaleCopy | Outcome::Unsolicited) {
                for a in &mut rec.asks {
                    a.erased = Some(Erase::Delivered);
                }
            }
        }
    }

    /// Our record that `peer` was asked for `hash` was erased (peer still
    /// holds the getdata unless `why` is notfound).
    pub(crate) fn note_erase(&mut self, hash: &BlockHash, peer: usize, why: Erase) {
        if let Some(a) = self
            .hashes
            .get_mut(hash)
            .and_then(|r| r.asks.iter_mut().find(|a| a.peer == peer))
        {
            a.erased = Some(why);
        }
    }

    pub(crate) fn note_notfound(&mut self, peer: usize, hashes: &[BlockHash], now: Instant) {
        self.peers.entry(peer).or_default().answered += hashes.len() as u64;
        self.iv.by_outcome.entry(Outcome::NotFound).or_default().0 += hashes.len() as u64;
        for h in hashes {
            if let Some(rec) = self.hashes.get_mut(h) {
                rec.note_arrival(peer, Outcome::NotFound, 0, now);
            }
            self.note_erase(h, peer, Erase::NotFound);
        }
    }

    /// Detail for a tip+1 wait line. Offsets are relative to `since`, when the
    /// hash became tip+1 (negative = before).
    pub(crate) fn describe(
        &self,
        hash: &BlockHash,
        since: Instant,
        slots: &[PeerSlot],
    ) -> String {
        let Some(rec) = self.hashes.get(hash) else {
            return "first_ask=none".into();
        };
        let mut s = format!(
            "first_ask={} asks={} by={{",
            off(rec.first_ask, since),
            rec.total_asks
        );
        let mut asks: Vec<&PeerAsk> = rec.asks.iter().collect();
        asks.sort_by_key(|a| a.first);
        for (i, a) in asks.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            // peer:times@first_ask/queue_ahead_at_first_ask>queue_now/counted_now
            let _ = write!(
                s,
                "{}:{}@{}/q{}>{}",
                a.peer,
                a.n,
                off(a.first, since),
                a.ahead,
                self.peer_now(a.peer, slots)
            );
        }
        s.push_str("} reask_by=");
        push_counts(&mut s, rec.reasks.iter().map(|(k, n)| (*k, *n as u64)));
        let _ = write!(s, " arrivals={}[", rec.total_arrivals);
        for (i, a) in rec.arrivals.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let _ = write!(s, "{}@{}:{}", a.peer, off(a.at, since), a.outcome.tag());
        }
        s.push(']');
        s
    }

    /// Shadow queue now / counted in_flight now, or `gone` if not alive.
    fn peer_now(&self, peer: usize, slots: &[PeerSlot]) -> String {
        let out = self.peers.get(&peer).map(|p| p.outstanding()).unwrap_or(0);
        match slots.iter().find(|s| s.id == peer && s.alive) {
            Some(slot) => format!("{out}/{}", slot.in_flight.len()),
            None => format!("{out}/gone"),
        }
    }

    /// Schedule an aftermath line for a blocker whose wait was logged.
    pub(crate) fn watch_aftermath(&mut self, height: u32, hash: BlockHash, now: Instant) {
        if self.aftermath.len() >= MAX_AFTERMATH {
            return;
        }
        let (asks, arrivals, bytes) = self
            .hashes
            .get(&hash)
            .map(|r| (r.total_asks, r.total_arrivals, r.arrival_bytes))
            .unwrap_or((0, 0, 0));
        self.aftermath.push(Aftermath {
            height,
            hash,
            finished: now,
            asks,
            arrivals,
            bytes,
        });
    }

    /// Periodic lines: aftermath for blockers finished long enough ago, and
    /// the interval summary. Also prunes idle hash records.
    pub(crate) fn tick(&mut self, now: Instant, slots: &[PeerSlot]) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.aftermath.len() {
            let a = &self.aftermath[i];
            if now.saturating_duration_since(a.finished) < AFTERMATH_AFTER {
                i += 1;
                continue;
            }
            let a = self.aftermath.swap_remove(i);
            out.push(self.aftermath_line(&a, now));
        }
        if now.saturating_duration_since(self.iv_start) < EMIT_EVERY {
            return out;
        }
        out.push(self.interval_line(now, slots));
        self.iv = Interval::default();
        self.iv_start = now;
        let keep: Vec<BlockHash> = self.aftermath.iter().map(|a| a.hash).collect();
        self.hashes.retain(|h, r| {
            now.saturating_duration_since(r.last) < HASH_IDLE_PRUNE || keep.contains(h)
        });
        out
    }

    fn aftermath_line(&self, a: &Aftermath, now: Instant) -> String {
        let (asks, arrivals, bytes, by) = match self.hashes.get(&a.hash) {
            Some(r) => {
                let mut by: HashMap<usize, u32> = HashMap::new();
                for x in &r.arrivals {
                    *by.entry(x.peer).or_default() += 1;
                }
                let mut by: Vec<_> = by.into_iter().collect();
                by.sort_unstable();
                (r.total_asks, r.total_arrivals, r.arrival_bytes, by)
            }
            None => (a.asks, a.arrivals, a.bytes, Vec::new()),
        };
        format!(
            "ibd: tip+1 aftermath h={} hash={} after={}s asks={} (+{} since wait ended) arrivals={} (+{} since) bytes={} (+{} since) listed_by={by:?}",
            a.height,
            a.hash,
            now.saturating_duration_since(a.finished).as_secs(),
            asks,
            asks.saturating_sub(a.asks),
            arrivals,
            arrivals.saturating_sub(a.arrivals),
            fmt_bytes(bytes),
            fmt_bytes(bytes.saturating_sub(a.bytes)),
        )
    }

    fn interval_line(&self, now: Instant, slots: &[PeerSlot]) -> String {
        let secs = now.saturating_duration_since(self.iv_start).as_secs_f64();
        let (mut blocks, mut bytes) = (0u64, 0u64);
        let mut rows: Vec<(&'static str, u64, u64)> = Vec::new();
        for (o, &(n, b)) in &self.iv.by_outcome {
            if *o != Outcome::NotFound {
                blocks += n;
                bytes += b;
            }
            rows.push((o.tag(), n, b));
        }
        rows.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)));
        let mut s = format!(
            "ibd: wire {:.1}s rx={} blocks {} asks={} reasks={} [",
            secs,
            blocks,
            fmt_bytes(bytes),
            self.iv.asks,
            self.iv.reasks
        );
        for (i, (tag, n, b)) in rows.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            let _ = write!(s, "{tag}={n}/{}", fmt_bytes(*b));
        }
        s.push_str("] first_by={");
        let mut firsts: Vec<_> = self.iv.firsts_by_site.iter().collect();
        firsts.sort_by(|a, b| b.1.cmp(a.1));
        for (i, (site, n)) in firsts.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let _ = write!(s, "{site}:{n}");
        }
        s.push_str("} reask_by=");
        push_counts(&mut s, self.iv.reasks_by.iter().map(|(k, n)| (*k, *n)));
        let (mut wire_q, mut counted, mut max): (u64, usize, Option<(usize, u64, usize)>) =
            (0, 0, None);
        for slot in slots.iter().filter(|s| s.alive) {
            let out = self.peers.get(&slot.id).map(|p| p.outstanding()).unwrap_or(0);
            wire_q += out;
            counted += slot.in_flight.len();
            if max.map(|m| out > m.1).unwrap_or(true) {
                max = Some((slot.id, out, slot.in_flight.len()));
            }
        }
        let _ = write!(s, " wire_q={wire_q} counted={counted}");
        if let Some((id, out, cnt)) = max {
            let _ = write!(s, " max={id}:{out}/{cnt}");
        }
        s
    }
}

/// `{site/erase:n,...}`, largest first.
fn push_counts(s: &mut String, it: impl Iterator<Item = ((&'static str, &'static str), u64)>) {
    let mut v: Vec<_> = it.collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    s.push('{');
    for (i, ((site, why), n)) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{site}/{why}:{n}");
    }
    s.push('}');
}

fn off(t: Instant, since: Instant) -> String {
    if t >= since {
        format!("+{:.1}s", t.duration_since(since).as_secs_f64())
    } else {
        format!("-{:.1}s", since.duration_since(t).as_secs_f64())
    }
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1 << 20 {
        format!("{:.1}MiB", b as f64 / (1u64 << 20) as f64)
    } else {
        format!("{}KiB", b >> 10)
    }
}
