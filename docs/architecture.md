# Architecture

TelePresence is a Rust workspace plus native virtual-device drivers. The Rust
side owns capture, encoding, transport, discovery and the UI. The drivers are
small native components that run inside OS media services and read decoded
frames out of shared memory.

```
 Sender machine                                Receiver machine
 ------------------------------------          --------------------------------------------
 camera, mic                                    telepresence (Receiver)
   -> tp-media: capture, H.264 / Opus encode      tp-proto: QUIC session, reassembly
   -> tp-proto: QUIC datagrams  ===== LAN =====>  tp-media: decode to NV12 / f32 PCM
   <- Stats, RequestKeyframe (control stream)     tp-sink: shared-memory writer
                                                    |
                                                    +--> tp-vcam.dll in Frame Server
                                                    |      -> "TelePresence Camera"
                                                    +--> VB-Cable "CABLE Input"
                                                           -> "CABLE Output" mic
```

## Crates

| Crate | Role |
| --- | --- |
| `tp-proto` | QUIC transport (quinn, rustls with ring). Control stream, media datagrams, fragmentation and reassembly, certificate identity, SPAKE2 pairing, LAN address filter. No media knowledge. See [protocol.md](protocol.md). |
| `tp-media` | GStreamer pipelines. Device enumeration, native camera mode selection, H.264 encoder and decoder selection, Opus encode and decode, audio output. Transport-agnostic: hands out and accepts encoded units. |
| `tp-discovery` | mDNS / DNS-SD. Receivers advertise `_telepresence._udp.local.`; Senders browse it. |
| `tp-sink` | Shared-memory writer and reference reader for the virtual devices. The layout is specified in [sink-format.md](sink-format.md) and mirrored in `drivers/common/tp_sink.h`. |
| `tp-app` | Application core and the `telepresence` binary. `run_sender` and `run_receiver` drive everything above; the CLI and the Qt Quick front end both call them and consume the same `Event` stream, so neither front end holds protocol logic. Also: adaptive bitrate (`abr.rs`), on-disk identity and pins (`store.rs`), the updater (`update.rs`). |

The Qt front end is cxx-qt 0.10 behind the `gui` feature (on by default).
`--no-default-features` builds a CLI-only binary with no Qt dependency.
QML lives in `crates/tp-app/qml/`; `video_sink.cpp` is the C++ side of the
receiver preview.

## Drivers

| Path | Status | Runs in |
| --- | --- | --- |
| `drivers/windows/vcam` | Shipping | Windows Frame Server, as LocalService |
| `drivers/windows/vmic` | Placeholder; VB-Cable is used meanwhile | |
| `drivers/macos/camera-extension` | Planned (CoreMediaIO camera extension) | sandboxed extension |
| `drivers/macos/hal-plugin` | Planned (AudioServerPlugIn) | coreaudiod |
| `drivers/linux` | Planned (v4l2loopback or PipeWire bridge) | |
| `drivers/common/tp_sink.h` | C/C++ reader for the shared-memory format | |

### Windows virtual camera

`tp-vcam.dll` is a Media Foundation media source registered as a COM server
and as a software camera with `MFCreateVirtualCamera` (system lifetime, all
users). Frame Server loads it when any app opens the camera. It:

- creates `Global\TelePresence.Sink.Video.v1` if it does not exist, because
  a standard user's Receiver lacks `SeCreateGlobalPrivilege`;
- reads the newest NV12 frame from the seqlocked triple buffer;
- offers the writer's native size first, then fixed 720p and 1080p types,
  in NV12, YUY2 and RGB32, converting and letterboxing as needed;
- shows black when the writer's heartbeat is stale.

`tp-vcam-register.exe` adds or (`--remove`) removes the registration and
needs elevation. The DLL must live in a location LocalService can read, so
it is installed under `C:\Program Files\TelePresence\VirtualCamera`, never
under a user profile. Both are built with the static CRT so a missing VC++
redistributable cannot break loading inside the service.

## Media path

Sender:

1. `tp-media::choose_mode` picks the camera's native mode, ranked NV12,
   YUY2, UYVY, I420, other raw, MJPEG, capped at 60 fps. `--size` and
   `--fps` override it.
2. The first available H.264 encoder from NVENC, Quick Sync, Media
   Foundation, VideoToolbox, VA-API, x264 is used, tuned for low latency
   with a 10-second GOP. Keyframes are requested on demand.
3. Opus at 48 kHz.
4. `tp-proto` fragments each access unit to the path's datagram size and
   sends it unreliably.
5. Once a second the Receiver reports loss; `abr::BitrateController` cuts
   the bitrate by 20% above 2% loss and raises it by 5% (at least 50 kbit/s)
   after three reports under 0.5%.

Receiver:

1. Reassembles fragments with a 100 ms deadline. A frame with a missing
   fragment is dropped and counted as loss; the Receiver then asks for a
   keyframe, at most once per 250 ms.
2. Decodes to NV12 and writes it to the video sink.
3. Decodes audio and writes it to the audio sink and to the chosen output
   (VB-Cable by default on Windows).
4. Optionally feeds every frame to the preview in the window.

## Trust model

- Each install has a self-signed certificate per role (`sender/`,
  `receiver/` under the config directory). The fingerprint is SHA-256 of
  its DER encoding.
- First contact: the Receiver shows a six-digit code, valid for 5 minutes and
  three wrong guesses. A new code is issued at most every 10 seconds. The
  Sender enters it; SPAKE2 bound to both fingerprints derives a key and both
  sides exchange HMAC confirmation tags. On success each side pins the other.
- Later connections check the pinned fingerprint and skip the code.
- Addresses outside loopback, RFC 1918, link-local and IPv6 ULA are refused
  before any handshake.

## Platform runtime setup

Release builds bundle their own GStreamer. At startup, before any thread
exists, `main.rs` points the plugin path, plugin scanner and registry file at
the bundled copy so a system-wide GStreamer cannot change which plugins load.
Development builds (no bundle next to the exe) use the system install.

On Windows the release exe is GUI-subsystem; when started with arguments it
attaches to the parent console so CLI output still appears.
