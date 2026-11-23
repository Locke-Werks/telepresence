#include "render.h"

#include <algorithm>
#include <cstring>
#include <vector>

namespace {

constexpr uint8_t kBlackY = 16;
constexpr uint8_t kNeutralC = 128;

struct Rect {
    uint32_t x, y, w, h;
};

// Largest even-aligned rectangle with the source aspect ratio, centred. Even
// coordinates keep 4:2:0 chroma sites aligned with their luma pairs.
Rect Fit(uint32_t sw, uint32_t sh, uint32_t dw, uint32_t dh) {
    uint32_t w = dw, h = dh;
    if (uint64_t{sw} * dh > uint64_t{dw} * sh) {
        h = static_cast<uint32_t>(uint64_t{dw} * sh / sw);
    } else {
        w = static_cast<uint32_t>(uint64_t{dh} * sw / sh);
    }
    w = std::max<uint32_t>(2, w & ~1u);
    h = std::max<uint32_t>(2, h & ~1u);
    return {((dw - w) / 2) & ~1u, ((dh - h) / 2) & ~1u, w, h};
}

inline uint8_t Clamp(int v) { return static_cast<uint8_t>(v < 0 ? 0 : v > 255 ? 255 : v); }

// BT.601 limited range, the default MF assumes for NV12 at these sizes when
// no MF_MT_YUV_MATRIX is set.
inline void ToBgra(uint8_t y, uint8_t u, uint8_t v, uint8_t* out) {
    const int c = 298 * (y - 16), d = u - 128, e = v - 128;
    out[0] = Clamp((c + 516 * d + 128) >> 8);
    out[1] = Clamp((c - 100 * d - 208 * e + 128) >> 8);
    out[2] = Clamp((c + 409 * e + 128) >> 8);
    out[3] = 255;
}

void FillBlack(uint8_t* dst, PixelFormat fmt, uint32_t w, uint32_t h) {
    switch (fmt) {
        case PixelFormat::NV12:
            std::memset(dst, kBlackY, size_t{w} * h);
            std::memset(dst + size_t{w} * h, kNeutralC, size_t{w} * h / 2);
            break;
        case PixelFormat::YUY2:
            for (size_t i = 0; i < size_t{w} * h * 2; i += 4) {
                dst[i] = kBlackY;
                dst[i + 1] = kNeutralC;
                dst[i + 2] = kBlackY;
                dst[i + 3] = kNeutralC;
            }
            break;
        case PixelFormat::RGB32:
            for (size_t i = 0; i < size_t{w} * h; ++i) {
                dst[i * 4] = dst[i * 4 + 1] = dst[i * 4 + 2] = 0;
                dst[i * 4 + 3] = 255;
            }
            break;
    }
}

// 5x7 glyphs, one row per byte, bit 4 is the leftmost column.
struct Glyph {
    char c;
    uint8_t rows[7];
};
constexpr Glyph kFont[] = {
    {'N', {0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11}}, {'O', {0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E}},
    {'S', {0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E}}, {'I', {0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E}},
    {'G', {0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F}}, {'A', {0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11}},
    {'L', {0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F}},
};

constexpr uint32_t kCardW = 640, kCardH = 360;

std::vector<uint8_t> BuildNoSignalCard() {
    std::vector<uint8_t> f(size_t{kCardW} * kCardH * 3 / 2, kNeutralC);
    std::memset(f.data(), 28, size_t{kCardW} * kCardH);
    const char* text = "NO SIGNAL";
    constexpr uint32_t scale = 6, advance = 6 * scale;
    const uint32_t len = static_cast<uint32_t>(std::strlen(text));
    const uint32_t x0 = (kCardW - (len * advance - scale)) / 2;
    const uint32_t y0 = (kCardH - 7 * scale) / 2;
    for (uint32_t i = 0; i < len; ++i) {
        const Glyph* g = nullptr;
        for (const auto& candidate : kFont)
            if (candidate.c == text[i]) g = &candidate;
        if (!g) continue;
        for (uint32_t r = 0; r < 7 * scale; ++r)
            for (uint32_t c = 0; c < 5 * scale; ++c)
                if (g->rows[r / scale] & (0x10 >> (c / scale)))
                    f[size_t{y0 + r} * kCardW + x0 + i * advance + c] = 150;
    }
    return f;
}

}  // namespace

uint32_t RowBytes(PixelFormat fmt, uint32_t width) {
    switch (fmt) {
        case PixelFormat::NV12: return width;
        case PixelFormat::YUY2: return width * 2;
        case PixelFormat::RGB32: return width * 4;
    }
    return 0;
}

uint32_t FrameBytes(PixelFormat fmt, uint32_t width, uint32_t height) {
    return fmt == PixelFormat::NV12 ? width * height * 3 / 2 : RowBytes(fmt, width) * height;
}

void RenderNv12(const uint8_t* src, uint32_t sw, uint32_t sh, uint32_t ss, uint8_t* dst, PixelFormat fmt,
                uint32_t dw, uint32_t dh) {
    const uint8_t* src_uv = src + size_t{ss} * sh;

    if (fmt == PixelFormat::NV12 && sw == dw && sh == dh) {
        for (uint32_t y = 0; y < dh; ++y) std::memcpy(dst + size_t{y} * dw, src + size_t{y} * ss, dw);
        uint8_t* duv = dst + size_t{dw} * dh;
        for (uint32_t y = 0; y < dh / 2; ++y) std::memcpy(duv + size_t{y} * dw, src_uv + size_t{y} * ss, dw);
        return;
    }

    const Rect r = Fit(sw, sh, dw, dh);
    if (r.w != dw || r.h != dh) FillBlack(dst, fmt, dw, dh);

    std::vector<uint32_t> xmap(r.w);
    for (uint32_t x = 0; x < r.w; ++x) xmap[x] = static_cast<uint32_t>(uint64_t{x} * sw / r.w);

    for (uint32_t y = 0; y < r.h; ++y) {
        const uint32_t sy = static_cast<uint32_t>(uint64_t{y} * sh / r.h);
        const uint8_t* sY = src + size_t{sy} * ss;
        const uint8_t* sUV = src_uv + size_t{sy / 2} * ss;
        const uint32_t dy = r.y + y;
        switch (fmt) {
            case PixelFormat::NV12: {
                uint8_t* dY = dst + size_t{dy} * dw + r.x;
                for (uint32_t x = 0; x < r.w; ++x) dY[x] = sY[xmap[x]];
                if ((dy & 1) == 0) {
                    uint8_t* dUV = dst + size_t{dw} * dh + size_t{dy / 2} * dw + r.x;
                    for (uint32_t x = 0; x < r.w; x += 2) {
                        const uint32_t sx = xmap[x] & ~1u;
                        dUV[x] = sUV[sx];
                        dUV[x + 1] = sUV[sx + 1];
                    }
                }
                break;
            }
            case PixelFormat::YUY2: {
                uint8_t* d = dst + size_t{dy} * dw * 2 + size_t{r.x} * 2;
                for (uint32_t x = 0; x < r.w; x += 2) {
                    const uint32_t sx = xmap[x] & ~1u;
                    d[x * 2] = sY[xmap[x]];
                    d[x * 2 + 1] = sUV[sx];
                    d[x * 2 + 2] = sY[xmap[x + 1]];
                    d[x * 2 + 3] = sUV[sx + 1];
                }
                break;
            }
            case PixelFormat::RGB32: {
                uint8_t* d = dst + size_t{dy} * dw * 4 + size_t{r.x} * 4;
                for (uint32_t x = 0; x < r.w; ++x) {
                    const uint32_t sx = xmap[x] & ~1u;
                    ToBgra(sY[xmap[x]], sUV[sx], sUV[sx + 1], d + x * 4);
                }
                break;
            }
        }
    }
}

void RenderNoSignal(uint8_t* dst, PixelFormat fmt, uint32_t dw, uint32_t dh) {
    static const std::vector<uint8_t> card = BuildNoSignalCard();
    RenderNv12(card.data(), kCardW, kCardH, kCardW, dst, fmt, dw, dh);
}
