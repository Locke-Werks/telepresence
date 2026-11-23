//! Latest-frame video channel: a triple buffer where each slot is a seqlock.
//!
//! The writer never waits for readers. It always writes into the slot after
//! the one last published, so a reader that just picked up the latest slot has
//! a full frame interval before that slot comes round again. If a reader is
//! slow enough to be lapped anyway, the slot sequence number changes under it
//! and it retries instead of returning a torn frame.

use std::sync::atomic::{Ordering, fence};
use std::time::{Duration, Instant};

use crate::layout::*;
use crate::shm::{self, Mapping};
use crate::{Error, Result, current_pid, is_stale, now_us};

fn header(map: &Mapping) -> &VideoHeader {
    // SAFETY: the mapping is page aligned and at least VIDEO_DATA_OFFSET long
    // (checked at open/create), and VideoHeader is all atomics.
    unsafe { &*(map.ptr() as *const VideoHeader) }
}

fn check_dims(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(Error::InvalidFormat("zero dimension"));
    }
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(Error::InvalidFormat("NV12 needs even width and height"));
    }
    Ok(())
}

pub struct VideoWriter {
    map: Mapping,
    capacity: u64,
    data_offset: u64,
    max_width: u32,
    max_height: u32,
    width: u32,
    height: u32,
    stride: u32,
    generation: u32,
    counter: u64,
}

impl VideoWriter {
    /// Create the region, or adopt one left by a previous writer that readers
    /// still have mapped. Frames up to `max_width` x `max_height` fit without
    /// recreating the object, which matters on Windows where a section held
    /// open by FrameServer cannot be replaced.
    pub fn create(name: &str, max_width: u32, max_height: u32) -> Result<Self> {
        check_dims(max_width, max_height)?;
        let need = video_region_size(max_width, max_height);
        let (map, created) = shm::create(name, need as usize)?;
        Self::init(map, created, max_width, max_height)
    }

    /// Open a section somebody else created and become its writer. On Windows
    /// the virtual camera, running as LocalService inside FrameServer, creates
    /// `Global\TelePresence.Sink.Video.v1` zero-filled because a standard user
    /// cannot create Global objects. A zeroed header is initialised here; a
    /// valid one left by an earlier writer is adopted.
    pub fn open_existing(name: &str, max_width: u32, max_height: u32) -> Result<Self> {
        check_dims(max_width, max_height)?;
        let map = shm::open(name, true)?;
        let need = video_region_size(max_width, max_height) as usize;
        if map.len() < need {
            return Err(Error::TooSmall { needed: need, actual: map.len() });
        }
        Self::init(map, false, max_width, max_height)
    }

    /// What the Receiver should call. Tries, in order: open the section the
    /// camera created; create `name` (works when elevated, or for any Local
    /// name); and, when `name` is in the Global namespace and creating it is
    /// refused, the same name under `Local\`. The last one is only visible in
    /// this logon session, so it serves tests and development, not the camera.
    pub fn open_or_create(name: &str, max_width: u32, max_height: u32) -> Result<Self> {
        match Self::open_existing(name, max_width, max_height) {
            Ok(w) => return Ok(w),
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        match Self::create(name, max_width, max_height) {
            Ok(w) => Ok(w),
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                match name.strip_prefix(r"Global\") {
                    Some(rest) => Self::create(&format!(r"Local\{rest}"), max_width, max_height),
                    None => Err(Error::Io(e)),
                }
            }
            Err(e) => Err(e),
        }
    }

    fn init(map: Mapping, created: bool, max_width: u32, max_height: u32) -> Result<Self> {
        let need = video_region_size(max_width, max_height);
        let h = header(&map);

        let adopt = !created
            && h.magic.load(Ordering::Acquire) == VIDEO_MAGIC
            && (VIDEO_VERSION_MIN..=VIDEO_VERSION).contains(&h.version.load(Ordering::Relaxed))
            && h.data_offset.load(Ordering::Relaxed) == VIDEO_DATA_OFFSET
            && h.slot_capacity.load(Ordering::Relaxed) >= video_slot_capacity(max_width, max_height)
            && h.region_size.load(Ordering::Relaxed) <= map.len() as u64;

        let prev_gen = if created { 0 } else { h.generation.load(Ordering::Relaxed) };
        let prev_counter = if adopt { h.frame_counter.load(Ordering::Relaxed) } else { 0 };

        if !adopt {
            // Hide the region from readers while the layout is rewritten.
            h.magic.store(0, Ordering::Release);
            h.version.store(VIDEO_VERSION, Ordering::Relaxed);
            h.header_size.store(size_of::<VideoHeader>() as u32, Ordering::Relaxed);
            h.slot_count.store(VIDEO_SLOTS as u32, Ordering::Relaxed);
            h.region_size.store(need, Ordering::Relaxed);
            h.data_offset.store(VIDEO_DATA_OFFSET, Ordering::Relaxed);
            h.slot_capacity.store(video_slot_capacity(max_width, max_height), Ordering::Relaxed);
            h.max_width.store(max_width, Ordering::Relaxed);
            h.max_height.store(max_height, Ordering::Relaxed);
            for s in &h.slots {
                // Keep seq even and monotonic so a reader mid-copy still fails
                // its recheck rather than matching a reset value.
                let seq = s.seq.load(Ordering::Relaxed);
                s.seq.store((seq | 1) + 1, Ordering::Relaxed);
                s.data_size.store(0, Ordering::Relaxed);
            }
        }

        // Version 2 only fills space that version 1 left zero, so an adopted
        // version 1 region is upgraded in place. Re-laying it out instead would
        // move the slots under a version 1 writer still attached to it.
        h.version.store(VIDEO_VERSION, Ordering::Relaxed);

        let generation = prev_gen.wrapping_add(1).max(1);
        h.latest_slot.store(NO_SLOT, Ordering::Relaxed);
        h.width.store(0, Ordering::Relaxed);
        h.height.store(0, Ordering::Relaxed);
        h.stride.store(0, Ordering::Relaxed);
        h.format.store(FORMAT_NV12, Ordering::Relaxed);
        h.writer_pid.store(current_pid(), Ordering::Relaxed);
        h.fps_num.store(0, Ordering::Relaxed);
        h.fps_den.store(0, Ordering::Relaxed);
        h.heartbeat_us.store(now_us(), Ordering::Relaxed);
        h.generation.store(generation, Ordering::Release);
        h.magic.store(VIDEO_MAGIC, Ordering::Release);

        let capacity = h.slot_capacity.load(Ordering::Relaxed);
        let (max_width, max_height) =
            (h.max_width.load(Ordering::Relaxed), h.max_height.load(Ordering::Relaxed));
        Ok(Self {
            map,
            capacity,
            data_offset: VIDEO_DATA_OFFSET,
            max_width,
            max_height,
            width: 0,
            height: 0,
            stride: 0,
            generation,
            counter: prev_counter,
        })
    }

    /// Change the frame geometry. Bumps the generation so readers reallocate.
    pub fn set_format(&mut self, width: u32, height: u32) -> Result<()> {
        self.set_format_with_stride(width, height, width)
    }

    pub fn set_format_with_stride(&mut self, width: u32, height: u32, stride: u32) -> Result<()> {
        check_dims(width, height)?;
        if stride < width {
            return Err(Error::InvalidFormat("stride smaller than width"));
        }
        if nv12_size(stride, height) > self.capacity {
            return Err(Error::CapacityExceeded);
        }
        if (width, height, stride) == (self.width, self.height, self.stride) {
            return Ok(());
        }
        self.width = width;
        self.height = height;
        self.stride = stride;
        self.generation = self.generation.wrapping_add(1).max(1);
        let h = header(&self.map);
        h.width.store(width, Ordering::Relaxed);
        h.height.store(height, Ordering::Relaxed);
        h.stride.store(stride, Ordering::Relaxed);
        h.generation.store(self.generation, Ordering::Release);
        Ok(())
    }

    /// Record the source's frame rate as `num / den` frames per second, so the
    /// camera can offer it as the native media type. Pass 0/0 for unknown.
    /// Informational only: it does not bump the generation, and the camera
    /// reads it when an app opens the device, not per frame.
    pub fn set_frame_rate(&mut self, num: u32, den: u32) -> Result<()> {
        if (num == 0) != (den == 0) {
            return Err(Error::InvalidFormat("frame rate needs both parts or neither"));
        }
        let h = header(&self.map);
        h.fps_den.store(den, Ordering::Relaxed);
        h.fps_num.store(num, Ordering::Relaxed);
        Ok(())
    }

    pub fn max_dimensions(&self) -> (u32, u32) {
        (self.max_width, self.max_height)
    }

    pub fn frame_size(&self) -> usize {
        nv12_size(self.stride, self.height) as usize
    }

    /// Publish one NV12 frame. `data` must be exactly [`frame_size`](Self::frame_size) bytes.
    pub fn write_frame(&mut self, data: &[u8], timestamp_us: u64) -> Result<u64> {
        if data.len() != self.frame_size() {
            return Err(Error::InvalidFormat("frame length does not match format"));
        }
        self.write_frame_with(timestamp_us, |dst| dst.copy_from_slice(data))
    }

    /// Publish a frame filled in place, which saves a copy when the decoder can
    /// write straight into shared memory.
    pub fn write_frame_with(&mut self, timestamp_us: u64, fill: impl FnOnce(&mut [u8])) -> Result<u64> {
        if self.width == 0 {
            return Err(Error::InvalidFormat("set_format not called"));
        }
        let size = self.frame_size();
        let h = header(&self.map);
        let latest = h.latest_slot.load(Ordering::Relaxed);
        let idx = if latest == NO_SLOT { 0 } else { (latest as usize + 1) % VIDEO_SLOTS };
        let slot = &h.slots[idx];

        let seq = slot.seq.load(Ordering::Relaxed);
        let begin = (seq | 1).wrapping_add(if seq & 1 == 1 { 2 } else { 0 });
        slot.seq.store(begin, Ordering::Relaxed);
        // Make the odd sequence visible before any of the payload stores.
        fence(Ordering::Release);

        self.counter += 1;
        slot.frame_counter.store(self.counter, Ordering::Relaxed);
        slot.timestamp_us.store(timestamp_us, Ordering::Relaxed);
        slot.generation.store(self.generation, Ordering::Relaxed);
        slot.width.store(self.width, Ordering::Relaxed);
        slot.height.store(self.height, Ordering::Relaxed);
        slot.stride.store(self.stride, Ordering::Relaxed);
        slot.format.store(FORMAT_NV12, Ordering::Relaxed);
        slot.data_size.store(size as u32, Ordering::Relaxed);

        let off = self.data_offset as usize + idx * self.capacity as usize;
        // SAFETY: off + size is within the mapping (capacity checked in
        // set_format, layout checked at create). Readers only touch this memory
        // through raw copies validated by the slot sequence.
        let dst = unsafe { std::slice::from_raw_parts_mut(self.map.ptr().add(off), size) };
        fill(dst);

        slot.seq.store(begin.wrapping_add(1), Ordering::Release);
        h.latest_slot.store(idx as u32, Ordering::Release);
        h.frame_counter.store(self.counter, Ordering::Relaxed);
        h.timestamp_us.store(timestamp_us, Ordering::Relaxed);
        h.heartbeat_us.store(now_us(), Ordering::Release);
        Ok(self.counter)
    }

    /// Keep the region marked live while no frames are flowing (paused stream).
    /// Call at least every 100 ms.
    pub fn heartbeat(&self) {
        header(&self.map).heartbeat_us.store(now_us(), Ordering::Release);
    }

    /// Tell readers to show "no signal" immediately instead of waiting for the
    /// heartbeat to go stale.
    pub fn clear(&self) {
        let h = header(&self.map);
        h.latest_slot.store(NO_SLOT, Ordering::Release);
        h.heartbeat_us.store(0, Ordering::Release);
    }
}

impl Drop for VideoWriter {
    fn drop(&mut self) {
        self.clear();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameInfo {
    pub frame_counter: u64,
    pub timestamp_us: u64,
    pub generation: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
    /// True the first time a frame from a new generation is returned.
    pub format_changed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoRead {
    /// Writer absent, stale, or has not produced a frame. Show black.
    NoSignal,
    Frame(FrameInfo),
    /// The writer lapped this reader on every retry. Keep showing the last
    /// frame and try again next tick.
    Busy,
}

pub struct VideoReader {
    name: String,
    map: Mapping,
    generation: u32,
    last_reopen: Instant,
}

const READ_RETRIES: usize = 8;
const REOPEN_INTERVAL: Duration = Duration::from_millis(500);

fn validate(map: &Mapping) -> Result<()> {
    if map.len() < VIDEO_DATA_OFFSET as usize {
        return Err(Error::TooSmall { needed: VIDEO_DATA_OFFSET as usize, actual: map.len() });
    }
    let h = header(map);
    let magic = h.magic.load(Ordering::Acquire);
    if magic == 0 {
        return Err(Error::NotReady);
    }
    if magic != VIDEO_MAGIC {
        return Err(Error::BadMagic(magic));
    }
    let v = h.version.load(Ordering::Relaxed);
    if !(VIDEO_VERSION_MIN..=VIDEO_VERSION).contains(&v) {
        return Err(Error::VersionMismatch(v));
    }
    let region = h.region_size.load(Ordering::Relaxed) as usize;
    if region > map.len() {
        return Err(Error::TooSmall { needed: region, actual: map.len() });
    }
    Ok(())
}

impl VideoReader {
    pub fn open(name: &str) -> Result<Self> {
        let map = shm::open(name, false)?;
        validate(&map)?;
        Ok(Self { name: name.to_owned(), map, generation: 0, last_reopen: Instant::now() })
    }

    pub fn signal_present(&self) -> bool {
        let h = header(&self.map);
        h.magic.load(Ordering::Acquire) == VIDEO_MAGIC
            && !is_stale(h.heartbeat_us.load(Ordering::Acquire), now_us())
    }

    /// The writer's current geometry and frame rate from the header, or None
    /// when no live writer has set a format. `fps` is (0, 0) when unknown.
    pub fn writer_format(&self) -> Option<(u32, u32, (u32, u32))> {
        if !self.signal_present() {
            return None;
        }
        let h = header(&self.map);
        let (w, ht) = (h.width.load(Ordering::Relaxed), h.height.load(Ordering::Relaxed));
        if w == 0 || ht == 0 {
            return None;
        }
        let fps = if h.version.load(Ordering::Relaxed) >= 2 {
            (h.fps_num.load(Ordering::Relaxed), h.fps_den.load(Ordering::Relaxed))
        } else {
            (0, 0)
        };
        Some((w, ht, fps))
    }

    /// A writer that had to replace the object (POSIX, larger capacity) leaves
    /// this reader on an orphan whose heartbeat never moves again.
    fn maybe_reopen(&mut self) {
        if self.last_reopen.elapsed() < REOPEN_INTERVAL {
            return;
        }
        self.last_reopen = Instant::now();
        if let Ok(m) = shm::open(&self.name, false)
            && validate(&m).is_ok() {
                self.map = m;
                self.generation = 0;
            }
    }

    /// Copy the most recent complete frame into `out`, resizing it to fit.
    pub fn read_latest(&mut self, out: &mut Vec<u8>) -> VideoRead {
        if !self.signal_present() {
            self.maybe_reopen();
            if !self.signal_present() {
                return VideoRead::NoSignal;
            }
        }
        let h = header(&self.map);
        let capacity = h.slot_capacity.load(Ordering::Relaxed);
        let data_offset = h.data_offset.load(Ordering::Relaxed);
        let region = self.map.len() as u64;

        for _ in 0..READ_RETRIES {
            let idx = h.latest_slot.load(Ordering::Acquire);
            if idx == NO_SLOT {
                return VideoRead::NoSignal;
            }
            if idx as usize >= VIDEO_SLOTS {
                return VideoRead::NoSignal;
            }
            let slot = &h.slots[idx as usize];
            let s1 = slot.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let info = FrameInfo {
                frame_counter: slot.frame_counter.load(Ordering::Relaxed),
                timestamp_us: slot.timestamp_us.load(Ordering::Relaxed),
                generation: slot.generation.load(Ordering::Relaxed),
                width: slot.width.load(Ordering::Relaxed),
                height: slot.height.load(Ordering::Relaxed),
                stride: slot.stride.load(Ordering::Relaxed),
                format: slot.format.load(Ordering::Relaxed),
                format_changed: false,
            };
            let size = slot.data_size.load(Ordering::Relaxed) as u64;
            let off = data_offset + idx as u64 * capacity;
            // Values read mid-write may be garbage; bound them before copying.
            if size > capacity || off + size > region {
                continue;
            }
            out.resize(size as usize, 0);
            // SAFETY: bounds checked above. The writer may be storing into this
            // range concurrently; the sequence recheck discards such a copy.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.map.ptr().add(off as usize),
                    out.as_mut_ptr(),
                    size as usize,
                );
            }
            fence(Ordering::Acquire);
            let s2 = slot.seq.load(Ordering::Relaxed);
            if s1 != s2 {
                continue;
            }
            let format_changed = info.generation != self.generation;
            self.generation = info.generation;
            return VideoRead::Frame(FrameInfo { format_changed, ..info });
        }
        VideoRead::Busy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(kind: &str) -> String {
        #[cfg(windows)]
        return format!(r"Local\tp-sink-unit-{kind}-{}", std::process::id());
        #[cfg(unix)]
        return format!("/tp-sink-unit-{kind}-{}", std::process::id());
    }

    #[test]
    fn writer_initialises_zeroed_section_made_by_reader_side() {
        let n = name("zeroed");
        // Stands in for the camera creating the section before any writer exists.
        let (held, created) = shm::create(&n, video_region_size(320, 240) as usize).unwrap();
        assert!(created);
        assert!(matches!(VideoReader::open(&n), Err(Error::NotReady)));

        let mut w = VideoWriter::open_existing(&n, 320, 240).unwrap();
        w.set_format(32, 24).unwrap();
        let f = vec![9u8; w.frame_size()];
        w.write_frame(&f, 5).unwrap();
        let mut r = VideoReader::open(&n).unwrap();
        let mut buf = Vec::new();
        let VideoRead::Frame(info) = r.read_latest(&mut buf) else { panic!("no frame") };
        assert_eq!((info.width, info.height, info.timestamp_us), (32, 24, 5));
        assert!(buf.iter().all(|&b| b == 9));

        assert!(matches!(
            VideoWriter::open_existing(&n, 1920, 1080),
            Err(Error::TooSmall { .. })
        ));
        drop((w, r, held));
        let _ = shm::unlink(&n);
    }

    #[test]
    fn frame_rate_and_format_reach_reader() {
        let n = name("fps");
        let mut w = VideoWriter::create(&n, 64, 64).unwrap();
        let r = VideoReader::open(&n).unwrap();
        assert_eq!(r.writer_format(), None);
        w.set_format(36, 64).unwrap();
        assert_eq!(r.writer_format(), Some((36, 64, (0, 0))));
        w.set_frame_rate(30000, 1001).unwrap();
        assert_eq!(r.writer_format(), Some((36, 64, (30000, 1001))));
        assert!(w.set_frame_rate(30, 0).is_err());
        drop((w, r));
        let _ = shm::unlink(&n);
    }

    #[test]
    fn reader_accepts_version_1_region() {
        let n = name("v1");
        let mut w = VideoWriter::create(&n, 64, 64).unwrap();
        w.set_format(32, 32).unwrap();
        w.set_frame_rate(25, 1).unwrap();
        header(&w.map).version.store(1, Ordering::Relaxed);
        let r = VideoReader::open(&n).unwrap();
        assert_eq!(r.writer_format(), Some((32, 32, (0, 0))));
        drop((w, r));
        let _ = shm::unlink(&n);
    }

    #[test]
    fn open_or_create_creates_when_missing() {
        let n = name("ooc");
        assert!(matches!(VideoWriter::open_existing(&n, 64, 48), Err(Error::Io(_))));
        let w = VideoWriter::open_or_create(&n, 64, 48).unwrap();
        assert!(VideoReader::open(&n).is_ok());
        drop(w);
        let _ = shm::unlink(&n);
    }
}
