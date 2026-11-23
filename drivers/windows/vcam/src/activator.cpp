#include "activator.h"

#include <mfvirtualcamera.h>

#include "guids.h"

HRESULT Activator::RuntimeClassInitialize() {
    HRESULT hr = MFCreateAttributes(&attributes_, 2);
    if (FAILED(hr)) return hr;
    // FrameServer looks for these on the activate object it gets from the
    // CLSID registered with MFCreateVirtualCamera.
    attributes_->SetUINT32(MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES, 1);
    attributes_->SetGUID(MFT_TRANSFORM_CLSID_Attribute, CLSID_TelePresenceCamera);
    return S_OK;
}

STDMETHODIMP Activator::ActivateObject(REFIID riid, void** out) {
    if (!out) return E_POINTER;
    *out = nullptr;
    std::lock_guard l(mutex_);
    if (!source_) {
        HRESULT hr = Microsoft::WRL::MakeAndInitialize<MediaSource>(&source_, attributes_.Get());
        if (FAILED(hr)) return hr;
    }
    return source_.CopyTo(riid, out);
}

STDMETHODIMP Activator::ShutdownObject() {
    ComPtr<MediaSource> s;
    {
        std::lock_guard l(mutex_);
        s = std::move(source_);
    }
    if (s) s->Shutdown();
    return S_OK;
}

STDMETHODIMP Activator::DetachObject() {
    std::lock_guard l(mutex_);
    source_.Reset();
    return S_OK;
}
