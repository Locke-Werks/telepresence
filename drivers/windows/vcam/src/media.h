#pragma once

#include <windows.h>
#include <ks.h>
#include <ksproxy.h>
#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <wrl/client.h>
#include <wrl/implements.h>
#include <wrl/module.h>

#include <atomic>
#include <condition_variable>
#include <deque>
#include <mutex>
#include <thread>

#include "render.h"
#include "sink_reader.h"

using Microsoft::WRL::ChainInterfaces;
using Microsoft::WRL::ComPtr;
using Microsoft::WRL::RuntimeClass;
using Microsoft::WRL::RuntimeClassFlags;
using Microsoft::WRL::ClassicCom;

// Forwards every IMFAttributes method to a member ComPtr<IMFAttributes>.
// The activator and FrameServer both treat the object itself as the
// attribute store, so it has to expose the full interface.
#define TP_FORWARD_ATTRIBUTES(store)                                                                              \
    STDMETHODIMP GetItem(REFGUID k, PROPVARIANT* v) override { return store->GetItem(k, v); }                     \
    STDMETHODIMP GetItemType(REFGUID k, MF_ATTRIBUTE_TYPE* t) override { return store->GetItemType(k, t); }       \
    STDMETHODIMP CompareItem(REFGUID k, REFPROPVARIANT v, BOOL* r) override { return store->CompareItem(k, v, r); } \
    STDMETHODIMP Compare(IMFAttributes* a, MF_ATTRIBUTES_MATCH_TYPE m, BOOL* r) override {                        \
        return store->Compare(a, m, r);                                                                           \
    }                                                                                                             \
    STDMETHODIMP GetUINT32(REFGUID k, UINT32* v) override { return store->GetUINT32(k, v); }                      \
    STDMETHODIMP GetUINT64(REFGUID k, UINT64* v) override { return store->GetUINT64(k, v); }                      \
    STDMETHODIMP GetDouble(REFGUID k, double* v) override { return store->GetDouble(k, v); }                      \
    STDMETHODIMP GetGUID(REFGUID k, GUID* v) override { return store->GetGUID(k, v); }                            \
    STDMETHODIMP GetStringLength(REFGUID k, UINT32* n) override { return store->GetStringLength(k, n); }          \
    STDMETHODIMP GetString(REFGUID k, LPWSTR s, UINT32 n, UINT32* l) override { return store->GetString(k, s, n, l); } \
    STDMETHODIMP GetAllocatedString(REFGUID k, LPWSTR* s, UINT32* l) override {                                   \
        return store->GetAllocatedString(k, s, l);                                                                \
    }                                                                                                             \
    STDMETHODIMP GetBlobSize(REFGUID k, UINT32* n) override { return store->GetBlobSize(k, n); }                  \
    STDMETHODIMP GetBlob(REFGUID k, UINT8* b, UINT32 n, UINT32* l) override { return store->GetBlob(k, b, n, l); } \
    STDMETHODIMP GetAllocatedBlob(REFGUID k, UINT8** b, UINT32* n) override { return store->GetAllocatedBlob(k, b, n); } \
    STDMETHODIMP GetUnknown(REFGUID k, REFIID i, LPVOID* p) override { return store->GetUnknown(k, i, p); }       \
    STDMETHODIMP SetItem(REFGUID k, REFPROPVARIANT v) override { return store->SetItem(k, v); }                   \
    STDMETHODIMP DeleteItem(REFGUID k) override { return store->DeleteItem(k); }                                  \
    STDMETHODIMP DeleteAllItems() override { return store->DeleteAllItems(); }                                    \
    STDMETHODIMP SetUINT32(REFGUID k, UINT32 v) override { return store->SetUINT32(k, v); }                       \
    STDMETHODIMP SetUINT64(REFGUID k, UINT64 v) override { return store->SetUINT64(k, v); }                       \
    STDMETHODIMP SetDouble(REFGUID k, double v) override { return store->SetDouble(k, v); }                       \
    STDMETHODIMP SetGUID(REFGUID k, REFGUID v) override { return store->SetGUID(k, v); }                          \
    STDMETHODIMP SetString(REFGUID k, LPCWSTR v) override { return store->SetString(k, v); }                      \
    STDMETHODIMP SetBlob(REFGUID k, const UINT8* b, UINT32 n) override { return store->SetBlob(k, b, n); }        \
    STDMETHODIMP SetUnknown(REFGUID k, IUnknown* u) override { return store->SetUnknown(k, u); }                  \
    STDMETHODIMP LockStore() override { return store->LockStore(); }                                              \
    STDMETHODIMP UnlockStore() override { return store->UnlockStore(); }                                          \
    STDMETHODIMP GetCount(UINT32* n) override { return store->GetCount(n); }                                      \
    STDMETHODIMP GetItemByIndex(UINT32 i, GUID* k, PROPVARIANT* v) override { return store->GetItemByIndex(i, k, v); } \
    STDMETHODIMP CopyAllItems(IMFAttributes* d) override { return store->CopyAllItems(d); }

class MediaSource;

class MediaStream
    : public RuntimeClass<RuntimeClassFlags<ClassicCom>,
                          ChainInterfaces<IMFMediaStream2, IMFMediaStream, IMFMediaEventGenerator>, IKsControl> {
public:
    MediaStream() = default;
    ~MediaStream() override;
    HRESULT RuntimeClassInitialize(MediaSource* source);

    // IMFMediaEventGenerator
    STDMETHODIMP BeginGetEvent(IMFAsyncCallback* cb, IUnknown* state) override;
    STDMETHODIMP EndGetEvent(IMFAsyncResult* result, IMFMediaEvent** ev) override;
    STDMETHODIMP GetEvent(DWORD flags, IMFMediaEvent** ev) override;
    STDMETHODIMP QueueEvent(MediaEventType type, REFGUID ext, HRESULT status, const PROPVARIANT* v) override;

    // IMFMediaStream
    STDMETHODIMP GetMediaSource(IMFMediaSource** source) override;
    STDMETHODIMP GetStreamDescriptor(IMFStreamDescriptor** sd) override;
    STDMETHODIMP RequestSample(IUnknown* token) override;

    // IMFMediaStream2
    STDMETHODIMP SetStreamState(MF_STREAM_STATE state) override;
    STDMETHODIMP GetStreamState(MF_STREAM_STATE* state) override;

    // IKsControl
    STDMETHODIMP KsProperty(PKSPROPERTY p, ULONG pl, LPVOID d, ULONG dl, ULONG* ret) override;
    STDMETHODIMP KsMethod(PKSMETHOD m, ULONG ml, LPVOID d, ULONG dl, ULONG* ret) override;
    STDMETHODIMP KsEvent(PKSEVENT e, ULONG el, LPVOID d, ULONG dl, ULONG* ret) override;

    // Called by MediaSource with its lock held.
    HRESULT Start(IMFMediaType* type);
    // Rebuild the type list if the writer's format changed since it was last
    // built and nothing is streaming. Returns true when the descriptor was
    // replaced, so the source rebuilds its presentation descriptor.
    bool RefreshTypes();
    HRESULT Stop();
    void Shutdown();
    IMFAttributes* attributes() const { return attributes_.Get(); }

private:
    HRESULT BuildDescriptor();
    void Worker();
    HRESULT Deliver(IUnknown* token);
    void StopWorker();

    MediaSource* source_ = nullptr;  // owner; cleared on Shutdown
    ComPtr<IMFMediaEventQueue> queue_;
    ComPtr<IMFStreamDescriptor> descriptor_;
    ComPtr<IMFAttributes> attributes_;

    std::mutex mutex_;
    std::condition_variable cv_;
    std::deque<ComPtr<IUnknown>> tokens_;
    std::thread worker_;
    bool quit_ = false;
    bool shutdown_ = false;
    MF_STREAM_STATE state_ = MF_STREAM_STATE_STOPPED;

    PixelFormat format_ = PixelFormat::NV12;
    uint32_t width_ = 0, height_ = 0;
    std::chrono::nanoseconds period_{33333333};
    LONGLONG duration_100ns_ = 333333;
    SinkReader::WriterFormat offered_native_{};  // zero when none was offered
    std::chrono::steady_clock::time_point next_due_{};

    SinkReader reader_;
};

class MediaSource
    : public RuntimeClass<RuntimeClassFlags<ClassicCom>,
                          ChainInterfaces<IMFMediaSourceEx, IMFMediaSource, IMFMediaEventGenerator>, IMFGetService,
                          IKsControl> {
public:
    MediaSource() = default;
    HRESULT RuntimeClassInitialize(IMFAttributes* activator_attributes);

    // IMFMediaEventGenerator
    STDMETHODIMP BeginGetEvent(IMFAsyncCallback* cb, IUnknown* state) override;
    STDMETHODIMP EndGetEvent(IMFAsyncResult* result, IMFMediaEvent** ev) override;
    STDMETHODIMP GetEvent(DWORD flags, IMFMediaEvent** ev) override;
    STDMETHODIMP QueueEvent(MediaEventType type, REFGUID ext, HRESULT status, const PROPVARIANT* v) override;

    // IMFMediaSource
    STDMETHODIMP GetCharacteristics(DWORD* c) override;
    STDMETHODIMP CreatePresentationDescriptor(IMFPresentationDescriptor** pd) override;
    STDMETHODIMP Start(IMFPresentationDescriptor* pd, const GUID* time_format, const PROPVARIANT* start) override;
    STDMETHODIMP Stop() override;
    STDMETHODIMP Pause() override;
    STDMETHODIMP Shutdown() override;

    // IMFMediaSourceEx
    STDMETHODIMP GetSourceAttributes(IMFAttributes** a) override;
    STDMETHODIMP GetStreamAttributes(DWORD id, IMFAttributes** a) override;
    STDMETHODIMP SetD3DManager(IUnknown* manager) override;

    // IMFGetService
    STDMETHODIMP GetService(REFGUID service, REFIID riid, LPVOID* obj) override;

    // IKsControl
    STDMETHODIMP KsProperty(PKSPROPERTY p, ULONG pl, LPVOID d, ULONG dl, ULONG* ret) override;
    STDMETHODIMP KsMethod(PKSMETHOD m, ULONG ml, LPVOID d, ULONG dl, ULONG* ret) override;
    STDMETHODIMP KsEvent(PKSEVENT e, ULONG el, LPVOID d, ULONG dl, ULONG* ret) override;

    std::recursive_mutex& lock() { return mutex_; }

private:
    std::recursive_mutex mutex_;
    ComPtr<IMFMediaEventQueue> queue_;
    ComPtr<IMFAttributes> attributes_;
    ComPtr<IMFPresentationDescriptor> descriptor_;
    ComPtr<MediaStream> stream_;
    bool started_ = false;
    bool shutdown_ = false;
};

// The media types offered, default first.
struct OfferedType {
    PixelFormat format;
    uint32_t width, height;
    uint32_t fps_num = 30, fps_den = 1;
};
// With a live writer, its size and rate come first in each pixel format so a
// client that takes the default gets the sender's native picture; the fixed
// 720p and 1080p types follow, but only those with fewer pixels than the
// source. With no writer, only the fixed types are offered.
HRESULT CreateOfferedMediaTypes(const SinkReader::WriterFormat* native, IMFMediaType*** types, DWORD* count);
