#pragma once

// Hands decoded NV12 frames to the QVideoSink behind the QML VideoOutput.
// Plain functions rather than a QObject so the Rust decode callback can call
// them from a GStreamer streaming thread without owning any Qt state.

#include <cstddef>
#include <cstdint>

#include <QtCore/QObject>
#include <QtMultimedia/QVideoSink>

#include "rust/cxx.h"

namespace tp {

// Remembers the sink frames go to; nullptr detaches. Call on the Qt thread.
void set_video_sink(QVideoSink* sink);

// Copies one NV12 frame (Y plane then interleaved UV, both `stride` bytes per
// row, UV starting at `uv_offset`) into a QVideoFrame and queues it onto the
// sink's thread. Safe from any thread. A frame that arrives while the
// previous one is still queued replaces it, so a busy UI thread shows the
// newest frame instead of falling further behind.
void push_nv12(std::uint32_t width, std::uint32_t height, std::uint32_t stride, rust::Slice<const std::uint8_t> data,
               std::size_t uv_offset);

// Blanks the output, for when a session ends.
void clear_video();

}  // namespace tp
