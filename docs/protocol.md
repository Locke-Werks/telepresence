# Wire protocol

Protocol version 1. Source of truth: `crates/tp-proto`.

## Transport

- QUIC (quinn) over UDP, default port 47800 on the Receiver.
- TLS 1.3 with self-signed certificates on both sides. ALPN
  `telepresence/1`, server name `telepresence`. Certificate validity is
  decided by fingerprint pinning or pairing, not by a CA.
- Keep-alive every 2 s, idle timeout 10 s, handshake timeout 10 s.
- Peers whose address is not loopback, RFC 1918, IPv4 link-local, IPv6 ULA
  (`fc00::/7`) or IPv6 link-local (`fe80::/10`) are refused. IPv4-mapped IPv6
  addresses are judged as IPv4.

The Sender is the QUIC client, the Receiver the server.

## Control stream

The Sender opens one bidirectional stream. Every message on it is a 4-byte
big-endian length followed by a [postcard](https://docs.rs/postcard)
encoding. Frames over 64 KiB are a protocol error.

### Authentication phase

Before the stream is handed to the application, the two sides exchange
`AuthMessage`s:

| Message | Direction | Meaning |
| --- | --- | --- |
| `Pinned` | S -> R | Sender believes both sides already pinned each other. |
| `PairStart { spake }` | S -> R | Sender's SPAKE2 message. |
| `PairReply { spake, confirm }` | R -> S | Receiver's SPAKE2 message and confirmation tag. |
| `PairConfirm { confirm }` | S -> R | Sender's confirmation tag. |
| `Accept` | R -> S | Session established. |
| `Reject { reason }` | R -> S | Refused; the connection closes shortly after. |

Pairing uses SPAKE2 over Ed25519 with the six-digit code as the password and
the Sender and Receiver certificate fingerprints as the two identities.
Confirmation tags are `HMAC-SHA256(key, label || sender_fp || receiver_fp)`
with labels `tp-confirm-sender` and `tp-confirm-receiv`, compared in constant
time. A man in the middle terminating TLS on either side sees different
fingerprints and fails confirmation; an eavesdropper gets one online guess
per attempt. The Receiver closes pairing after three failures.

QUIC close codes:

| Code | Meaning |
| --- | --- |
| `0x01` | Handshake failed |
| `0x10` | Not paired (Sender claimed `Pinned`, Receiver does not know it) |
| `0x11` | Pairing failed (wrong code) |
| `0x12` | Receiver is not in pairing mode |
| `0x13` | Protocol version mismatch |

On `0x10` the Sender unpins the Receiver so the next attempt asks for a code.

### Application phase

`ControlMessage`:

| Message | Direction | Meaning |
| --- | --- | --- |
| `Hello { version, name }` | both | Protocol version and display name. Versions must match exactly. |
| `StreamConfig { video, audio }` | S -> R | Codec, size, rate, channels. |
| `RequestKeyframe` | R -> S | Decoder lost sync. Rate-limited by the Receiver to one per 250 ms. |
| `Stats { loss, rtt_ms }` | R -> S | Sent once a second; drives the Sender's bitrate controller. |
| `Bye` | both | Clean shutdown. |

Codecs defined: video `H264`, `Hevc`, `Av1`, `Raw`; audio `Opus`, `PcmS16`.
The current implementation sends H.264 and Opus.

## Media datagrams

Every encoded access unit or audio packet is split into QUIC datagrams no
larger than the connection's current maximum datagram size. Each carries a
24-byte big-endian header:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | header version (1) |
| 1 | 1 | stream kind: 0 video, 1 audio |
| 2 | 1 | flags: bit 0 keyframe |
| 3 | 1 | reserved, zero |
| 4 | 4 | datagram sequence, per kind |
| 8 | 8 | timestamp, microseconds |
| 16 | 4 | frame id, per kind |
| 20 | 2 | fragment index |
| 22 | 2 | fragment count |

Datagrams with an unknown header version or kind are discarded.

The receiving side reassembles by `(kind, frame_id)`. An incomplete frame
expires after the reassembly deadline (100 ms by default) and counts as loss.
At most 64 partial frames are held per kind. Gaps in the datagram sequence
feed the loss figure reported in `Stats`.

## Discovery

Receivers advertise DNS-SD service `_telepresence._udp.local.` on their QUIC
port with TXT keys:

| Key | Value |
| --- | --- |
| `v` | discovery protocol version (1) |
| `id` | stable install id |
| `n` | display name |
| `fp` | hex SHA-256 of the Receiver certificate |
| `paired` | `1` when the Receiver has at least one pinned Sender |

Discovery is a convenience. The `fp` value is not trusted; the TLS handshake
and pin check are what authenticate the Receiver.

## Versioning

`PROTOCOL_VERSION` (in `Hello`) and `HEADER_VERSION` (in each datagram) are
bumped on any incompatible change. Peers with different versions refuse each
other rather than attempt compatibility.
