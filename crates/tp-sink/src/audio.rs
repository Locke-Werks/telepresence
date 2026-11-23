//! Audio channel: a ring of interleaved f32 frames at 48 kHz.
//!
//! Indices are 64-bit frame counts that only ever increase, so they never wrap
//! in practice and "how far behind am I" is a subtraction. The writer never
//! waits: it overwrites whatever is oldest. A reader that falls more than the
//! ring behind skips forward to near live; a reader that catches up gets
//! silence for the missing part. A copy the writer overran while it was in
//! progress is detected by rereading the write index afterwards.

use std::sync::atomic::{Ordering, fence};

use crate::layout::*;
use crate::shm::{self, Mapping};
use crate::{Error, Result, current_pid, is_stale, now_us};

fn header(map: &Mapping) -> &AudioHeader {
    // SAFETY: mapping is page aligned and at least AUDIO_DATA_OFFSET long.
    unsafe { &*(map.ptr() as *const AudioHeader) }
}

fn samples_ptr(map: &Mapping) -> *mut f32 {
    // SAFETY: AUDIO_DATA_OFFSET is within the mapping and 4-byte aligned.
    unsafe { map.ptr().add(AUDIO_DATA_OFFSET as usize) as *mut f32 }
}

pub struct AudioWriter {
    map: Mapping,
    channels: u32,
    capacity: u64,
    write_index: u64,
}

impl AudioWriter {
    pub fn create(name: &str, channels: u32) -> Result<Self> {
        Self::create_with_capacity(name, channels, AUDIO_CAPACITY_FRAMES)
    }

    pub fn create_with_capacity(name: &str, channels: u32, capacity_frames: u32) -> Result<Self> {
        if channels == 0 || channels > 8 {
            return Err(Error::InvalidFormat("channels must be 1..=8"));
        }
        if capacity_frames == 0 {
            return Err(Error::InvalidFormat("capacity must be non-zero"));
        }
        let need = audio_region_size(channels, capacity_frames);
        let (map, created) = shm::create(name, need as usize)?;
        let h = header(&map);

        let prev_session = if created { 0 } else { h.session.load(Ordering::Relaxed) };
        // Continue the index across a writer restart so a reader's cursor stays
        // meaningful; the session bump makes it resync anyway.
        let prev_index = if !created && h.magic.load(Ordering::Acquire) == AUDIO_MAGIC {
            h.write_index.load(Ordering::Relaxed)
        } else {
            0
        };

        h.magic.store(0, Ordering::Release);
        h.version.store(VERSION, Ordering::Relaxed);
        h.header_size.store(size_of::<AudioHeader>() as u32, Ordering::Relaxed);
        h.channels.store(channels, Ordering::Relaxed);
        h.sample_rate.store(AUDIO_SAMPLE_RATE, Ordering::Relaxed);
        h.capacity_frames.store(capacity_frames, Ordering::Relaxed);
        h.data_offset.store(AUDIO_DATA_OFFSET, Ordering::Relaxed);
        h.region_size.store(need, Ordering::Relaxed);
        h.write_index.store(prev_index, Ordering::Relaxed);
        h.writer_pid.store(current_pid(), Ordering::Relaxed);
        h.heartbeat_us.store(now_us(), Ordering::Relaxed);
        h.session.store(prev_session.wrapping_add(1), Ordering::Release);
        h.magic.store(AUDIO_MAGIC, Ordering::Release);

        Ok(Self { map, channels, capacity: capacity_frames as u64, write_index: prev_index })
    }

    pub fn channels(&self) -> u32 {
        self.channels
    }

    /// Append interleaved samples. Length must be a multiple of the channel
    /// count. Never blocks; a burst larger than the ring keeps only its tail.
    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        let ch = self.channels as usize;
        if !samples.len().is_multiple_of(ch) {
            return Err(Error::InvalidFormat("sample count not a multiple of channels"));
        }
        let frames = (samples.len() / ch) as u64;
        let h = header(&self.map);
        let base = samples_ptr(&self.map);

        let (mut src, mut start) = (samples, self.write_index);
        if frames > self.capacity {
            let drop = (frames - self.capacity) as usize;
            src = &samples[drop * ch..];
            start += drop as u64;
        }
        let mut done = 0usize;
        let total = src.len() / ch;
        while done < total {
            let pos = ((start + done as u64) % self.capacity) as usize;
            let run = (self.capacity as usize - pos).min(total - done);
            // SAFETY: pos + run <= capacity, so the range is inside the ring.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    src.as_ptr().add(done * ch),
                    base.add(pos * ch),
                    run * ch,
                );
            }
            done += run;
        }
        self.write_index += frames;
        h.write_index.store(self.write_index, Ordering::Release);
        h.heartbeat_us.store(now_us(), Ordering::Release);
        Ok(())
    }

    /// Call at least every 100 ms while no audio is flowing.
    pub fn heartbeat(&self) {
        header(&self.map).heartbeat_us.store(now_us(), Ordering::Release);
    }

    pub fn write_index(&self) -> u64 {
        self.write_index
    }

    /// Reader cursor as last published by the primary reader. Diagnostic only.
    pub fn reader_index(&self) -> u64 {
        header(&self.map).read_index.load(Ordering::Relaxed)
    }
}

impl Drop for AudioWriter {
    fn drop(&mut self) {
        header(&self.map).heartbeat_us.store(0, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioReadStats {
    /// Frames copied from the ring.
    pub frames: usize,
    /// Frames filled with silence because the ring had nothing newer.
    pub silence: usize,
    /// Frames skipped because this reader fell too far behind.
    pub skipped: u64,
    /// Writer absent or stale; the whole buffer is silence.
    pub no_signal: bool,
}

pub struct AudioReader {
    map: Mapping,
    channels: u32,
    capacity: u64,
    cursor: u64,
    session: u64,
    latency: u64,
}

impl AudioReader {
    /// `latency_frames` is where the reader lands after a skip or resync,
    /// measured back from the write index.
    pub fn open(name: &str, latency_frames: u32) -> Result<Self> {
        let map = shm::open_prefer_rw(name)?;
        if map.len() < AUDIO_DATA_OFFSET as usize {
            return Err(Error::TooSmall { needed: AUDIO_DATA_OFFSET as usize, actual: map.len() });
        }
        let h = header(&map);
        let magic = h.magic.load(Ordering::Acquire);
        if magic == 0 {
            return Err(Error::NotReady);
        }
        if magic != AUDIO_MAGIC {
            return Err(Error::BadMagic(magic));
        }
        let v = h.version.load(Ordering::Relaxed);
        if v != VERSION {
            return Err(Error::VersionMismatch(v));
        }
        let region = h.region_size.load(Ordering::Relaxed) as usize;
        if region > map.len() {
            return Err(Error::TooSmall { needed: region, actual: map.len() });
        }
        let channels = h.channels.load(Ordering::Relaxed);
        let capacity = h.capacity_frames.load(Ordering::Relaxed) as u64;
        let latency = (latency_frames as u64).min(capacity / 2);
        let mut r = Self { map, channels, capacity, cursor: 0, session: 0, latency };
        r.resync();
        Ok(r)
    }

    pub fn channels(&self) -> u32 {
        self.channels
    }

    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    fn resync(&mut self) {
        let h = header(&self.map);
        self.session = h.session.load(Ordering::Acquire);
        // A restarted writer may have chosen a different channel count.
        self.channels = h.channels.load(Ordering::Relaxed).max(1);
        self.capacity = (h.capacity_frames.load(Ordering::Relaxed) as u64).max(1);
        self.latency = self.latency.min(self.capacity / 2);
        let w = h.write_index.load(Ordering::Acquire);
        self.cursor = w.saturating_sub(self.latency);
    }

    pub fn signal_present(&self) -> bool {
        let h = header(&self.map);
        h.magic.load(Ordering::Acquire) == AUDIO_MAGIC
            && !is_stale(h.heartbeat_us.load(Ordering::Acquire), now_us())
    }

    /// Fill `out` (interleaved, length a multiple of channels) with the next
    /// frames, padding with silence on underrun.
    pub fn read(&mut self, out: &mut [f32]) -> AudioReadStats {
        if header(&self.map).session.load(Ordering::Acquire) != self.session {
            self.resync();
        }
        let ch = self.channels as usize;
        let want = out.len() / ch;
        let mut stats = AudioReadStats::default();
        if !self.signal_present() {
            out.fill(0.0);
            stats.silence = want;
            stats.no_signal = true;
            return stats;
        }
        let h = header(&self.map);
        let w = h.write_index.load(Ordering::Acquire);
        if self.cursor > w {
            self.cursor = w;
        }
        // Keep a quarter ring of headroom: data that close to being overwritten
        // would likely be clobbered mid-copy.
        let limit = self.capacity - self.capacity / 4;
        if w - self.cursor > limit {
            let target = w - self.latency;
            stats.skipped = target - self.cursor;
            self.cursor = target;
        }

        let avail = ((w - self.cursor) as usize).min(want);
        let base = samples_ptr(&self.map);
        let mut done = 0usize;
        while done < avail {
            let pos = ((self.cursor + done as u64) % self.capacity) as usize;
            let run = (self.capacity as usize - pos).min(avail - done);
            // SAFETY: pos + run <= capacity. Concurrent writer stores into this
            // range are detected by the write index recheck below.
            unsafe {
                std::ptr::copy_nonoverlapping(base.add(pos * ch), out.as_mut_ptr().add(done * ch), run * ch);
            }
            done += run;
        }
        fence(Ordering::Acquire);
        let w2 = h.write_index.load(Ordering::Relaxed);
        // Frames below w2 - capacity were overwritten while we copied.
        let oldest_valid = w2.saturating_sub(self.capacity);
        if self.cursor < oldest_valid && avail > 0 {
            let target = w2 - self.latency;
            stats.skipped += target - self.cursor;
            self.cursor = target;
            out.fill(0.0);
            stats.silence = want;
            self.publish();
            return stats;
        }

        self.cursor += avail as u64;
        out[avail * ch..].fill(0.0);
        stats.frames = avail;
        stats.silence = want - avail;
        self.publish();
        stats
    }

    fn publish(&self) {
        if self.map.writable() {
            header(&self.map).read_index.store(self.cursor, Ordering::Release);
        }
    }
}
