# tp-sink shared-memory format (video version 2, audio version 1)

The Receiver decodes video and audio and hands them to virtual device drivers
running in other processes: the Windows Media Foundation virtual camera
(loaded by FrameServer as LocalService), the macOS CoreMediaIO camera
extension (sandboxed), the macOS AudioServerPlugIn (inside coreaudiod), and
later Linux. tp-sink is the transport: two named shared-memory regions, one
writer (the Receiver), any number of readers.

Sources of truth:

- Rust: `crates/tp-sink/src/layout.rs` (writer and reference reader)
- C/C++: `drivers/common/tp_sink.h`

Both carry static size and offset assertions. A layout change bumps
`version` and must land in both files at once.

## Conventions

- Little-endian, fixed-width integers. Offsets below are bytes from the start
  of the region.
- Every integer field is a lock-free 32- or 64-bit atomic with the same size
  and alignment as the plain integer. Headers are 64-byte aligned and
  writer-owned and reader-owned fields sit on separate 64-byte lines.
- Line 0 of each header is layout: written once, before `magic` is published
  with release ordering. A reader loads `magic` with acquire ordering and may
  then treat line 0 as constant. `magic == 0` means a writer is (re)building
  the region; treat as no signal.
- Timestamps and heartbeats are wall-clock microseconds since the Unix epoch.
  The processes do not share a monotonic clock base.
- **Liveness**: the writer stores `heartbeat_us` on every frame or audio write
  and at least every 100 ms while idle. A reader treats the writer as absent
  when `heartbeat_us == 0` or `now - heartbeat_us > 500000`, and then shows a
  black frame or plays silence. A heartbeat in the future counts as live so a
  clock step back does not blank the device. The writer zeroes the heartbeat
  on clean shutdown so drivers react at once.

## Object names

| Platform | Video | Audio |
| --- | --- | --- |
| Windows | `Global\TelePresence.Sink.Video.v1` | `Global\TelePresence.Sink.Audio.v1` |
| macOS, Linux | `/tp.sink.video.v1` | `/tp.sink.audio.v1` |

Names are configurable on both sides; the above are defaults.

### Windows

Pagefile-backed sections from `CreateFileMappingW(INVALID_HANDLE_VALUE, ...)`.
The `Global\` prefix is required because FrameServer runs in session 0 and the
Receiver runs in the user's session; `Local\` objects are per session.

Security descriptor applied at creation:

```
D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;GRGW;;;LS)(A;;GR;;;AU)
```

| ACE | Who | Rights |
| --- | --- | --- |
| `SY` | LocalSystem | full |
| `BA` | Administrators | full |
| `OW` | owner (creating user) | full |
| `LS` | LocalService, which FrameServer runs as | read and write |
| `AU` | Authenticated Users | read |

`P` blocks inherited ACEs. On a section, `GR` maps to `SECTION_QUERY |
SECTION_MAP_READ` and `GW` to `SECTION_MAP_WRITE`. Other users can read but
not write, so a second logged-on user cannot inject frames into the camera.
Readers open with `FILE_MAP_READ`; the audio reader asks for write access
only to publish its cursor and falls back to read-only.

Creating a `Global\` object needs `SeCreateGlobalPrivilege`. Administrators
and service accounts hold it; a standard interactive user does not, and
neither does an administrator's UAC-filtered token. So for video the camera
owns the section:

1. The virtual camera's media source runs as LocalService inside FrameServer,
   which does hold the privilege. When FrameServer activates it, it creates
   `Global\TelePresence.Sink.Video.v1` if it does not exist, sized for
   3840x2160 (`4096 + 3 * 12443648` = 37335040 bytes), and leaves it
   zero-filled. A zero `magic` reads as no signal.
2. The Receiver opens that section read-write as a writer
   (`VideoWriter::open_existing`). A zeroed header is initialised exactly as
   a freshly created region would be; a valid header from an earlier writer
   is adopted. Nothing is recreated, because FrameServer holds the handle.
3. `VideoWriter::open_or_create` tries that first, then creating the name
   (works when elevated), then, if the name is `Global\...` and creation is
   refused, the same name under `Local\`. The `Local\` fallback is only
   visible in the creating logon session, so it serves tests and development
   and the camera never sees it.

The camera-created section carries a different descriptor from the one above:

```
D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;GRGW;;;LS)(A;;GRGW;;;IU)(A;;GR;;;AU)
```

The owner there is LocalService, not the user, so `IU` (INTERACTIVE) gets
read and write, otherwise a standard-user Receiver could only map it
read-only. The cost: any interactively logged-on user can write frames into
the camera, not only the one running the Receiver. A helper service that
creates the section with the Receiver user's SID would close that, at the
price of another installed component.

The section exists only once something has opened the camera at least once
since boot, and FrameServer keeps the source, and the handle, alive for a
while after the last client closes. A Receiver started before any camera
client sees `NotFound`. It should retry `open_existing` about once a second
while it has no section or is on the `Local\` fallback, and switch over when
the camera's section appears. Once the Receiver has the section open its own
handle keeps it alive, so the camera finds the same one next time.

The 3840x2160 size is a ceiling in bytes, not in shape: any even frame with
no more pixels than that fits, portrait included (`VIDEO_MAX_WIDTH` and
`VIDEO_MAX_HEIGHT` in tp-sink). `open_existing` with a larger maximum fails
with `TooSmall`. A section an older camera build created at 1920x1080 stays
that size until every handle closes, Receiver included.

Media types. When FrameServer builds the camera's media source and a live
writer has set a format, the camera offers the writer's `width x height` at
its `fps_num/fps_den` (30 when unknown) first, in NV12, YUY2 and RGB32, then
the fixed 1280x720 and 1920x1080 types at 30 fps that have fewer pixels than
the source. Larger fixed types are left out because clients that take the
largest type on offer would otherwise pick a 16:9 type over a portrait, 4:3
or smaller source and get it letterboxed. With no live writer only the fixed
types are offered. The camera scales, preserving aspect with black bars, only
when the negotiated type differs from the incoming frame.

The type list is fixed for the life of the media source. FrameServer keeps a
source alive across client sessions and reads its types once, so a writer
that attaches or changes size after the source was built is offered natively
only once FrameServer rebuilds the source (it does so after the camera has
been idle for a while, or on a FrameServer restart). A size change while an
app is streaming does not break the session: frames keep arriving in the
negotiated type, scaled. FrameServer shares one stream among clients and
gives a source no way to change a negotiated type, so no
`MEStreamFormatChanged` is sent; the new native size takes effect when apps
reopen the camera after the source is rebuilt.

Audio is unchanged: the Receiver creates it, or opens an existing one. Tests
use `Local\` names.

A section cannot be resized and lives until its last handle closes, so a
section held open by FrameServer survives a Receiver restart. A restarted
writer gets the existing section from `CreateFileMappingW`
(`ERROR_ALREADY_EXISTS`), validates it, and adopts it. The writer therefore
sizes the video region for a maximum resolution up front. On Windows that
is 3840x2160 whenever the camera created the section (above); a region the
Receiver creates itself, elevated or under `Local\`, can be larger.

### macOS

POSIX `shm_open` + `ftruncate` + `mmap`, mode `0644` (set with `fchmod`
because umask applies at creation). coreaudiod runs as `_coreaudiod`, so it
reads but does not write; the audio reader keeps its cursor locally.

A sandboxed camera extension can only open POSIX shared memory whose name
carries an application group it is entitled to:
`<TEAMID>.<group-id>/<name>`, for example
`KVJL9RB8BY.com.lockewerks.telepresence/video`. The Receiver and the extension
must both list that group in `com.apple.security.application-groups`. The
31-character `PSHMNAMLEN` limit applies to the whole string, so keep the group
and the suffix short. This is why the name is a parameter rather than a
constant.

### Linux

POSIX `shm_open` (`/dev/shm`), same as macOS. A `memfd_create` descriptor
passed over a Unix socket is an alternative for a future v4l2loopback or
PipeWire bridge that cannot share a namespace; the layout is unchanged.

On POSIX the writer does not unlink on exit, so restarts adopt the same object
just as on Windows. If an existing object is too small the writer unlinks it
and creates a new one; readers still mapping the orphan see its heartbeat go
stale and reopen by name (the Rust reader retries every 500 ms).

## Video region

Total size: `4096 + 3 * slot_capacity`, where
`slot_capacity = align_up(max_width * max_height * 3 / 2, 4096)`.

### Header (offset 0, 320 bytes)

| Off | Size | Field | Notes |
| --- | --- | --- | --- |
| 0 | 4 | `magic` | `'TPSV'` = `0x56535054` |
| 4 | 4 | `version` | 2 (1 is still accepted, see Versioning) |
| 8 | 4 | `header_size` | 320 |
| 12 | 4 | `slot_count` | 3 |
| 16 | 8 | `region_size` | bytes the writer laid out; must be <= mapped size |
| 24 | 8 | `data_offset` | 4096 |
| 32 | 8 | `slot_capacity` | bytes per slot buffer, multiple of 4096 |
| 40 | 4 | `max_width` | |
| 44 | 4 | `max_height` | |
| 48 | 16 | reserved | |
| 64 | 4 | `generation` | bumped on any geometry change or writer restart; never 0 once live |
| 68 | 4 | `width` | current geometry (informational; use the slot's) |
| 72 | 4 | `height` | |
| 76 | 4 | `stride` | bytes per row of both planes |
| 80 | 4 | `format` | `'NV12'` = `0x3231564E` |
| 84 | 4 | `latest_slot` | index of the newest complete slot, `0xFFFFFFFF` for none |
| 88 | 8 | `frame_counter` | last published frame number, starts at 1 |
| 96 | 8 | `timestamp_us` | presentation timestamp of that frame |
| 104 | 8 | `heartbeat_us` | liveness, see above |
| 112 | 4 | `writer_pid` | diagnostic |
| 116 | 4 | `fps_num` | source frame rate numerator, 0 when unknown (version 2) |
| 120 | 4 | `fps_den` | denominator, 0 when unknown |
| 124 | 4 | reserved | |
| 128 | 192 | `slots[3]` | slot descriptors |

### Slot descriptor (64 bytes each, at 128, 192, 256)

| Off | Size | Field |
| --- | --- | --- |
| 0 | 8 | `seq` (seqlock: odd while writing) |
| 8 | 8 | `frame_counter` |
| 16 | 8 | `timestamp_us` |
| 24 | 4 | `generation` |
| 28 | 4 | `width` |
| 32 | 4 | `height` |
| 36 | 4 | `stride` |
| 40 | 4 | `format` |
| 44 | 4 | `data_size` = `stride * height * 3 / 2` |
| 48 | 16 | reserved |

Slot `i` pixel data is at `data_offset + i * slot_capacity`: Y plane
(`stride * height` bytes) followed by interleaved CbCr (`stride * height / 2`).
Width and height are even.

### Writer protocol

1. `i = (latest_slot + 1) % 3` (0 if none). This is never the slot readers
   were just told about, so a reader that picked up `latest_slot` has a full
   frame interval before its slot is reused.
2. `slots[i].seq` := next odd value (relaxed), then a release fence.
3. Store descriptor fields and pixel data.
4. `slots[i].seq` := next even value (release).
5. `latest_slot` := i (release), then `frame_counter`, `timestamp_us`,
   `heartbeat_us`.

Frame rate: the writer stores `fps_den` then `fps_num` whenever the source
rate is known or changes (tp-sink `VideoWriter::set_frame_rate(num, den)`).
It is informational and does not bump `generation`.

Resize within capacity: store new `width/height/stride`, then increment
`generation` (release). Frames written afterwards carry the new generation in
their slot descriptor. A resolution larger than `max_width x max_height` needs
a new region (POSIX) or is refused (Windows).

### Reader protocol

```
if magic != TPSV or heartbeat stale: no signal (black)
repeat up to N times:
    i  = latest_slot (acquire); if 0xFFFFFFFF: no signal
    s1 = slots[i].seq (acquire); if odd: retry
    read descriptor; reject if data_size > slot_capacity
    copy data_size bytes from slot i
    acquire fence
    s2 = slots[i].seq (relaxed)
    if s1 == s2: done; the copy is a complete frame
give up this tick, keep showing the previous frame
```

Readers never write to the video region and never block the writer. When the
slot's `generation` differs from the last one seen, reallocate downstream
buffers and renegotiate the media type. The same `frame_counter` twice means
no new frame; repeat the previous one.

## Audio region

Total size: `256 + channels * capacity_frames * 4`.

### Header (offset 0, 192 bytes)

| Off | Size | Field | Notes |
| --- | --- | --- | --- |
| 0 | 4 | `magic` | `'TPSA'` = `0x41535054` |
| 4 | 4 | `version` | 1 |
| 8 | 4 | `header_size` | 192 |
| 12 | 4 | `channels` | 1..8 |
| 16 | 4 | `sample_rate` | 48000 |
| 20 | 4 | `capacity_frames` | 24000 (500 ms) |
| 24 | 4 | `data_offset` | 256 |
| 28 | 4 | reserved | |
| 32 | 8 | `region_size` | |
| 40 | 24 | reserved | |
| 64 | 8 | `write_index` | total frames ever written, monotonic |
| 72 | 8 | `heartbeat_us` | |
| 80 | 8 | `session` | bumped on each writer start; readers resync on change |
| 88 | 4 | `writer_pid` | |
| 92 | 36 | reserved | |
| 128 | 8 | `read_index` | primary reader's cursor, diagnostic only |
| 136 | 56 | reserved | |

Samples are interleaved little-endian IEEE f32. Frame `n` lives at ring
position `n % capacity_frames`, i.e. byte
`256 + (n % capacity_frames) * channels * 4`.

### Writer protocol

Copy the frames into the ring at `write_index % capacity`, wrapping as
needed, then store `write_index += frames` (release) and `heartbeat_us`.
The writer never reads `read_index` and never waits. A burst larger than the
ring keeps only its last `capacity_frames` frames.

### Reader protocol

The reader keeps a private 64-bit cursor.

```
if magic != TPSA or heartbeat stale: output silence
if session changed: reread channels/capacity, cursor = write_index - latency
w = write_index (acquire)
if cursor > w: cursor = w
if w - cursor > capacity * 3/4: cursor = w - latency          (overflow skip)
n = min(w - cursor, wanted)
copy n frames from ring
acquire fence; w2 = write_index
if cursor < w2 - capacity: the writer overran the copy;
    discard it, output silence, cursor = w2 - latency
else: cursor += n; fill the remaining wanted - n frames with zeros (underrun)
optionally store cursor to read_index if mapped writable
```

`latency` is the reader's chosen target distance behind live, typically 10 to
20 ms. The quarter-ring headroom keeps a reader from copying data the writer
is about to overwrite. Multiple readers are allowed; only one should publish
`read_index`.

## Versioning

The video region is version 2, which adds `fps_num`/`fps_den` in space
version 1 left reserved and zero. Video readers accept versions 1 and 2 and
read a version 1 region as frame rate unknown. A writer adopting a live
version 1 region upgrades `version` in place rather than re-laying it out,
so a version 1 writer still attached keeps writing to the same slots. The
audio region stays at version 1.

Readers must refuse a region whose `version` they do not know, and treat it as
no signal. Reserved fields are zero in version 1. Additive changes that fit in
reserved space still bump `version`, because an older reader cannot know a
new field changes semantics.
