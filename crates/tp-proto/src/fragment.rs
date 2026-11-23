use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};

use crate::error::{Error, Result};
use crate::header::{HEADER_LEN, MediaHeader, StreamKind};

/// Splits frames into datagrams and stamps sequence and frame ids. One per
/// stream kind, so each kind has its own counters and loss is measured per kind.
#[derive(Debug)]
pub struct Fragmenter {
    kind: StreamKind,
    next_sequence: u32,
    next_frame_id: u32,
}

impl Fragmenter {
    pub fn new(kind: StreamKind) -> Self {
        Self { kind, next_sequence: 0, next_frame_id: 0 }
    }

    /// Returns ready-to-send datagrams, each no larger than `max_datagram`.
    pub fn fragment(
        &mut self,
        frame: &[u8],
        timestamp_us: u64,
        keyframe: bool,
        max_datagram: usize,
    ) -> Result<Vec<Bytes>> {
        let chunk = max_datagram.saturating_sub(HEADER_LEN);
        if chunk == 0 {
            return Err(Error::FrameTooLarge(frame.len()));
        }
        // An empty frame still produces one datagram so the receiver sees the
        // frame id advance and does not count it as lost.
        let count = frame.len().div_ceil(chunk).max(1);
        let count: u16 = count.try_into().map_err(|_| Error::FrameTooLarge(frame.len()))?;
        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);

        let mut out = Vec::with_capacity(count as usize);
        for index in 0..count {
            let start = index as usize * chunk;
            let end = (start + chunk).min(frame.len());
            let header = MediaHeader {
                kind: self.kind,
                sequence: self.next_sequence,
                timestamp_us,
                frame_id,
                frag_index: index,
                frag_count: count,
                keyframe,
            };
            self.next_sequence = self.next_sequence.wrapping_add(1);
            let mut buf = BytesMut::with_capacity(HEADER_LEN + end - start);
            buf.extend_from_slice(&header.to_bytes());
            buf.extend_from_slice(&frame[start..end]);
            out.push(buf.freeze());
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompleteFrame {
    pub kind: StreamKind,
    pub frame_id: u32,
    pub timestamp_us: u64,
    pub keyframe: bool,
    pub data: Bytes,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReassemblyOutcome {
    /// Frames given up on during this call.
    pub lost: u32,
}

#[derive(Debug)]
struct Partial {
    timestamp_us: u64,
    keyframe: bool,
    first_seen: Instant,
    chunks: Vec<Option<Bytes>>,
    received: u16,
}

/// Wrapping comparison so frame ids keep ordering correctly across u32 wrap.
fn newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Rebuilds frames from datagrams for one stream kind.
///
/// Frames are delivered in frame-id order only: once frame N is delivered, any
/// incomplete frame older than N is dropped and counted as lost, because a
/// decoder cannot use a frame that arrives after its successor. Incomplete
/// frames also expire after `deadline` so a lost fragment surfaces as loss
/// promptly instead of waiting for the next frame to finish.
#[derive(Debug)]
pub struct Reassembler {
    deadline: Duration,
    partials: BTreeMap<u32, Partial>,
    last_delivered: Option<u32>,
    expected_sequence: Option<u32>,
    datagrams_received: u64,
    datagrams_missing: u64,
    frames_lost: u64,
}

/// Bound on how many frames may be in flight at once, so a peer cannot make us
/// hold unbounded partial state by sending first fragments only.
const MAX_PARTIALS: usize = 64;

impl Reassembler {
    pub fn new(deadline: Duration) -> Self {
        Self {
            deadline,
            partials: BTreeMap::new(),
            last_delivered: None,
            expected_sequence: None,
            datagrams_received: 0,
            datagrams_missing: 0,
            frames_lost: 0,
        }
    }

    /// Feeds one datagram. Returns a frame when this datagram completed one.
    pub fn push(
        &mut self,
        header: &MediaHeader,
        payload: &[u8],
        now: Instant,
    ) -> (Option<CompleteFrame>, ReassemblyOutcome) {
        let mut outcome = ReassemblyOutcome::default();
        self.track_sequence(header.sequence);

        if let Some(last) = self.last_delivered
            && !newer(header.frame_id, last)
        {
            return (None, outcome);
        }

        if !self.partials.contains_key(&header.frame_id) && self.partials.len() >= MAX_PARTIALS {
            let oldest = self.oldest_key();
            if let Some(k) = oldest {
                self.partials.remove(&k);
                outcome.lost += 1;
            }
        }

        let partial = self.partials.entry(header.frame_id).or_insert_with(|| Partial {
            timestamp_us: header.timestamp_us,
            keyframe: header.keyframe,
            first_seen: now,
            chunks: vec![None; header.frag_count as usize],
            received: 0,
        });
        // A count that disagrees with the first fragment seen means corruption or
        // a hostile peer; ignore the datagram rather than index out of range.
        if partial.chunks.len() != header.frag_count as usize {
            self.frames_lost += outcome.lost as u64;
            return (None, outcome);
        }
        let slot = &mut partial.chunks[header.frag_index as usize];
        if slot.is_none() {
            *slot = Some(Bytes::copy_from_slice(payload));
            partial.received += 1;
        }

        let mut done = None;
        if partial.received as usize == partial.chunks.len() {
            let partial = self.partials.remove(&header.frame_id).expect("present");
            let len = partial.chunks.iter().flatten().map(Bytes::len).sum();
            let mut data = BytesMut::with_capacity(len);
            for c in partial.chunks.into_iter().flatten() {
                data.extend_from_slice(&c);
            }
            // Everything older than the frame we are about to hand out is now
            // useless to the caller.
            let stale: Vec<u32> = self
                .partials
                .keys()
                .copied()
                .filter(|&k| newer(header.frame_id, k))
                .collect();
            for k in stale {
                self.partials.remove(&k);
                outcome.lost += 1;
            }
            // Frames that never sent a single datagram show up only as a gap in
            // frame ids.
            if let Some(last) = self.last_delivered {
                let gap = header.frame_id.wrapping_sub(last).saturating_sub(1);
                outcome.lost += gap.saturating_sub(outcome.lost);
            }
            self.last_delivered = Some(header.frame_id);
            done = Some(CompleteFrame {
                kind: header.kind,
                frame_id: header.frame_id,
                timestamp_us: partial.timestamp_us,
                keyframe: partial.keyframe,
                data: data.freeze(),
            });
        }
        self.frames_lost += outcome.lost as u64;
        (done, outcome)
    }

    /// Drops incomplete frames older than the deadline.
    pub fn expire(&mut self, now: Instant) -> ReassemblyOutcome {
        let deadline = self.deadline;
        let before = self.partials.len();
        let mut newest_dropped = None;
        self.partials.retain(|&id, p| {
            let keep = now.duration_since(p.first_seen) < deadline;
            if !keep && newest_dropped.is_none_or(|n| newer(id, n)) {
                newest_dropped = Some(id);
            }
            keep
        });
        // Nothing older than an abandoned frame can be delivered later either,
        // or the decoder would see frames out of order.
        if let Some(n) = newest_dropped
            && self.last_delivered.is_none_or(|l| newer(n, l))
        {
            self.last_delivered = Some(n);
        }
        let lost = (before - self.partials.len()) as u32;
        self.frames_lost += lost as u64;
        ReassemblyOutcome { lost }
    }

    /// When the next partial frame would expire, if any are pending.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.partials.values().map(|p| p.first_seen + self.deadline).min()
    }

    pub fn stats(&self) -> ReassemblyStats {
        ReassemblyStats {
            datagrams_received: self.datagrams_received,
            datagrams_missing: self.datagrams_missing,
            frames_lost: self.frames_lost,
        }
    }

    fn oldest_key(&self) -> Option<u32> {
        let mut it = self.partials.keys().copied();
        let first = it.next()?;
        Some(it.fold(first, |acc, k| if newer(acc, k) { k } else { acc }))
    }

    fn track_sequence(&mut self, seq: u32) {
        self.datagrams_received += 1;
        match self.expected_sequence {
            None => self.expected_sequence = Some(seq.wrapping_add(1)),
            Some(exp) => {
                if seq == exp || newer(seq, exp) {
                    self.datagrams_missing += seq.wrapping_sub(exp) as u64;
                    self.expected_sequence = Some(seq.wrapping_add(1));
                } else if self.datagrams_missing > 0 {
                    // A late datagram fills a gap counted earlier.
                    self.datagrams_missing -= 1;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReassemblyStats {
    pub datagrams_received: u64,
    pub datagrams_missing: u64,
    pub frames_lost: u64,
}

impl ReassemblyStats {
    /// Fraction of datagrams that never arrived, 0.0 to 1.0.
    pub fn loss_fraction(&self) -> f32 {
        let total = self.datagrams_received + self.datagrams_missing;
        if total == 0 { 0.0 } else { self.datagrams_missing as f32 / total as f32 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MTU: usize = HEADER_LEN + 10;

    fn frame(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_add(seed)).collect()
    }

    fn feed(r: &mut Reassembler, d: &Bytes, now: Instant) -> (Option<CompleteFrame>, u32) {
        let (h, p) = MediaHeader::decode(d).unwrap();
        let (f, o) = r.push(&h, p, now);
        (f, o.lost)
    }

    #[test]
    fn fragments_fit_and_carry_counters() {
        let mut f = Fragmenter::new(StreamKind::Video);
        let ds = f.fragment(&frame(25, 0), 1000, true, MTU).unwrap();
        assert_eq!(ds.len(), 3);
        for (i, d) in ds.iter().enumerate() {
            assert!(d.len() <= MTU);
            let (h, _) = MediaHeader::decode(d).unwrap();
            assert_eq!(h.frag_index as usize, i);
            assert_eq!(h.frag_count, 3);
            assert_eq!(h.sequence as usize, i);
            assert_eq!(h.frame_id, 0);
            assert!(h.keyframe);
        }
        let ds = f.fragment(&frame(5, 0), 2000, false, MTU).unwrap();
        let (h, _) = MediaHeader::decode(&ds[0]).unwrap();
        assert_eq!((h.frame_id, h.sequence, h.frag_count), (1, 3, 1));
    }

    #[test]
    fn empty_frame_is_one_datagram() {
        let mut f = Fragmenter::new(StreamKind::Audio);
        let ds = f.fragment(&[], 0, false, MTU).unwrap();
        assert_eq!(ds.len(), 1);
        let mut r = Reassembler::new(Duration::from_millis(50));
        let (got, lost) = feed(&mut r, &ds[0], Instant::now());
        assert!(got.unwrap().data.is_empty());
        assert_eq!(lost, 0);
    }

    #[test]
    fn too_small_mtu_errors() {
        let mut f = Fragmenter::new(StreamKind::Video);
        assert!(f.fragment(b"x", 0, false, HEADER_LEN).is_err());
    }

    #[test]
    fn reassembles_in_order() {
        let mut f = Fragmenter::new(StreamKind::Video);
        let mut r = Reassembler::new(Duration::from_millis(50));
        let now = Instant::now();
        let data = frame(95, 7);
        let ds = f.fragment(&data, 42, true, MTU).unwrap();
        let mut out = None;
        for d in &ds {
            let (got, lost) = feed(&mut r, d, now);
            assert_eq!(lost, 0);
            if got.is_some() {
                out = got;
            }
        }
        let out = out.unwrap();
        assert_eq!(&out.data[..], &data[..]);
        assert_eq!(out.timestamp_us, 42);
        assert!(out.keyframe);
    }

    #[test]
    fn reassembles_reordered_and_duplicated() {
        let mut f = Fragmenter::new(StreamKind::Video);
        let mut r = Reassembler::new(Duration::from_millis(50));
        let now = Instant::now();
        let data = frame(57, 3);
        let ds = f.fragment(&data, 1, false, MTU).unwrap();
        let order = [4, 0, 2, 2, 5, 1, 3];
        let mut out = None;
        for &i in &order {
            let (got, _) = feed(&mut r, &ds[i], now);
            if got.is_some() {
                assert!(out.is_none(), "delivered twice");
                out = got;
            }
        }
        assert_eq!(&out.unwrap().data[..], &data[..]);
        assert_eq!(r.stats().datagrams_missing, 0);
    }

    #[test]
    fn interleaved_frames_reassemble() {
        let mut f = Fragmenter::new(StreamKind::Video);
        let mut r = Reassembler::new(Duration::from_millis(50));
        let now = Instant::now();
        let a = f.fragment(&frame(20, 1), 1, false, MTU).unwrap();
        let b = f.fragment(&frame(20, 2), 2, false, MTU).unwrap();
        assert!(feed(&mut r, &a[0], now).0.is_none());
        assert!(feed(&mut r, &b[0], now).0.is_none());
        assert_eq!(feed(&mut r, &a[1], now).0.unwrap().frame_id, 0);
        assert_eq!(feed(&mut r, &b[1], now).0.unwrap().frame_id, 1);
    }

    #[test]
    fn incomplete_frame_expires_as_loss() {
        let mut f = Fragmenter::new(StreamKind::Video);
        let mut r = Reassembler::new(Duration::from_millis(50));
        let t0 = Instant::now();
        let ds = f.fragment(&frame(30, 0), 1, true, MTU).unwrap();
        feed(&mut r, &ds[0], t0);
        feed(&mut r, &ds[2], t0);
        assert_eq!(r.next_deadline(), Some(t0 + Duration::from_millis(50)));
        assert_eq!(r.expire(t0 + Duration::from_millis(10)).lost, 0);
        assert_eq!(r.expire(t0 + Duration::from_millis(60)).lost, 1);
        assert_eq!(r.next_deadline(), None);
        // The straggler arrives after we gave up: it must not resurrect the frame.
        assert!(feed(&mut r, &ds[1], t0 + Duration::from_millis(70)).0.is_none());
        assert_eq!(r.stats().frames_lost, 1);
        assert_eq!(r.stats().datagrams_missing, 0);
    }

    #[test]
    fn newer_complete_frame_drops_older_partial() {
        let mut f = Fragmenter::new(StreamKind::Video);
        let mut r = Reassembler::new(Duration::from_secs(5));
        let now = Instant::now();
        let a = f.fragment(&frame(20, 1), 1, false, MTU).unwrap();
        let b = f.fragment(&frame(20, 2), 2, false, MTU).unwrap();
        feed(&mut r, &a[0], now);
        feed(&mut r, &b[0], now);
        let (got, lost) = feed(&mut r, &b[1], now);
        assert_eq!(got.unwrap().frame_id, 1);
        assert_eq!(lost, 1);
        // Frame 0 completing late is not delivered out of order.
        assert!(feed(&mut r, &a[1], now).0.is_none());
    }

    #[test]
    fn wholly_missing_frame_counts_as_lost() {
        let mut f = Fragmenter::new(StreamKind::Audio);
        let mut r = Reassembler::new(Duration::from_millis(50));
        let now = Instant::now();
        let a = f.fragment(b"one", 1, false, MTU).unwrap();
        let _b = f.fragment(b"two", 2, false, MTU).unwrap();
        let _c = f.fragment(b"three", 3, false, MTU).unwrap();
        let d = f.fragment(b"four", 4, false, MTU).unwrap();
        assert_eq!(feed(&mut r, &a[0], now).1, 0);
        let (got, lost) = feed(&mut r, &d[0], now);
        assert_eq!(&got.unwrap().data[..], b"four");
        assert_eq!(lost, 2);
        let s = r.stats();
        assert_eq!(s.datagrams_missing, 2);
        assert!((s.loss_fraction() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn frame_ids_wrap() {
        let mut f = Fragmenter::new(StreamKind::Video);
        f.next_frame_id = u32::MAX;
        f.next_sequence = u32::MAX;
        let mut r = Reassembler::new(Duration::from_millis(50));
        let now = Instant::now();
        let a = f.fragment(b"a", 1, false, MTU).unwrap();
        let b = f.fragment(b"b", 2, false, MTU).unwrap();
        assert_eq!(feed(&mut r, &a[0], now).0.unwrap().frame_id, u32::MAX);
        let (got, lost) = feed(&mut r, &b[0], now);
        assert_eq!(got.unwrap().frame_id, 0);
        assert_eq!(lost, 0);
    }

    #[test]
    fn mismatched_fragment_count_is_ignored() {
        let mut r = Reassembler::new(Duration::from_millis(50));
        let now = Instant::now();
        let mut h = MediaHeader {
            kind: StreamKind::Video,
            sequence: 0,
            timestamp_us: 0,
            frame_id: 0,
            frag_index: 0,
            frag_count: 2,
            keyframe: false,
        };
        assert!(r.push(&h, b"a", now).0.is_none());
        h.sequence = 1;
        h.frag_count = 5;
        h.frag_index = 4;
        assert!(r.push(&h, b"b", now).0.is_none());
    }
}
