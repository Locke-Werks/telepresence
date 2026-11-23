#pragma once

#include <windows.h>

#include <chrono>
#include <cstdint>
#include <vector>

// One validated NV12 frame copied out of the shared region.
struct SinkFrame {
    std::vector<uint8_t> data;
    uint32_t width = 0;
    uint32_t height = 0;
    uint32_t stride = 0;
    uint64_t frame_counter = 0;
};

// Reader side of the tp-sink video region (docs/sink-format.md).
//
// Unlike the generic reader this one also creates the section: it runs as
// LocalService inside FrameServer, which holds SeCreateGlobalPrivilege, while
// the Receiver usually runs as a standard user that does not. The section is
// left zero-filled; the Receiver initialises the header when it attaches.
class SinkReader {
public:
    enum class State { NoSignal, NewFrame, SameFrame };

    SinkReader() = default;
    ~SinkReader();
    SinkReader(const SinkReader&) = delete;
    SinkReader& operator=(const SinkReader&) = delete;

    // Create or open the section. Cheap to call often; retries are throttled.
    void EnsureOpen();

    // Pick up the newest frame. NewFrame refreshes frame(); SameFrame means the
    // writer is live but has published nothing since the last call.
    State Poll();
    const SinkFrame& frame() const { return frame_; }

    // The live writer's current geometry and frame rate from the header, for
    // offering as the native media type. False with no live writer or before
    // it has set a format. fps_num/fps_den are 0 when the writer did not say.
    struct WriterFormat {
        uint32_t width = 0, height = 0, fps_num = 0, fps_den = 0;
        bool operator==(const WriterFormat&) const = default;
    };
    bool PeekWriterFormat(WriterFormat* out);

private:
    void Close();

    HANDLE section_ = nullptr;
    const uint8_t* view_ = nullptr;
    size_t view_size_ = 0;
    std::chrono::steady_clock::time_point last_attempt_{};
    SinkFrame frame_;
    std::vector<uint8_t> scratch_;
};

// Largest frame the section is sized for, as bytes: any orientation with no
// more pixels than 3840x2160 fits. The camera offers the writer's own size as
// its native type, so this ceiling is what a sender may deliver unscaled.
inline constexpr uint32_t kSinkMaxWidth = 3840;
inline constexpr uint32_t kSinkMaxHeight = 2160;
