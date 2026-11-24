// tp-setup-helper: the installer's pre_install / pre_uninstall / post_install
// hook. It runs before the rest of the payload is on disk (pre_install), so it
// links nothing but the OS and a static CRT.
//
//   tp-setup-helper stop-app <InstallDir>   close telepresence.exe running from InstallDir
//   tp-setup-helper stop-frameserver        stop FrameServerMonitor and FrameServer
//   tp-setup-helper start-frameserver       start FrameServer and FrameServerMonitor
//
// Every command exits 0 when there was nothing to do, so a fresh install or a
// machine without Frame Server is not reported as a failed hook.

#include <windows.h>
#include <tlhelp32.h>

#include <cstdio>
#include <cwchar>
#include <string>
#include <vector>

namespace {

std::wstring Lower(std::wstring s) {
    for (auto& c : s) c = static_cast<wchar_t>(towlower(c));
    return s;
}

std::wstring WithSlash(std::wstring s) {
    if (!s.empty() && s.back() != L'\\') s.push_back(L'\\');
    return s;
}

struct CloseCtx {
    DWORD pid;
    bool posted;
};

BOOL CALLBACK CloseTopLevel(HWND hwnd, LPARAM lp) {
    auto* ctx = reinterpret_cast<CloseCtx*>(lp);
    DWORD pid = 0;
    GetWindowThreadProcessId(hwnd, &pid);
    if (pid == ctx->pid && IsWindowVisible(hwnd)) {
        PostMessageW(hwnd, WM_CLOSE, 0, 0);
        ctx->posted = true;
    }
    return TRUE;
}

// Only copies under the install dir: a dev build running from a source tree
// is somebody's work in progress and is not ours to close.
int StopApp(const std::wstring& install_dir) {
    const std::wstring prefix = Lower(WithSlash(install_dir));
    std::vector<DWORD> pids;
    HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if (snap == INVALID_HANDLE_VALUE) return 0;
    PROCESSENTRY32W pe{sizeof(pe)};
    for (BOOL ok = Process32FirstW(snap, &pe); ok; ok = Process32NextW(snap, &pe)) {
        if (_wcsicmp(pe.szExeFile, L"telepresence.exe") != 0) continue;
        HANDLE h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pe.th32ProcessID);
        if (!h) continue;
        wchar_t path[MAX_PATH * 2];
        DWORD n = ARRAYSIZE(path);
        if (QueryFullProcessImageNameW(h, 0, path, &n) && Lower(path).rfind(prefix, 0) == 0)
            pids.push_back(pe.th32ProcessID);
        CloseHandle(h);
    }
    CloseHandle(snap);

    int rc = 0;
    for (DWORD pid : pids) {
        HANDLE h = OpenProcess(SYNCHRONIZE | PROCESS_TERMINATE, FALSE, pid);
        if (!h) continue;
        // Ask first, so the app winds its streams down and saves settings.
        CloseCtx ctx{pid, false};
        EnumWindows(CloseTopLevel, reinterpret_cast<LPARAM>(&ctx));
        if (!ctx.posted || WaitForSingleObject(h, 5000) != WAIT_OBJECT_0) {
            if (!TerminateProcess(h, 1)) {
                fwprintf(stderr, L"could not stop telepresence.exe (pid %lu): %lu\n", pid, GetLastError());
                rc = 1;
            } else {
                WaitForSingleObject(h, 5000);
            }
        }
        wprintf(L"stopped telepresence.exe (pid %lu)\n", pid);
        CloseHandle(h);
    }
    return rc;
}

bool WaitState(SC_HANDLE svc, DWORD want, DWORD timeout_ms) {
    const ULONGLONG until = GetTickCount64() + timeout_ms;
    SERVICE_STATUS_PROCESS st{};
    DWORD need = 0;
    while (QueryServiceStatusEx(svc, SC_STATUS_PROCESS_INFO, reinterpret_cast<BYTE*>(&st), sizeof(st), &need)) {
        if (st.dwCurrentState == want) return true;
        if (GetTickCount64() > until) return false;
        Sleep(200);
    }
    return false;
}

// FrameServer keeps tp-vcam.dll loaded while the camera is registered, and a
// loaded DLL cannot be replaced or deleted. Both services are demand-start:
// Windows brings them back the next time anything opens a camera.
int Service(const wchar_t* name, bool start) {
    SC_HANDLE scm = OpenSCManagerW(nullptr, nullptr, SC_MANAGER_CONNECT);
    if (!scm) return 1;
    SC_HANDLE svc = OpenServiceW(scm, name, SERVICE_QUERY_STATUS | SERVICE_START | SERVICE_STOP);
    int rc = 0;
    if (!svc) {
        // Server SKUs and N editions can lack Frame Server; nothing to unlock.
        CloseServiceHandle(scm);
        return 0;
    }
    if (start) {
        if (!StartServiceW(svc, 0, nullptr) && GetLastError() != ERROR_SERVICE_ALREADY_RUNNING) {
            fwprintf(stderr, L"start %ls: %lu\n", name, GetLastError());
            rc = 1;
        } else if (!WaitState(svc, SERVICE_RUNNING, 15000)) {
            fwprintf(stderr, L"%ls did not reach RUNNING\n", name);
        }
    } else {
        SERVICE_STATUS st{};
        if (!ControlService(svc, SERVICE_CONTROL_STOP, &st) && GetLastError() != ERROR_SERVICE_NOT_ACTIVE) {
            fwprintf(stderr, L"stop %ls: %lu\n", name, GetLastError());
            rc = 1;
        } else if (!WaitState(svc, SERVICE_STOPPED, 30000)) {
            // ControlService accepting the stop is not the DLL being unloaded.
            fwprintf(stderr, L"%ls did not reach STOPPED\n", name);
            rc = 1;
        }
    }
    CloseServiceHandle(svc);
    CloseServiceHandle(scm);
    wprintf(L"%ls %ls\n", name, start ? L"started" : L"stopped");
    return rc;
}

}  // namespace

int wmain(int argc, wchar_t** argv) {
    if (argc == 3 && !wcscmp(argv[1], L"stop-app")) return StopApp(argv[2]);
    if (argc == 2 && !wcscmp(argv[1], L"stop-frameserver")) {
        // Monitor first: it restarts FrameServer if FrameServer goes away under it.
        int rc = Service(L"FrameServerMonitor", false);
        rc |= Service(L"FrameServer", false);
        return rc;
    }
    if (argc == 2 && !wcscmp(argv[1], L"start-frameserver")) {
        int rc = Service(L"FrameServer", true);
        rc |= Service(L"FrameServerMonitor", true);
        return rc;
    }
    fwprintf(stderr, L"usage: tp-setup-helper stop-app <dir> | stop-frameserver | start-frameserver\n");
    return 2;
}
