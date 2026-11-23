#include <windows.h>

#include <string>

#include "activator.h"
#include "guids.h"

using Microsoft::WRL::InProc;
using Microsoft::WRL::Module;

namespace {

HMODULE g_module = nullptr;

class ClassFactory : public RuntimeClass<RuntimeClassFlags<ClassicCom>, IClassFactory> {
public:
    STDMETHODIMP CreateInstance(IUnknown* outer, REFIID riid, void** out) override {
        if (!out) return E_POINTER;
        *out = nullptr;
        if (outer) return CLASS_E_NOAGGREGATION;
        ComPtr<Activator> a;
        HRESULT hr = Microsoft::WRL::MakeAndInitialize<Activator>(&a);
        return FAILED(hr) ? hr : a.CopyTo(riid, out);
    }
    STDMETHODIMP LockServer(BOOL lock) override {
        auto& m = Module<InProc>::GetModule();
        lock ? m.IncrementObjectCount() : m.DecrementObjectCount();
        return S_OK;
    }
};

const std::wstring kClsidKey = std::wstring(L"SOFTWARE\\Classes\\CLSID\\") + CLSID_TelePresenceCamera_String;

LSTATUS SetString(HKEY key, const wchar_t* name, const std::wstring& value) {
    return RegSetValueExW(key, name, 0, REG_SZ, reinterpret_cast<const BYTE*>(value.c_str()),
                          static_cast<DWORD>((value.size() + 1) * sizeof(wchar_t)));
}

}  // namespace

BOOL APIENTRY DllMain(HMODULE module, DWORD reason, LPVOID) {
    if (reason == DLL_PROCESS_ATTACH) {
        g_module = module;
        DisableThreadLibraryCalls(module);
    }
    return TRUE;
}

STDAPI DllCanUnloadNow() {
    return Module<InProc>::GetModule().GetObjectCount() == 0 ? S_OK : S_FALSE;
}

STDAPI DllGetClassObject(REFCLSID clsid, REFIID riid, LPVOID* out) {
    if (!out) return E_POINTER;
    *out = nullptr;
    if (clsid != CLSID_TelePresenceCamera) return CLASS_E_CLASSNOTAVAILABLE;
    // Instantiating the module first makes every WRL object count towards
    // DllCanUnloadNow.
    Module<InProc>::GetModule();
    auto f = Microsoft::WRL::Make<ClassFactory>();
    return f ? f.CopyTo(riid, out) : E_OUTOFMEMORY;
}

// HKLM rather than HKCU: FrameServer runs as LocalService and never sees the
// registering user's hive.
STDAPI DllRegisterServer() {
    wchar_t path[MAX_PATH];
    const DWORD n = GetModuleFileNameW(g_module, path, MAX_PATH);
    if (n == 0 || n == MAX_PATH) return HRESULT_FROM_WIN32(GetLastError());

    HKEY clsid = nullptr, inproc = nullptr;
    LSTATUS s = RegCreateKeyExW(HKEY_LOCAL_MACHINE, kClsidKey.c_str(), 0, nullptr, 0, KEY_WRITE, nullptr, &clsid,
                                nullptr);
    if (s != ERROR_SUCCESS) return HRESULT_FROM_WIN32(s);
    s = SetString(clsid, nullptr, L"TelePresence Camera Source");
    if (s == ERROR_SUCCESS)
        s = RegCreateKeyExW(clsid, L"InprocServer32", 0, nullptr, 0, KEY_WRITE, nullptr, &inproc, nullptr);
    if (s == ERROR_SUCCESS) s = SetString(inproc, nullptr, path);
    if (s == ERROR_SUCCESS) s = SetString(inproc, L"ThreadingModel", L"Both");
    if (inproc) RegCloseKey(inproc);
    RegCloseKey(clsid);
    return HRESULT_FROM_WIN32(s);
}

STDAPI DllUnregisterServer() {
    const LSTATUS s = RegDeleteTreeW(HKEY_LOCAL_MACHINE, kClsidKey.c_str());
    return (s == ERROR_SUCCESS || s == ERROR_FILE_NOT_FOUND) ? S_OK : HRESULT_FROM_WIN32(s);
}
