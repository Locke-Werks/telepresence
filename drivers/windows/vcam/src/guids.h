#pragma once

#include <windows.h>

// CLSID of the activator FrameServer instantiates. Changing it orphans every
// existing registration, so treat it as permanent.
inline constexpr GUID CLSID_TelePresenceCamera = {
    0x955ad2b2, 0xd1f7, 0x40bf, {0x8a, 0xea, 0xde, 0xd7, 0x2b, 0x64, 0x4e, 0x57}};
inline constexpr wchar_t CLSID_TelePresenceCamera_String[] = L"{955AD2B2-D1F7-40BF-8AEA-DED72B644E57}";
inline constexpr wchar_t kCameraFriendlyName[] = L"TelePresence Camera";

// PINNAME_VIDEO_CAPTURE from ksmedia.h, spelled out to avoid pulling the KS
// headers and their initguid ordering rules into every translation unit.
inline constexpr GUID kPinNameVideoCapture = {
    0xfb6c4281, 0x0353, 0x11d1, {0x90, 0x5f, 0x00, 0x00, 0xc0, 0xcc, 0x16, 0xba}};
