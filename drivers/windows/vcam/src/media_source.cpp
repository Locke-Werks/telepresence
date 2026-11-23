#include "media.h"

#include "guids.h"

#include <algorithm>
#include <vector>

namespace {

constexpr OfferedType kFallback[] = {
    {PixelFormat::NV12, 1280, 720},  {PixelFormat::NV12, 1920, 1080}, {PixelFormat::YUY2, 1280, 720},
    {PixelFormat::YUY2, 1920, 1080}, {PixelFormat::RGB32, 1280, 720}, {PixelFormat::RGB32, 1920, 1080},
};
constexpr PixelFormat kFormats[] = {PixelFormat::NV12, PixelFormat::YUY2, PixelFormat::RGB32};

const GUID& Subtype(PixelFormat f) {
    switch (f) {
        case PixelFormat::YUY2: return MFVideoFormat_YUY2;
        case PixelFormat::RGB32: return MFVideoFormat_RGB32;
        default: return MFVideoFormat_NV12;
    }
}

HRESULT MakeType(const OfferedType& o, IMFMediaType** out) {
    ComPtr<IMFMediaType> t;
    HRESULT hr = MFCreateMediaType(&t);
    if (FAILED(hr)) return hr;
    const UINT32 frame = FrameBytes(o.format, o.width, o.height);
    t->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
    t->SetGUID(MF_MT_SUBTYPE, Subtype(o.format));
    MFSetAttributeSize(t.Get(), MF_MT_FRAME_SIZE, o.width, o.height);
    MFSetAttributeRatio(t.Get(), MF_MT_FRAME_RATE, o.fps_num, o.fps_den);
    MFSetAttributeRatio(t.Get(), MF_MT_PIXEL_ASPECT_RATIO, 1, 1);
    t->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
    t->SetUINT32(MF_MT_ALL_SAMPLES_INDEPENDENT, TRUE);
    t->SetUINT32(MF_MT_FIXED_SIZE_SAMPLES, TRUE);
    t->SetUINT32(MF_MT_SAMPLE_SIZE, frame);
    // Positive stride means top-down rows, which is how render.cpp writes RGB32.
    t->SetUINT32(MF_MT_DEFAULT_STRIDE, RowBytes(o.format, o.width));
    t->SetUINT32(MF_MT_AVG_BITRATE,
                 static_cast<UINT32>(std::min<uint64_t>(uint64_t{frame} * 8 * o.fps_num / o.fps_den, UINT32_MAX)));
    *out = t.Detach();
    return S_OK;
}

}  // namespace

HRESULT CreateOfferedMediaTypes(const SinkReader::WriterFormat* native, IMFMediaType*** types, DWORD* count) {
    std::vector<OfferedType> list;
    if (native) {
        const uint32_t num = native->fps_num ? native->fps_num : 30;
        const uint32_t den = native->fps_num ? native->fps_den : 1;
        for (PixelFormat f : kFormats) list.push_back({f, native->width, native->height, num, den});
    }
    for (const OfferedType& o : kFallback) {
        // Only standard sizes with fewer pixels than the source follow it.
        // Clients that pick the largest type on offer (GStreamer's
        // mfvideosrc, and most apps when unconstrained) would otherwise take
        // a 16:9 fallback over a smaller or portrait source and get it
        // letterboxed. A smaller fallback still serves a client that asks
        // for 720p, and it is a downscale, never an upscale.
        const bool keep = !native || uint64_t{o.width} * o.height < uint64_t{native->width} * native->height;
        if (keep) list.push_back(o);
    }

    const DWORD n = static_cast<DWORD>(list.size());
    auto* arr = static_cast<IMFMediaType**>(CoTaskMemAlloc(sizeof(IMFMediaType*) * n));
    if (!arr) return E_OUTOFMEMORY;
    for (DWORD i = 0; i < n; ++i) {
        HRESULT hr = MakeType(list[i], &arr[i]);
        if (FAILED(hr)) {
            for (DWORD j = 0; j < i; ++j) arr[j]->Release();
            CoTaskMemFree(arr);
            return hr;
        }
    }
    *types = arr;
    *count = n;
    return S_OK;
}

HRESULT MediaSource::RuntimeClassInitialize(IMFAttributes* activator_attributes) {
    HRESULT hr = MFCreateEventQueue(&queue_);
    if (FAILED(hr)) return hr;
    hr = MFCreateAttributes(&attributes_, 4);
    if (FAILED(hr)) return hr;
    if (activator_attributes) activator_attributes->CopyAllItems(attributes_.Get());

    hr = Microsoft::WRL::MakeAndInitialize<MediaStream>(&stream_, this);
    if (FAILED(hr)) return hr;

    ComPtr<IMFStreamDescriptor> sd;
    hr = stream_->GetStreamDescriptor(&sd);
    if (FAILED(hr)) return hr;
    IMFStreamDescriptor* sds[] = {sd.Get()};
    hr = MFCreatePresentationDescriptor(1, sds, &descriptor_);
    if (FAILED(hr)) return hr;
    return descriptor_->SelectStream(0);
}

#define TP_CHECK_SHUTDOWN() \
    if (shutdown_) return MF_E_SHUTDOWN

STDMETHODIMP MediaSource::BeginGetEvent(IMFAsyncCallback* cb, IUnknown* state) {
    std::lock_guard l(mutex_);
    TP_CHECK_SHUTDOWN();
    return queue_->BeginGetEvent(cb, state);
}

STDMETHODIMP MediaSource::EndGetEvent(IMFAsyncResult* result, IMFMediaEvent** ev) {
    std::lock_guard l(mutex_);
    TP_CHECK_SHUTDOWN();
    return queue_->EndGetEvent(result, ev);
}

STDMETHODIMP MediaSource::GetEvent(DWORD flags, IMFMediaEvent** ev) {
    // GetEvent may block, so it must not hold the lock or Start/Stop would
    // deadlock behind it.
    ComPtr<IMFMediaEventQueue> q;
    {
        std::lock_guard l(mutex_);
        TP_CHECK_SHUTDOWN();
        q = queue_;
    }
    return q->GetEvent(flags, ev);
}

STDMETHODIMP MediaSource::QueueEvent(MediaEventType type, REFGUID ext, HRESULT status, const PROPVARIANT* v) {
    std::lock_guard l(mutex_);
    TP_CHECK_SHUTDOWN();
    return queue_->QueueEventParamVar(type, ext, status, v);
}

STDMETHODIMP MediaSource::GetCharacteristics(DWORD* c) {
    if (!c) return E_POINTER;
    std::lock_guard l(mutex_);
    TP_CHECK_SHUTDOWN();
    *c = MFMEDIASOURCE_IS_LIVE;
    return S_OK;
}

STDMETHODIMP MediaSource::CreatePresentationDescriptor(IMFPresentationDescriptor** pd) {
    if (!pd) return E_POINTER;
    std::lock_guard l(mutex_);
    TP_CHECK_SHUTDOWN();
    // FrameServer may keep a source alive across client sessions, so the
    // native type is re-read whenever a new presentation descriptor is asked
    // for while idle, not only at activation.
    if (!started_ && stream_->RefreshTypes()) {
        ComPtr<IMFStreamDescriptor> sd;
        HRESULT hr = stream_->GetStreamDescriptor(&sd);
        ComPtr<IMFPresentationDescriptor> fresh;
        IMFStreamDescriptor* sds[] = {sd.Get()};
        if (SUCCEEDED(hr)) hr = MFCreatePresentationDescriptor(1, sds, &fresh);
        if (SUCCEEDED(hr)) hr = fresh->SelectStream(0);
        if (SUCCEEDED(hr)) descriptor_ = fresh;
    }
    return descriptor_->Clone(pd);
}

STDMETHODIMP MediaSource::Start(IMFPresentationDescriptor* pd, const GUID* time_format, const PROPVARIANT* start) {
    if (!pd || !start) return E_INVALIDARG;
    if (time_format && *time_format != GUID_NULL) return MF_E_UNSUPPORTED_TIME_FORMAT;
    std::lock_guard l(mutex_);
    TP_CHECK_SHUTDOWN();

    DWORD count = 0;
    HRESULT hr = pd->GetStreamDescriptorCount(&count);
    if (FAILED(hr)) return hr;
    if (count != 1) return E_INVALIDARG;

    BOOL selected = FALSE;
    ComPtr<IMFStreamDescriptor> sd;
    hr = pd->GetStreamDescriptorByIndex(0, &selected, &sd);
    if (FAILED(hr)) return hr;

    if (selected) {
        ComPtr<IMFMediaTypeHandler> handler;
        ComPtr<IMFMediaType> type;
        hr = sd->GetMediaTypeHandler(&handler);
        if (SUCCEEDED(hr)) hr = handler->GetCurrentMediaType(&type);
        if (FAILED(hr)) return hr;

        hr = queue_->QueueEventParamUnk(started_ ? MEUpdatedStream : MENewStream, GUID_NULL, S_OK,
                                        static_cast<IMFMediaStream2*>(stream_.Get()));
        if (FAILED(hr)) return hr;
        hr = stream_->Start(type.Get());
        if (FAILED(hr)) return hr;
    }
    started_ = true;
    return queue_->QueueEventParamVar(MESourceStarted, GUID_NULL, S_OK, start);
}

STDMETHODIMP MediaSource::Stop() {
    std::lock_guard l(mutex_);
    TP_CHECK_SHUTDOWN();
    HRESULT hr = stream_->Stop();
    if (FAILED(hr)) return hr;
    started_ = false;
    return queue_->QueueEventParamVar(MESourceStopped, GUID_NULL, S_OK, nullptr);
}

STDMETHODIMP MediaSource::Pause() {
    // A live camera has no paused state.
    return MF_E_INVALID_STATE_TRANSITION;
}

STDMETHODIMP MediaSource::Shutdown() {
    ComPtr<MediaStream> stream;
    {
        std::lock_guard l(mutex_);
        TP_CHECK_SHUTDOWN();
        shutdown_ = true;
        queue_->Shutdown();
        stream = std::move(stream_);
    }
    // Outside the lock: the worker thread may be waiting for it while
    // delivering a sample, and Shutdown joins that thread.
    if (stream) stream->Shutdown();
    return S_OK;
}

STDMETHODIMP MediaSource::GetSourceAttributes(IMFAttributes** a) {
    if (!a) return E_POINTER;
    std::lock_guard l(mutex_);
    TP_CHECK_SHUTDOWN();
    return attributes_.CopyTo(a);
}

STDMETHODIMP MediaSource::GetStreamAttributes(DWORD id, IMFAttributes** a) {
    if (!a) return E_POINTER;
    std::lock_guard l(mutex_);
    TP_CHECK_SHUTDOWN();
    if (id != 0) return MF_E_INVALIDSTREAMNUMBER;
    *a = stream_->attributes();
    (*a)->AddRef();
    return S_OK;
}

STDMETHODIMP MediaSource::SetD3DManager(IUnknown*) {
    // Frames are produced in system memory; FrameServer uploads as needed.
    return S_OK;
}

STDMETHODIMP MediaSource::GetService(REFGUID, REFIID, LPVOID* obj) {
    if (obj) *obj = nullptr;
    return MF_E_UNSUPPORTED_SERVICE;
}

// No camera controls (exposure, focus, ...) are exposed. ERROR_SET_NOT_FOUND
// is what the KS proxy expects for "this property set is not supported".
STDMETHODIMP MediaSource::KsProperty(PKSPROPERTY, ULONG, LPVOID, ULONG, ULONG*) {
    return HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND);
}
STDMETHODIMP MediaSource::KsMethod(PKSMETHOD, ULONG, LPVOID, ULONG, ULONG*) {
    return HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND);
}
STDMETHODIMP MediaSource::KsEvent(PKSEVENT, ULONG, LPVOID, ULONG, ULONG*) {
    return HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND);
}
