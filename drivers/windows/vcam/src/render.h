#pragma once

#include <cstdint>

enum class PixelFormat { NV12, YUY2, RGB32 };

// Bytes per row and total buffer size for a tightly packed output frame.
uint32_t RowBytes(PixelFormat fmt, uint32_t width);
uint32_t FrameBytes(PixelFormat fmt, uint32_t width, uint32_t height);

// Scale an NV12 source into dst, preserving aspect ratio with black bars.
// Nearest-neighbour: the common case is a same-size copy, and a camera feed
// headed for a video call is about to be re-encoded anyway.
void RenderNv12(const uint8_t* src, uint32_t src_w, uint32_t src_h, uint32_t src_stride, uint8_t* dst,
                PixelFormat fmt, uint32_t dst_w, uint32_t dst_h);

// Dark frame with "NO SIGNAL" across the middle.
void RenderNoSignal(uint8_t* dst, PixelFormat fmt, uint32_t dst_w, uint32_t dst_h);
