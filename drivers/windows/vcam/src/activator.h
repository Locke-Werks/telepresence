#pragma once

#include "media.h"

// The object FrameServer creates from our CLSID. It hands out the media
// source lazily, which keeps CoCreateInstance cheap for callers that only
// inspect attributes.
class Activator
    : public RuntimeClass<RuntimeClassFlags<ClassicCom>, ChainInterfaces<IMFActivate, IMFAttributes>> {
public:
    HRESULT RuntimeClassInitialize();

    TP_FORWARD_ATTRIBUTES(attributes_)

    STDMETHODIMP ActivateObject(REFIID riid, void** out) override;
    STDMETHODIMP ShutdownObject() override;
    STDMETHODIMP DetachObject() override;

private:
    std::mutex mutex_;
    ComPtr<IMFAttributes> attributes_;
    ComPtr<MediaSource> source_;
};
