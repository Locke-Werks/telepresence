#include "video_sink.h"

#include <algorithm>
#include <cstring>
#include <mutex>
#include <optional>

#include <QtCore/QMetaObject>
#include <QtCore/QPointer>
#include <QtMultimedia/QVideoFrame>
#include <QtMultimedia/QVideoFrameFormat>

namespace tp {
namespace {

std::mutex g_mutex;
QPointer<QVideoSink> g_sink;
// Newest frame not yet handed to the sink. Only one delivery is queued at a
// time; it takes whatever is here when it runs.
std::optional<QVideoFrame> g_pending;
bool g_queued = false;

void deliver() {
    QPointer<QVideoSink> sink;
    std::optional<QVideoFrame> frame;
    {
        std::lock_guard lock(g_mutex);
        g_queued = false;
        sink = g_sink;
        frame.swap(g_pending);
    }
    if (sink && frame) {
        sink->setVideoFrame(*frame);
    }
}

void queue(std::optional<QVideoFrame> frame) {
    QPointer<QVideoSink> sink;
    {
        std::lock_guard lock(g_mutex);
        if (!g_sink) {
            return;
        }
        g_pending = std::move(frame);
        if (g_queued) {
            return;
        }
        g_queued = true;
        sink = g_sink;
    }
    QMetaObject::invokeMethod(sink.data(), &deliver, Qt::QueuedConnection);
}

}  // namespace

void set_video_sink(QVideoSink* sink) {
    std::lock_guard lock(g_mutex);
    g_sink = sink;
    g_pending.reset();
}

void push_nv12(std::uint32_t width, std::uint32_t height, std::uint32_t stride, rust::Slice<const std::uint8_t> data,
               std::size_t uv_offset) {
    if (width == 0 || height == 0 || stride < width) {
        return;
    }
    const std::size_t y_rows = height;
    const std::size_t uv_rows = (height + 1) / 2;
    if (uv_offset < stride * y_rows || data.size() < uv_offset + stride * uv_rows) {
        return;
    }
    {
        std::lock_guard lock(g_mutex);
        if (!g_sink) {
            return;
        }
    }

    QVideoFrameFormat format(QSize(int(width), int(height)), QVideoFrameFormat::Format_NV12);
    // The decoder does not report colorimetry, so use what H.264 encoders
    // assume when the stream says nothing: BT.709 for HD, BT.601 for SD, both
    // limited range. Getting this wrong is what tints a hand-rolled preview.
    if (height >= 720) {
        format.setColorSpace(QVideoFrameFormat::ColorSpace_BT709);
        format.setColorTransfer(QVideoFrameFormat::ColorTransfer_BT709);
    } else {
        format.setColorSpace(QVideoFrameFormat::ColorSpace_BT601);
        format.setColorTransfer(QVideoFrameFormat::ColorTransfer_BT601);
    }
    format.setColorRange(QVideoFrameFormat::ColorRange_Video);

    QVideoFrame frame(format);
    if (!frame.map(QVideoFrame::WriteOnly)) {
        return;
    }
    const std::uint8_t* src = data.data();
    const std::size_t row = width;
    for (int plane = 0; plane < 2; ++plane) {
        const std::uint8_t* from = plane == 0 ? src : src + uv_offset;
        const std::size_t rows = plane == 0 ? y_rows : uv_rows;
        std::uint8_t* to = frame.bits(plane);
        const std::size_t dst_stride = std::size_t(frame.bytesPerLine(plane));
        if (dst_stride == stride) {
            std::memcpy(to, from, stride * rows);
        } else {
            const std::size_t n = std::min(row, dst_stride);
            for (std::size_t r = 0; r < rows; ++r) {
                std::memcpy(to + r * dst_stride, from + r * stride, n);
            }
        }
    }
    frame.unmap();
    queue(std::move(frame));
}

void clear_video() {
    queue(QVideoFrame());
}

}  // namespace tp
