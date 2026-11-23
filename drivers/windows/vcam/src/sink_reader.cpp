#include "sink_reader.h"

#include <sddl.h>

#include <atomic>
#include <cstring>

#include "tp_sink.h"

namespace {

// The spec's descriptor plus read/write for INTERACTIVE. Here the creator
// (OW) is LocalService, not the user, so without the IU entry the Receiver,
// running as a standard user, could only open the section read-only.
constexpr wchar_t kSectionSddl[] =
    L"D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;GRGW;;;LS)(A;;GRGW;;;IU)(A;;GR;;;AU)";

constexpr uint64_t AlignUp(uint64_t v, uint64_t a) { return (v + a - 1) / a * a; }

constexpr uint64_t kSlotCapacity = AlignUp(uint64_t{kSinkMaxWidth} * kSinkMaxHeight * 3 / 2, 4096);
constexpr uint64_t kRegionSize = TP_SINK_VIDEO_DATA_OFFSET + TP_SINK_VIDEO_SLOTS * kSlotCapacity;

constexpr auto kRetryInterval = std::chrono::milliseconds(500);
constexpr int kReadRetries = 8;
constexpr uint32_t kMaxDimension = 8192;

uint64_t NowUs() {
    FILETIME ft;
    GetSystemTimePreciseAsFileTime(&ft);
    const uint64_t t = (uint64_t{ft.dwHighDateTime} << 32) | ft.dwLowDateTime;
    // FILETIME counts 100 ns ticks from 1601; the sink uses Unix microseconds.
    return (t - 116444736000000000ull) / 10;
}

bool Stale(uint64_t heartbeat, uint64_t now) {
    return heartbeat == 0 || (now > heartbeat && now - heartbeat > TP_SINK_STALE_AFTER_US);
}

bool VersionKnown(uint32_t v) { return v >= TP_SINK_VIDEO_VERSION_MIN && v <= TP_SINK_VIDEO_VERSION; }

}  // namespace

SinkReader::~SinkReader() { Close(); }

void SinkReader::Close() {
    if (view_) UnmapViewOfFile(view_);
    if (section_) CloseHandle(section_);
    view_ = nullptr;
    section_ = nullptr;
    view_size_ = 0;
}

void SinkReader::EnsureOpen() {
    if (view_) return;
    const auto now = std::chrono::steady_clock::now();
    if (last_attempt_.time_since_epoch().count() != 0 && now - last_attempt_ < kRetryInterval) return;
    last_attempt_ = now;

    PSECURITY_DESCRIPTOR sd = nullptr;
    if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(kSectionSddl, SDDL_REVISION_1, &sd, nullptr)) {
        return;
    }
    SECURITY_ATTRIBUTES sa{sizeof(sa), sd, FALSE};
    // Opens the existing section if the Receiver (elevated) or an earlier
    // camera instance made it; the size argument is then ignored.
    HANDLE h = CreateFileMappingW(INVALID_HANDLE_VALUE, &sa, PAGE_READWRITE, static_cast<DWORD>(kRegionSize >> 32),
                                  static_cast<DWORD>(kRegionSize), TP_SINK_VIDEO_NAME_WIN);
    LocalFree(sd);
    if (!h) {
        h = OpenFileMappingW(FILE_MAP_READ, FALSE, TP_SINK_VIDEO_NAME_WIN);
        if (!h) return;
    }
    void* v = MapViewOfFile(h, FILE_MAP_READ, 0, 0, 0);
    if (!v) {
        CloseHandle(h);
        return;
    }
    MEMORY_BASIC_INFORMATION mbi{};
    if (!VirtualQuery(v, &mbi, sizeof(mbi)) || mbi.RegionSize < TP_SINK_VIDEO_DATA_OFFSET) {
        UnmapViewOfFile(v);
        CloseHandle(h);
        return;
    }
    section_ = h;
    view_ = static_cast<const uint8_t*>(v);
    view_size_ = mbi.RegionSize;
}

bool SinkReader::PeekWriterFormat(WriterFormat* out) {
    EnsureOpen();
    if (!view_ || !out) return false;
    const auto* hdr = reinterpret_cast<const tp_video_header*>(view_);
    if (hdr->magic.load(std::memory_order_acquire) != TP_SINK_VIDEO_MAGIC) return false;
    const uint32_t version = hdr->version.load(std::memory_order_relaxed);
    if (!VersionKnown(version)) return false;
    if (Stale(hdr->heartbeat_us.load(std::memory_order_acquire), NowUs())) return false;
    WriterFormat f;
    f.width = hdr->width.load(std::memory_order_relaxed);
    f.height = hdr->height.load(std::memory_order_relaxed);
    if (!f.width || !f.height || (f.width & 1) || (f.height & 1) || f.width > kMaxDimension ||
        f.height > kMaxDimension) {
        return false;
    }
    if (version >= 2) {
        f.fps_num = hdr->fps_num.load(std::memory_order_relaxed);
        f.fps_den = hdr->fps_den.load(std::memory_order_relaxed);
        // A rate MF cannot pace sensibly is treated as unknown.
        if (!f.fps_num || !f.fps_den || f.fps_num / f.fps_den > 240) f.fps_num = f.fps_den = 0;
    }
    *out = f;
    return true;
}

SinkReader::State SinkReader::Poll() {
    EnsureOpen();
    if (!view_) return State::NoSignal;

    const auto* hdr = reinterpret_cast<const tp_video_header*>(view_);
    if (hdr->magic.load(std::memory_order_acquire) != TP_SINK_VIDEO_MAGIC) return State::NoSignal;
    if (!VersionKnown(hdr->version.load(std::memory_order_relaxed))) return State::NoSignal;
    if (Stale(hdr->heartbeat_us.load(std::memory_order_acquire), NowUs())) return State::NoSignal;

    const uint64_t region = hdr->region_size.load(std::memory_order_relaxed);
    const uint64_t capacity = hdr->slot_capacity.load(std::memory_order_relaxed);
    const uint64_t data_offset = hdr->data_offset.load(std::memory_order_relaxed);
    if (region > view_size_) return State::NoSignal;

    for (int attempt = 0; attempt < kReadRetries; ++attempt) {
        const uint32_t idx = hdr->latest_slot.load(std::memory_order_acquire);
        if (idx >= TP_SINK_VIDEO_SLOTS) return State::NoSignal;
        const tp_video_slot& slot = hdr->slots[idx];

        const uint64_t s1 = slot.seq.load(std::memory_order_acquire);
        if (s1 & 1) {
            YieldProcessor();
            continue;
        }
        const uint64_t counter = slot.frame_counter.load(std::memory_order_relaxed);
        if (counter == frame_.frame_counter && !frame_.data.empty()) {
            // Nothing new; skip the multi-megabyte copy.
            if (slot.seq.load(std::memory_order_relaxed) == s1) return State::SameFrame;
            continue;
        }
        const uint32_t w = slot.width.load(std::memory_order_relaxed);
        const uint32_t h = slot.height.load(std::memory_order_relaxed);
        const uint32_t stride = slot.stride.load(std::memory_order_relaxed);
        const uint32_t format = slot.format.load(std::memory_order_relaxed);
        const uint64_t size = slot.data_size.load(std::memory_order_relaxed);
        const uint64_t off = data_offset + idx * capacity;

        // Fields read while the writer is mid-update can be anything, so
        // bound every one before using it; the seq recheck discards the rest.
        const bool sane = format == TP_SINK_FORMAT_NV12 && w && h && w <= kMaxDimension && h <= kMaxDimension &&
                          !(w & 1) && !(h & 1) && stride >= w && size == uint64_t{stride} * h * 3 / 2 &&
                          size <= capacity && off + size <= view_size_;
        if (!sane) continue;

        scratch_.resize(static_cast<size_t>(size));
        std::memcpy(scratch_.data(), view_ + off, static_cast<size_t>(size));
        std::atomic_thread_fence(std::memory_order_acquire);
        if (slot.seq.load(std::memory_order_relaxed) != s1) continue;

        frame_.data.swap(scratch_);
        frame_.width = w;
        frame_.height = h;
        frame_.stride = stride;
        frame_.frame_counter = counter;
        return State::NewFrame;
    }
    // Lapped on every retry; the caller repeats the previous frame.
    return frame_.data.empty() ? State::NoSignal : State::SameFrame;
}
