//! Wire layout shared with `drivers/common/tp_sink.h`.
//!
//! Every field is fixed width and little-endian (all supported targets are
//! little-endian; the assertion below refuses to build otherwise). Fields the
//! writer mutates after publication are atomics. Fields set once at creation
//! are also atomics on the Rust side so a writer re-initialising a region that
//! readers still hold mapped is not a data race; in C they may be read as plain
//! integers after the magic has been observed with acquire ordering.
//!
//! Writer-owned and reader-owned state sit on separate 64-byte lines so the
//! drivers polling their own cursor do not bounce the writer's cache line.

use std::mem::{offset_of, size_of};
use std::sync::atomic::{AtomicU32, AtomicU64};

pub const VIDEO_MAGIC: u32 = u32::from_le_bytes(*b"TPSV");
pub const AUDIO_MAGIC: u32 = u32::from_le_bytes(*b"TPSA");
/// Audio layout version.
pub const VERSION: u32 = 1;
/// Video layout version. Version 2 adds the writer's frame rate in what was
/// reserved space on line 1. Readers accept 1 and 2; a version 1 region reads
/// as frame rate unknown.
pub const VIDEO_VERSION: u32 = 2;
pub const VIDEO_VERSION_MIN: u32 = 1;

/// FourCC, matching MFVideoFormat_NV12 and kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange.
pub const FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");

pub const VIDEO_SLOTS: usize = 3;
pub const NO_SLOT: u32 = u32::MAX;

/// Pixel data starts on its own page so drivers can hand slot buffers to APIs
/// that want page-aligned memory.
pub const VIDEO_DATA_OFFSET: u64 = 4096;
pub const AUDIO_DATA_OFFSET: u32 = 256;

pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
/// 500 ms. Long enough to absorb a scheduling hiccup in the app, short enough
/// that a reader that skips ahead lands near live.
pub const AUDIO_CAPACITY_FRAMES: u32 = 24_000;

/// Per-slot descriptor, guarded by `seq` (even = stable, odd = being written).
#[repr(C, align(64))]
pub struct VideoSlot {
    pub seq: AtomicU64,
    pub frame_counter: AtomicU64,
    pub timestamp_us: AtomicU64,
    pub generation: AtomicU32,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub stride: AtomicU32,
    pub format: AtomicU32,
    pub data_size: AtomicU32,
    pub _reserved: [u32; 4],
}

#[repr(C, align(64))]
pub struct VideoHeader {
    // Line 0: layout, fixed for the life of the object.
    pub magic: AtomicU32,
    pub version: AtomicU32,
    pub header_size: AtomicU32,
    pub slot_count: AtomicU32,
    pub region_size: AtomicU64,
    pub data_offset: AtomicU64,
    pub slot_capacity: AtomicU64,
    pub max_width: AtomicU32,
    pub max_height: AtomicU32,
    pub _reserved0: [u32; 4],
    // Line 1: writer state.
    pub generation: AtomicU32,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub stride: AtomicU32,
    pub format: AtomicU32,
    pub latest_slot: AtomicU32,
    pub frame_counter: AtomicU64,
    pub timestamp_us: AtomicU64,
    pub heartbeat_us: AtomicU64,
    pub writer_pid: AtomicU32,
    /// Source frame rate as a ratio; 0/0 when unknown (or a version 1 writer).
    pub fps_num: AtomicU32,
    pub fps_den: AtomicU32,
    pub _reserved1: u32,
    // Lines 2..4.
    pub slots: [VideoSlot; VIDEO_SLOTS],
}

#[repr(C, align(64))]
pub struct AudioHeader {
    // Line 0: layout.
    pub magic: AtomicU32,
    pub version: AtomicU32,
    pub header_size: AtomicU32,
    pub channels: AtomicU32,
    pub sample_rate: AtomicU32,
    pub capacity_frames: AtomicU32,
    pub data_offset: AtomicU32,
    pub _reserved0a: u32,
    pub region_size: AtomicU64,
    pub _reserved0: [u32; 6],
    // Line 1: writer state.
    pub write_index: AtomicU64,
    pub heartbeat_us: AtomicU64,
    pub session: AtomicU64,
    pub writer_pid: AtomicU32,
    pub _reserved1: [u32; 9],
    // Line 2: reader state.
    pub read_index: AtomicU64,
    pub _reserved2: [u64; 7],
}

const _: () = {
    assert!(cfg!(target_endian = "little"));

    assert!(size_of::<VideoSlot>() == 64);
    assert!(offset_of!(VideoSlot, seq) == 0);
    assert!(offset_of!(VideoSlot, frame_counter) == 8);
    assert!(offset_of!(VideoSlot, timestamp_us) == 16);
    assert!(offset_of!(VideoSlot, generation) == 24);
    assert!(offset_of!(VideoSlot, width) == 28);
    assert!(offset_of!(VideoSlot, height) == 32);
    assert!(offset_of!(VideoSlot, stride) == 36);
    assert!(offset_of!(VideoSlot, format) == 40);
    assert!(offset_of!(VideoSlot, data_size) == 44);

    assert!(size_of::<VideoHeader>() == 320);
    assert!(offset_of!(VideoHeader, magic) == 0);
    assert!(offset_of!(VideoHeader, version) == 4);
    assert!(offset_of!(VideoHeader, header_size) == 8);
    assert!(offset_of!(VideoHeader, slot_count) == 12);
    assert!(offset_of!(VideoHeader, region_size) == 16);
    assert!(offset_of!(VideoHeader, data_offset) == 24);
    assert!(offset_of!(VideoHeader, slot_capacity) == 32);
    assert!(offset_of!(VideoHeader, max_width) == 40);
    assert!(offset_of!(VideoHeader, max_height) == 44);
    assert!(offset_of!(VideoHeader, generation) == 64);
    assert!(offset_of!(VideoHeader, width) == 68);
    assert!(offset_of!(VideoHeader, height) == 72);
    assert!(offset_of!(VideoHeader, stride) == 76);
    assert!(offset_of!(VideoHeader, format) == 80);
    assert!(offset_of!(VideoHeader, latest_slot) == 84);
    assert!(offset_of!(VideoHeader, frame_counter) == 88);
    assert!(offset_of!(VideoHeader, timestamp_us) == 96);
    assert!(offset_of!(VideoHeader, heartbeat_us) == 104);
    assert!(offset_of!(VideoHeader, writer_pid) == 112);
    assert!(offset_of!(VideoHeader, fps_num) == 116);
    assert!(offset_of!(VideoHeader, fps_den) == 120);
    assert!(offset_of!(VideoHeader, slots) == 128);
    assert!((size_of::<VideoHeader>() as u64) <= VIDEO_DATA_OFFSET);

    assert!(size_of::<AudioHeader>() == 192);
    assert!(offset_of!(AudioHeader, magic) == 0);
    assert!(offset_of!(AudioHeader, version) == 4);
    assert!(offset_of!(AudioHeader, header_size) == 8);
    assert!(offset_of!(AudioHeader, channels) == 12);
    assert!(offset_of!(AudioHeader, sample_rate) == 16);
    assert!(offset_of!(AudioHeader, capacity_frames) == 20);
    assert!(offset_of!(AudioHeader, data_offset) == 24);
    assert!(offset_of!(AudioHeader, region_size) == 32);
    assert!(offset_of!(AudioHeader, write_index) == 64);
    assert!(offset_of!(AudioHeader, heartbeat_us) == 72);
    assert!(offset_of!(AudioHeader, session) == 80);
    assert!(offset_of!(AudioHeader, writer_pid) == 88);
    assert!(offset_of!(AudioHeader, read_index) == 128);
    assert!(size_of::<AudioHeader>() <= AUDIO_DATA_OFFSET as usize);
};

/// Bytes of one NV12 image: a full-resolution Y plane plus a half-height
/// interleaved CbCr plane, both at `stride`.
pub const fn nv12_size(stride: u32, height: u32) -> u64 {
    stride as u64 * height as u64 + stride as u64 * (height as u64 / 2)
}

pub const fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

pub const fn video_slot_capacity(max_width: u32, max_height: u32) -> u64 {
    align_up(nv12_size(max_width, max_height), 4096)
}

pub const fn video_region_size(max_width: u32, max_height: u32) -> u64 {
    VIDEO_DATA_OFFSET + VIDEO_SLOTS as u64 * video_slot_capacity(max_width, max_height)
}

pub const fn audio_region_size(channels: u32, capacity_frames: u32) -> u64 {
    AUDIO_DATA_OFFSET as u64 + channels as u64 * capacity_frames as u64 * 4
}
