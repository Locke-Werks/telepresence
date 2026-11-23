/*
 * tp_sink.h: shared-memory layout between the TelePresence Receiver (writer)
 * and the virtual camera / microphone drivers (readers).
 *
 * Mirror of crates/tp-sink/src/layout.rs. The spec is docs/sink-format.md.
 * Any change here bumps TP_SINK_VIDEO_VERSION or TP_SINK_VERSION (audio) and must match the Rust side; the
 * static assertions at the bottom catch drift at compile time on both sides.
 *
 * All fields are little-endian and fixed width. Atomic fields are lock-free
 * 32/64-bit integers with the same size and alignment as the plain type, so
 * the C, C++ and Rust views of one mapping agree byte for byte.
 */
#ifndef TP_SINK_H
#define TP_SINK_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
  /* C++ before C++23 has no <stdatomic.h>; std::atomic of a lock-free
   * integer is standard-layout with the integer's size and alignment. */
  #include <atomic>
  #define TP_ATOMIC_U32 std::atomic<uint32_t>
  #define TP_ATOMIC_U64 std::atomic<uint64_t>
  #define TP_ALIGN64 alignas(64)
  #define TP_STATIC_ASSERT(c, m) static_assert(c, m)
#else
  /* MSVC needs /std:c11 /experimental:c11atomics. */
  #include <stdatomic.h>
  #define TP_ATOMIC_U32 _Atomic uint32_t
  #define TP_ATOMIC_U64 _Atomic uint64_t
  #define TP_ALIGN64 _Alignas(64)
  #define TP_STATIC_ASSERT(c, m) _Static_assert(c, m)
#endif

/* Alignment sits on each struct's first member because C11 does not allow
 * _Alignas on a struct tag; the struct inherits it either way. */

#define TP_SINK_FOURCC(a, b, c, d) \
    ((uint32_t)(a) | ((uint32_t)(b) << 8) | ((uint32_t)(c) << 16) | ((uint32_t)(d) << 24))

#define TP_SINK_VIDEO_MAGIC   TP_SINK_FOURCC('T', 'P', 'S', 'V')
#define TP_SINK_AUDIO_MAGIC   TP_SINK_FOURCC('T', 'P', 'S', 'A')
#define TP_SINK_VERSION       1u  /* audio layout */
/* Video version 2 adds fps_num/fps_den in former reserved space; readers
 * accept 1 and 2, and a version 1 region means frame rate unknown. */
#define TP_SINK_VIDEO_VERSION     2u
#define TP_SINK_VIDEO_VERSION_MIN 1u
#define TP_SINK_FORMAT_NV12   TP_SINK_FOURCC('N', 'V', '1', '2')

#define TP_SINK_VIDEO_SLOTS        3u
#define TP_SINK_NO_SLOT            0xFFFFFFFFu
#define TP_SINK_VIDEO_DATA_OFFSET  4096u
#define TP_SINK_AUDIO_DATA_OFFSET  256u
#define TP_SINK_AUDIO_SAMPLE_RATE  48000u
#define TP_SINK_AUDIO_CAPACITY     24000u
#define TP_SINK_STALE_AFTER_US     500000ull

#define TP_SINK_VIDEO_NAME_WIN  L"Global\\TelePresence.Sink.Video.v1"
#define TP_SINK_AUDIO_NAME_WIN  L"Global\\TelePresence.Sink.Audio.v1"
#define TP_SINK_VIDEO_NAME_POSIX "/tp.sink.video.v1"
#define TP_SINK_AUDIO_NAME_POSIX "/tp.sink.audio.v1"

/* One triple-buffer slot descriptor. seq is a seqlock: odd while the writer
 * fills the slot, even when stable. */
typedef struct tp_video_slot {
    TP_ALIGN64 TP_ATOMIC_U64 seq; /*  0 */
    TP_ATOMIC_U64 frame_counter;  /*  8 */
    TP_ATOMIC_U64 timestamp_us;   /* 16 */
    TP_ATOMIC_U32 generation;     /* 24 */
    TP_ATOMIC_U32 width;          /* 28 */
    TP_ATOMIC_U32 height;         /* 32 */
    TP_ATOMIC_U32 stride;         /* 36 */
    TP_ATOMIC_U32 format;         /* 40 */
    TP_ATOMIC_U32 data_size;      /* 44 */
    uint32_t      reserved[4];    /* 48 */
} tp_video_slot;

typedef struct tp_video_header {
    /* Line 0: layout, fixed once magic is published. */
    TP_ALIGN64 TP_ATOMIC_U32 magic; /*   0 */
    TP_ATOMIC_U32 version;        /*   4 */
    TP_ATOMIC_U32 header_size;    /*   8 */
    TP_ATOMIC_U32 slot_count;     /*  12 */
    TP_ATOMIC_U64 region_size;    /*  16 */
    TP_ATOMIC_U64 data_offset;    /*  24 */
    TP_ATOMIC_U64 slot_capacity;  /*  32 */
    TP_ATOMIC_U32 max_width;      /*  40 */
    TP_ATOMIC_U32 max_height;     /*  44 */
    uint32_t      reserved0[4];   /*  48 */
    /* Line 1: writer state. */
    TP_ATOMIC_U32 generation;     /*  64 */
    TP_ATOMIC_U32 width;          /*  68 */
    TP_ATOMIC_U32 height;         /*  72 */
    TP_ATOMIC_U32 stride;         /*  76 */
    TP_ATOMIC_U32 format;         /*  80 */
    TP_ATOMIC_U32 latest_slot;    /*  84 */
    TP_ATOMIC_U64 frame_counter;  /*  88 */
    TP_ATOMIC_U64 timestamp_us;   /*  96 */
    TP_ATOMIC_U64 heartbeat_us;   /* 104 */
    TP_ATOMIC_U32 writer_pid;     /* 112 */
    TP_ATOMIC_U32 fps_num;        /* 116, 0 when unknown */
    TP_ATOMIC_U32 fps_den;        /* 120 */
    uint32_t      reserved1;      /* 124 */
    /* Lines 2..4. */
    tp_video_slot slots[3];       /* 128 */
} tp_video_header;

typedef struct tp_audio_header {
    /* Line 0: layout. */
    TP_ALIGN64 TP_ATOMIC_U32 magic; /*   0 */
    TP_ATOMIC_U32 version;          /*   4 */
    TP_ATOMIC_U32 header_size;      /*   8 */
    TP_ATOMIC_U32 channels;         /*  12 */
    TP_ATOMIC_U32 sample_rate;      /*  16 */
    TP_ATOMIC_U32 capacity_frames;  /*  20 */
    TP_ATOMIC_U32 data_offset;      /*  24 */
    uint32_t      reserved0a;       /*  28 */
    TP_ATOMIC_U64 region_size;      /*  32 */
    uint32_t      reserved0[6];     /*  40 */
    /* Line 1: writer state. */
    TP_ATOMIC_U64 write_index;      /*  64 */
    TP_ATOMIC_U64 heartbeat_us;     /*  72 */
    TP_ATOMIC_U64 session;          /*  80 */
    TP_ATOMIC_U32 writer_pid;       /*  88 */
    uint32_t      reserved1[9];     /*  92 */
    /* Line 2: reader state. */
    TP_ATOMIC_U64 read_index;       /* 128 */
    uint64_t      reserved2[7];     /* 136 */
} tp_audio_header;

TP_STATIC_ASSERT(sizeof(tp_video_slot) == 64, "tp_video_slot size");
TP_STATIC_ASSERT(offsetof(tp_video_slot, seq) == 0, "slot.seq");
TP_STATIC_ASSERT(offsetof(tp_video_slot, frame_counter) == 8, "slot.frame_counter");
TP_STATIC_ASSERT(offsetof(tp_video_slot, timestamp_us) == 16, "slot.timestamp_us");
TP_STATIC_ASSERT(offsetof(tp_video_slot, generation) == 24, "slot.generation");
TP_STATIC_ASSERT(offsetof(tp_video_slot, width) == 28, "slot.width");
TP_STATIC_ASSERT(offsetof(tp_video_slot, height) == 32, "slot.height");
TP_STATIC_ASSERT(offsetof(tp_video_slot, stride) == 36, "slot.stride");
TP_STATIC_ASSERT(offsetof(tp_video_slot, format) == 40, "slot.format");
TP_STATIC_ASSERT(offsetof(tp_video_slot, data_size) == 44, "slot.data_size");

TP_STATIC_ASSERT(sizeof(tp_video_header) == 320, "tp_video_header size");
TP_STATIC_ASSERT(offsetof(tp_video_header, magic) == 0, "video.magic");
TP_STATIC_ASSERT(offsetof(tp_video_header, version) == 4, "video.version");
TP_STATIC_ASSERT(offsetof(tp_video_header, header_size) == 8, "video.header_size");
TP_STATIC_ASSERT(offsetof(tp_video_header, slot_count) == 12, "video.slot_count");
TP_STATIC_ASSERT(offsetof(tp_video_header, region_size) == 16, "video.region_size");
TP_STATIC_ASSERT(offsetof(tp_video_header, data_offset) == 24, "video.data_offset");
TP_STATIC_ASSERT(offsetof(tp_video_header, slot_capacity) == 32, "video.slot_capacity");
TP_STATIC_ASSERT(offsetof(tp_video_header, max_width) == 40, "video.max_width");
TP_STATIC_ASSERT(offsetof(tp_video_header, max_height) == 44, "video.max_height");
TP_STATIC_ASSERT(offsetof(tp_video_header, generation) == 64, "video.generation");
TP_STATIC_ASSERT(offsetof(tp_video_header, width) == 68, "video.width");
TP_STATIC_ASSERT(offsetof(tp_video_header, height) == 72, "video.height");
TP_STATIC_ASSERT(offsetof(tp_video_header, stride) == 76, "video.stride");
TP_STATIC_ASSERT(offsetof(tp_video_header, format) == 80, "video.format");
TP_STATIC_ASSERT(offsetof(tp_video_header, latest_slot) == 84, "video.latest_slot");
TP_STATIC_ASSERT(offsetof(tp_video_header, frame_counter) == 88, "video.frame_counter");
TP_STATIC_ASSERT(offsetof(tp_video_header, timestamp_us) == 96, "video.timestamp_us");
TP_STATIC_ASSERT(offsetof(tp_video_header, heartbeat_us) == 104, "video.heartbeat_us");
TP_STATIC_ASSERT(offsetof(tp_video_header, writer_pid) == 112, "video.writer_pid");
TP_STATIC_ASSERT(offsetof(tp_video_header, fps_num) == 116, "video.fps_num");
TP_STATIC_ASSERT(offsetof(tp_video_header, fps_den) == 120, "video.fps_den");
TP_STATIC_ASSERT(offsetof(tp_video_header, slots) == 128, "video.slots");

TP_STATIC_ASSERT(sizeof(tp_audio_header) == 192, "tp_audio_header size");
TP_STATIC_ASSERT(offsetof(tp_audio_header, magic) == 0, "audio.magic");
TP_STATIC_ASSERT(offsetof(tp_audio_header, version) == 4, "audio.version");
TP_STATIC_ASSERT(offsetof(tp_audio_header, header_size) == 8, "audio.header_size");
TP_STATIC_ASSERT(offsetof(tp_audio_header, channels) == 12, "audio.channels");
TP_STATIC_ASSERT(offsetof(tp_audio_header, sample_rate) == 16, "audio.sample_rate");
TP_STATIC_ASSERT(offsetof(tp_audio_header, capacity_frames) == 20, "audio.capacity_frames");
TP_STATIC_ASSERT(offsetof(tp_audio_header, data_offset) == 24, "audio.data_offset");
TP_STATIC_ASSERT(offsetof(tp_audio_header, region_size) == 32, "audio.region_size");
TP_STATIC_ASSERT(offsetof(tp_audio_header, write_index) == 64, "audio.write_index");
TP_STATIC_ASSERT(offsetof(tp_audio_header, heartbeat_us) == 72, "audio.heartbeat_us");
TP_STATIC_ASSERT(offsetof(tp_audio_header, session) == 80, "audio.session");
TP_STATIC_ASSERT(offsetof(tp_audio_header, writer_pid) == 88, "audio.writer_pid");
TP_STATIC_ASSERT(offsetof(tp_audio_header, read_index) == 128, "audio.read_index");

/* Pointer to slot i's pixel data. Read only between two seq loads. */
static inline const uint8_t *tp_video_slot_data(const tp_video_header *h, uint32_t i,
                                                uint64_t data_offset, uint64_t slot_capacity)
{
    return (const uint8_t *)h + data_offset + (uint64_t)i * slot_capacity;
}

/* Pointer to the first interleaved f32 sample of the audio ring. */
static inline const float *tp_audio_samples(const tp_audio_header *h)
{
    return (const float *)((const uint8_t *)h + TP_SINK_AUDIO_DATA_OFFSET);
}

#endif /* TP_SINK_H */
