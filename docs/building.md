# Building

## Common requirements

- Rust stable (edition 2024).
- GStreamer 1.24 or later, runtime and development files.
- Qt 6 with Qt Quick and Qt Quick Controls, for the GUI. Releases use Qt
  6.11.3. Without Qt, build with `--no-default-features` for a CLI-only
  binary.

```
cargo build --release                         # GUI + CLI
cargo build --release --no-default-features   # CLI only, no Qt
cargo test --workspace
```

The binary is `target/release/telepresence`. Tests need a working GStreamer;
the loopback tests run a Sender and Receiver in one process over localhost
with test sources.

Useful examples:

```
cargo run -p tp-media --example native_mode     # show the mode chosen for each camera
cargo run -p tp-media --example mic_capture
cargo run -p tp-sink  --example write_pattern   # feed a test pattern to the virtual camera
```

To run two instances on one machine, give each its own identity with
`--config-dir` or `TELEPRESENCE_CONFIG_DIR`.

## Windows

Requirements:

- Visual Studio 2022 or later with the C++ workload, and CMake 3.21+.
- Qt 6 for MSVC 2022 x64 (for example `C:\Qt\6.11.3\msvc2022_64`). Set
  `QMAKE` or put its `bin` on `PATH` so cxx-qt finds it.
- GStreamer MSVC x86_64 runtime and devel MSIs from
  [gstreamer.freedesktop.org](https://gstreamer.freedesktop.org/download/),
  installed with all features. The installer sets
  `GSTREAMER_1_0_ROOT_MSVC_X86_64`; put its `bin` on `PATH` and point
  `PKG_CONFIG_PATH` at its `lib\pkgconfig`.

### Virtual camera

```
cmake -S drivers/windows/vcam -B drivers/windows/vcam/build
cmake --build drivers/windows/vcam/build --config Release
cmake --install drivers/windows/vcam/build --config Release   # elevated
"C:\Program Files\TelePresence\VirtualCamera\tp-vcam-register.exe"   # elevated
```

Install before registering: Frame Server runs as LocalService and cannot read
a DLL under a user profile. `tp-vcam-register.exe --remove` unregisters.

To replace a registered DLL, stop Frame Server first, since it keeps the DLL
loaded:

```
net stop FrameServerMonitor
net stop FrameServer
```

Both services are demand-start and come back when an app opens a camera.

With the camera registered, run `telepresence receive` (or
`cargo run -p tp-sink --example write_pattern`) and open the Camera app.

### Installer

`packaging/windows/package.ps1` runs the whole local release path: build,
sign, stage, forge with Forge `lwforge` (pinned to v0.4.1), sign the
installer.

```
pwsh packaging/windows/package.ps1 -NoSign            # unsigned dev installer
pwsh packaging/windows/package.ps1 -CredsFile <path>  # signed
```

Output: `target\windows\dist\telepresence-<version>-windows-x64-setup.exe`.
An unsigned build is forged with `--dev`, which the installer displays.

`stage.ps1` alone builds (`-Phase build`) or assembles the payload
(`-Phase assemble`) into `target\windows\stage`: the app, the camera DLL and
register tool under `VirtualCamera\`, `tp-setup-helper.exe`, Qt via
windeployqt, and only the GStreamer plugins the app uses plus their
dependencies.

Installer hooks (`packaging/windows/installer.toml`):

| Phase | Action |
| --- | --- |
| `pre_install` | Close a running copy; stop Frame Server so the DLL can be replaced. |
| `post_install` | Start Frame Server; register the camera. |
| `pre_uninstall` | Close the app; unregister the camera; stop Frame Server. |

`tp-setup-helper` is statically linked because `pre_install` runs before
the rest of the payload exists.

## macOS

Requirements: Xcode command line tools, Qt 6 for macOS, and the official
`GStreamer.framework` runtime and devel packages.

```
packaging/macos/build-app.sh                 # host arch
ARCHS="arm64 x86_64" packaging/macos/build-app.sh   # universal
packaging/macos/sign-notarize.sh             # Developer ID sign, notarize, staple, dmg + zip
```

`build-app.sh` produces an unsigned `target/macos/TelePresence.app` with Qt
from macdeployqt and a private GStreamer under `Contents/Frameworks` and
`Contents/PlugIns/gstreamer`. The plugin list is in the script: an element
added to the pipeline code without its plugin added there fails at runtime,
not at build time.

The app runs under the hardened runtime without the JIT entitlement, so ORC
runs its C fallback paths (`ORC_CODE=backup` is set at startup).

## Linux

```
packaging/linux/build.sh [version]
```

Targets Ubuntu 24.04, the oldest release with GStreamer 1.24. Run it as root
in an `ubuntu:24.04` container or as a sudo user on a noble host; it installs
its own apt dependencies (`SKIP_DEPS=1` skips that). Output goes to
`dist/linux/`: a `.deb` and an AppImage. It builds into `target/linux` so it
does not collide with a Windows or macOS build in the same checkout.
