// tp-vcam-register: registers tp-vcam.dll as a COM server and the
// "TelePresence Camera" with FrameServer, or removes both with --remove.
// Needs an elevated prompt: both steps write machine-wide state.

#include <windows.h>
#include <mfapi.h>
#include <mfvirtualcamera.h>
#include <wrl/client.h>

#include <cstdio>
#include <cwchar>
#include <string>

#include "guids.h"

using Microsoft::WRL::ComPtr;

namespace {

std::wstring DefaultDllPath() {
    wchar_t exe[MAX_PATH];
    GetModuleFileNameW(nullptr, exe, MAX_PATH);
    std::wstring p(exe);
    p.resize(p.find_last_of(L"\\/") + 1);
    return p + L"tp-vcam.dll";
}

HRESULT CallExport(const std::wstring& dll, const char* name) {
    HMODULE m = LoadLibraryExW(dll.c_str(), nullptr, LOAD_WITH_ALTERED_SEARCH_PATH);
    if (!m) return HRESULT_FROM_WIN32(GetLastError());
    auto fn = reinterpret_cast<HRESULT(STDAPICALLTYPE*)()>(GetProcAddress(m, name));
    const HRESULT hr = fn ? fn() : HRESULT_FROM_WIN32(GetLastError());
    FreeLibrary(m);
    return hr;
}

HRESULT CreateCamera(ComPtr<IMFVirtualCamera>& cam) {
    // Same parameters on add and remove: FrameServer identifies a persistent
    // camera by them, so --remove gets a handle to the one registered earlier.
    return MFCreateVirtualCamera(MFVirtualCameraType_SoftwareCameraSource, MFVirtualCameraLifetime_System,
                                 MFVirtualCameraAccess_AllUsers, kCameraFriendlyName,
                                 CLSID_TelePresenceCamera_String, nullptr, 0, &cam);
}

int Fail(const wchar_t* what, HRESULT hr) {
    fwprintf(stderr, L"%ls failed: 0x%08lX\n", what, static_cast<unsigned long>(hr));
    if (hr == E_ACCESSDENIED || hr == HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED))
        fwprintf(stderr, L"Run from an elevated prompt.\n");
    return 1;
}

int Register(const std::wstring& dll) {
    if (GetFileAttributesW(dll.c_str()) == INVALID_FILE_ATTRIBUTES) {
        fwprintf(stderr, L"%ls not found\n", dll.c_str());
        return 1;
    }
    if (_wcsnicmp(dll.c_str(), L"C:\\Users\\", 9) == 0)
        fwprintf(stderr,
                 L"warning: %ls is under a user profile; FrameServer (LocalService) usually cannot read it.\n"
                 L"Install it first (cmake --install) and register the installed copy.\n",
                 dll.c_str());

    HRESULT hr = CallExport(dll, "DllRegisterServer");
    if (FAILED(hr)) return Fail(L"DllRegisterServer", hr);

    ComPtr<IMFVirtualCamera> cam;
    hr = CreateCamera(cam);
    if (FAILED(hr)) return Fail(L"MFCreateVirtualCamera", hr);
    hr = cam->Start(nullptr);
    if (FAILED(hr)) return Fail(L"IMFVirtualCamera::Start", hr);
    // No Shutdown: with System lifetime the camera outlives this handle, and
    // releasing it is enough to leave it registered.
    wprintf(L"Registered \"%ls\" (%ls) from %ls\n", kCameraFriendlyName, CLSID_TelePresenceCamera_String,
            dll.c_str());
    return 0;
}

int Remove(const std::wstring& dll) {
    ComPtr<IMFVirtualCamera> cam;
    HRESULT hr = CreateCamera(cam);
    if (SUCCEEDED(hr)) hr = cam->Remove();
    if (FAILED(hr)) fwprintf(stderr, L"removing camera: 0x%08lX (continuing)\n", static_cast<unsigned long>(hr));

    if (GetFileAttributesW(dll.c_str()) != INVALID_FILE_ATTRIBUTES) {
        const HRESULT u = CallExport(dll, "DllUnregisterServer");
        if (FAILED(u)) return Fail(L"DllUnregisterServer", u);
    }
    wprintf(L"Removed \"%ls\"\n", kCameraFriendlyName);
    return FAILED(hr) ? 1 : 0;
}

}  // namespace

int wmain(int argc, wchar_t** argv) {
    bool remove = false;
    std::wstring dll = DefaultDllPath();
    for (int i = 1; i < argc; ++i) {
        if (!wcscmp(argv[i], L"--remove")) {
            remove = true;
        } else if (!wcscmp(argv[i], L"--dll") && i + 1 < argc) {
            dll = argv[++i];
        } else {
            fwprintf(stderr, L"usage: tp-vcam-register [--remove] [--dll PATH]\n");
            return 2;
        }
    }
    CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    MFStartup(MF_VERSION, MFSTARTUP_LITE);
    const int rc = remove ? Remove(dll) : Register(dll);
    MFShutdown();
    CoUninitialize();
    return rc;
}
