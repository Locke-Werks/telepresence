#include "media.h"

#include "guids.h"

namespace {

// FrameServer pre-queues a handful of requests; anything beyond this is a
// client that stopped draining, and holding more tokens only adds latency.
constexpr size_t kMaxPendingTokens = 8;

PixelFormat FormatFromSubtype(const GUID& g) {
    if (g == MFVideoFormat_YUY2) return PixelFormat::YUY2;
    if (g == MFVideoFormat_RGB32) return PixelFormat::RGB32;
    return PixelFormat::NV12;
}

}  // namespace

MediaStream::~MediaStream() { StopWorker(); }

HRESULT MediaStream::RuntimeClassInitialize(MediaSource* source) {
    source_ = source;
    HRESULT hr = MFCreateEventQueue(&queue_);
    if (FAILED(hr)) return hr;

    hr = MFCreateAttributes(&attributes_, 4);
    if (FAILED(hr)) return hr;
    attributes_->SetGUID(MF_DEVICESTREAM_STREAM_CATEGORY, kPinNameVideoCapture);
    attributes_->SetUINT32(MF_DEVICESTREAM_STREAM_ID, 0);
    attributes_->SetUINT32(MF_DEVICESTREAM_FRAMESERVER_SHARED, 1);
    attributes_->SetUINT32(MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MFFrameSourceTypes_Color);

    // Open the section first so a writer already attached decides the
    // native type offered from the very first activation.
    reader_.EnsureOpen();
    return BuildDescriptor();
}

HRESULT MediaStream::BuildDescriptor() {
    SinkReader::WriterFormat native{};
    const bool have = reader_.PeekWriterFormat(&native);

    IMFMediaType** types = nullptr;
    DWORD count = 0;
    HRESULT hr = CreateOfferedMediaTypes(have ? &native : nullptr, &types, &count);
    if (FAILED(hr)) return hr;
    ComPtr<IMFStreamDescriptor> sd;
    hr = MFCreateStreamDescriptor(0, count, types, &sd);
    if (SUCCEEDED(hr)) {
        ComPtr<IMFMediaTypeHandler> handler;
        hr = sd->GetMediaTypeHandler(&handler);
        if (SUCCEEDED(hr)) hr = handler->SetCurrentMediaType(types[0]);
    }
    for (DWORD i = 0; i < count; ++i) types[i]->Release();
    CoTaskMemFree(types);
    if (FAILED(hr)) return hr;

    // FrameServer reads these off the descriptor when it builds the device
    // interface, and off GetStreamAttributes later; keep both in step.
    attributes_->CopyAllItems(sd.Get());
    std::lock_guard l(mutex_);
    descriptor_ = sd;
    offered_native_ = have ? native : SinkReader::WriterFormat{};
    return S_OK;
}

bool MediaStream::RefreshTypes() {
    {
        std::lock_guard l(mutex_);
        if (shutdown_ || state_ != MF_STREAM_STATE_STOPPED) return false;
    }
    SinkReader::WriterFormat native{};
    if (!reader_.PeekWriterFormat(&native)) native = {};
    {
        std::lock_guard l(mutex_);
        if (native == offered_native_) return false;
    }
    return SUCCEEDED(BuildDescriptor());
}


STDMETHODIMP MediaStream::BeginGetEvent(IMFAsyncCallback* cb, IUnknown* state) {
    std::lock_guard l(mutex_);
    if (shutdown_) return MF_E_SHUTDOWN;
    return queue_->BeginGetEvent(cb, state);
}

STDMETHODIMP MediaStream::EndGetEvent(IMFAsyncResult* result, IMFMediaEvent** ev) {
    std::lock_guard l(mutex_);
    if (shutdown_) return MF_E_SHUTDOWN;
    return queue_->EndGetEvent(result, ev);
}

STDMETHODIMP MediaStream::GetEvent(DWORD flags, IMFMediaEvent** ev) {
    ComPtr<IMFMediaEventQueue> q;
    {
        std::lock_guard l(mutex_);
        if (shutdown_) return MF_E_SHUTDOWN;
        q = queue_;
    }
    return q->GetEvent(flags, ev);
}

STDMETHODIMP MediaStream::QueueEvent(MediaEventType type, REFGUID ext, HRESULT status, const PROPVARIANT* v) {
    std::lock_guard l(mutex_);
    if (shutdown_) return MF_E_SHUTDOWN;
    return queue_->QueueEventParamVar(type, ext, status, v);
}

STDMETHODIMP MediaStream::GetMediaSource(IMFMediaSource** source) {
    if (!source) return E_POINTER;
    std::lock_guard l(mutex_);
    if (shutdown_ || !source_) return MF_E_SHUTDOWN;
    *source = static_cast<IMFMediaSourceEx*>(source_);
    (*source)->AddRef();
    return S_OK;
}

STDMETHODIMP MediaStream::GetStreamDescriptor(IMFStreamDescriptor** sd) {
    if (!sd) return E_POINTER;
    std::lock_guard l(mutex_);
    if (shutdown_) return MF_E_SHUTDOWN;
    return descriptor_.CopyTo(sd);
}

STDMETHODIMP MediaStream::RequestSample(IUnknown* token) {
    std::lock_guard l(mutex_);
    if (shutdown_) return MF_E_SHUTDOWN;
    if (state_ != MF_STREAM_STATE_RUNNING) return MF_E_MEDIA_SOURCE_WRONGSTATE;
    if (tokens_.size() >= kMaxPendingTokens) tokens_.pop_front();
    tokens_.emplace_back(token);
    cv_.notify_one();
    return S_OK;
}

STDMETHODIMP MediaStream::SetStreamState(MF_STREAM_STATE state) {
    {
        std::lock_guard l(mutex_);
        if (shutdown_) return MF_E_SHUTDOWN;
        if (state == state_) return S_OK;
    }
    switch (state) {
        case MF_STREAM_STATE_RUNNING: {
            ComPtr<IMFMediaTypeHandler> handler;
            ComPtr<IMFMediaType> type;
            HRESULT hr = descriptor_->GetMediaTypeHandler(&handler);
            if (SUCCEEDED(hr)) hr = handler->GetCurrentMediaType(&type);
            return SUCCEEDED(hr) ? Start(type.Get()) : hr;
        }
        case MF_STREAM_STATE_STOPPED:
            return Stop();
        case MF_STREAM_STATE_PAUSED: {
            std::lock_guard l(mutex_);
            state_ = MF_STREAM_STATE_PAUSED;
            return S_OK;
        }
        default:
            return MF_E_INVALID_STATE_TRANSITION;
    }
}

STDMETHODIMP MediaStream::GetStreamState(MF_STREAM_STATE* state) {
    if (!state) return E_POINTER;
    std::lock_guard l(mutex_);
    if (shutdown_) return MF_E_SHUTDOWN;
    *state = state_;
    return S_OK;
}

STDMETHODIMP MediaStream::KsProperty(PKSPROPERTY, ULONG, LPVOID, ULONG, ULONG*) {
    return HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND);
}
STDMETHODIMP MediaStream::KsMethod(PKSMETHOD, ULONG, LPVOID, ULONG, ULONG*) {
    return HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND);
}
STDMETHODIMP MediaStream::KsEvent(PKSEVENT, ULONG, LPVOID, ULONG, ULONG*) {
    return HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND);
}

HRESULT MediaStream::Start(IMFMediaType* type) {
    if (!type) return E_INVALIDARG;
    GUID subtype{};
    UINT32 w = 0, h = 0;
    HRESULT hr = type->GetGUID(MF_MT_SUBTYPE, &subtype);
    if (SUCCEEDED(hr)) hr = MFGetAttributeSize(type, MF_MT_FRAME_SIZE, &w, &h);
    if (FAILED(hr)) return hr;
    if (!w || !h || (w & 1) || (h & 1)) return MF_E_INVALIDMEDIATYPE;
    UINT32 fps_num = 30, fps_den = 1;
    if (FAILED(MFGetAttributeRatio(type, MF_MT_FRAME_RATE, &fps_num, &fps_den)) || !fps_num || !fps_den) {
        fps_num = 30;
        fps_den = 1;
    }

    StopWorker();
    {
        std::lock_guard l(mutex_);
        if (shutdown_) return MF_E_SHUTDOWN;
        format_ = FormatFromSubtype(subtype);
        width_ = w;
        height_ = h;
        period_ = std::chrono::nanoseconds(uint64_t{1000000000} * fps_den / fps_num);
        duration_100ns_ = static_cast<LONGLONG>(uint64_t{10000000} * fps_den / fps_num);
        tokens_.clear();
        quit_ = false;
        state_ = MF_STREAM_STATE_RUNNING;
        next_due_ = std::chrono::steady_clock::now();
        worker_ = std::thread([this] { Worker(); });
    }
    return queue_->QueueEventParamVar(MEStreamStarted, GUID_NULL, S_OK, nullptr);
}

HRESULT MediaStream::Stop() {
    StopWorker();
    {
        std::lock_guard l(mutex_);
        if (shutdown_) return MF_E_SHUTDOWN;
        state_ = MF_STREAM_STATE_STOPPED;
        tokens_.clear();
    }
    return queue_->QueueEventParamVar(MEStreamStopped, GUID_NULL, S_OK, nullptr);
}

void MediaStream::Shutdown() {
    StopWorker();
    std::lock_guard l(mutex_);
    if (shutdown_) return;
    shutdown_ = true;
    state_ = MF_STREAM_STATE_STOPPED;
    tokens_.clear();
    source_ = nullptr;
    queue_->Shutdown();
}

void MediaStream::StopWorker() {
    std::thread t;
    {
        std::lock_guard l(mutex_);
        quit_ = true;
        t = std::move(worker_);
    }
    cv_.notify_all();
    if (t.joinable() && t.get_id() != std::this_thread::get_id()) t.join();
}

// FrameServer calls RequestSample as fast as it likes; this thread answers
// those requests at the negotiated frame rate so a client never sees more frames than the media
// type promises and an idle Receiver does not cost a spinning core.
void MediaStream::Worker() {
    std::unique_lock lk(mutex_);
    while (!quit_) {
        if (tokens_.empty()) {
            cv_.wait(lk);
            continue;
        }
        const auto now = std::chrono::steady_clock::now();
        if (now < next_due_) {
            cv_.wait_until(lk, next_due_);
            continue;
        }
        // Fall back onto the wall clock after a stall instead of bursting to
        // catch up.
        next_due_ = (now - next_due_ > period_) ? now + period_ : next_due_ + period_;
        ComPtr<IUnknown> token = std::move(tokens_.front());
        tokens_.pop_front();
        lk.unlock();
        Deliver(token.Get());
        lk.lock();
    }
}

HRESULT MediaStream::Deliver(IUnknown* token) {
    // When the writer's size changes mid-session the frame is scaled into the
    // negotiated type rather than signalling a format change: FrameServer
    // shares one stream among clients and does not pass MEStreamFormatChanged
    // through to them, so the new native size is offered on the next open.
    // format_/width_/height_ only change in Start, after this thread is joined.
    const PixelFormat fmt = format_;
    const uint32_t w = width_, h = height_;
    const LONGLONG duration = duration_100ns_;
    const DWORD bytes = FrameBytes(fmt, w, h);

    ComPtr<IMFMediaBuffer> buffer;
    HRESULT hr = MFCreateMemoryBuffer(bytes, &buffer);
    if (FAILED(hr)) return hr;
    BYTE* dst = nullptr;
    hr = buffer->Lock(&dst, nullptr, nullptr);
    if (FAILED(hr)) return hr;

    if (reader_.Poll() == SinkReader::State::NoSignal) {
        RenderNoSignal(dst, fmt, w, h);
    } else {
        const SinkFrame& f = reader_.frame();
        RenderNv12(f.data.data(), f.width, f.height, f.stride, dst, fmt, w, h);
    }
    buffer->Unlock();
    buffer->SetCurrentLength(bytes);

    ComPtr<IMFSample> sample;
    hr = MFCreateSample(&sample);
    if (FAILED(hr)) return hr;
    sample->AddBuffer(buffer.Get());
    sample->SetSampleTime(MFGetSystemTime());
    sample->SetSampleDuration(duration);
    if (token) sample->SetUnknown(MFSampleExtension_Token, token);

    ComPtr<IMFMediaEventQueue> q;
    {
        std::lock_guard l(mutex_);
        if (shutdown_ || state_ != MF_STREAM_STATE_RUNNING) return MF_E_SHUTDOWN;
        q = queue_;
    }
    return q->QueueEventParamUnk(MEMediaSample, GUID_NULL, S_OK, sample.Get());
}
