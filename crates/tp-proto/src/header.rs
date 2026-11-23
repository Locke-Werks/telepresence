/// Wire version of the datagram header. Bumped whenever the layout changes so a
/// mismatched peer's datagrams are discarded instead of misparsed.
pub const HEADER_VERSION: u8 = 1;
pub const HEADER_LEN: usize = 24;

const FLAG_KEYFRAME: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamKind {
    Video,
    Audio,
}

impl StreamKind {
    fn to_byte(self) -> u8 {
        match self {
            StreamKind::Video => 0,
            StreamKind::Audio => 1,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(StreamKind::Video),
            1 => Some(StreamKind::Audio),
            _ => None,
        }
    }
}

/// Fixed 24-byte header in front of every media datagram, big endian:
///
/// | off | size | field                         |
/// |-----|------|-------------------------------|
/// | 0   | 1    | header version                |
/// | 1   | 1    | stream kind                   |
/// | 2   | 1    | flags (bit 0 keyframe)        |
/// | 3   | 1    | reserved, zero                |
/// | 4   | 4    | datagram sequence (per kind)  |
/// | 8   | 8    | timestamp, microseconds       |
/// | 16  | 4    | frame id (per kind)           |
/// | 20  | 2    | fragment index                |
/// | 22  | 2    | fragment count                |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaHeader {
    pub kind: StreamKind,
    pub sequence: u32,
    pub timestamp_us: u64,
    pub frame_id: u32,
    pub frag_index: u16,
    pub frag_count: u16,
    pub keyframe: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HeaderError {
    #[error("datagram shorter than header")]
    Short,
    #[error("unsupported header version {0}")]
    Version(u8),
    #[error("unknown stream kind {0}")]
    Kind(u8),
    #[error("fragment index out of range")]
    Fragment,
}

impl MediaHeader {
    pub fn encode(&self, out: &mut [u8; HEADER_LEN]) {
        out[0] = HEADER_VERSION;
        out[1] = self.kind.to_byte();
        out[2] = if self.keyframe { FLAG_KEYFRAME } else { 0 };
        out[3] = 0;
        out[4..8].copy_from_slice(&self.sequence.to_be_bytes());
        out[8..16].copy_from_slice(&self.timestamp_us.to_be_bytes());
        out[16..20].copy_from_slice(&self.frame_id.to_be_bytes());
        out[20..22].copy_from_slice(&self.frag_index.to_be_bytes());
        out[22..24].copy_from_slice(&self.frag_count.to_be_bytes());
    }

    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        self.encode(&mut b);
        b
    }

    /// Parses the header and returns it with the payload that follows.
    pub fn decode(buf: &[u8]) -> Result<(Self, &[u8]), HeaderError> {
        if buf.len() < HEADER_LEN {
            return Err(HeaderError::Short);
        }
        if buf[0] != HEADER_VERSION {
            return Err(HeaderError::Version(buf[0]));
        }
        let kind = StreamKind::from_byte(buf[1]).ok_or(HeaderError::Kind(buf[1]))?;
        let be16 = |o: usize| u16::from_be_bytes([buf[o], buf[o + 1]]);
        let be32 = |o: usize| u32::from_be_bytes(buf[o..o + 4].try_into().unwrap());
        let h = MediaHeader {
            kind,
            keyframe: buf[2] & FLAG_KEYFRAME != 0,
            sequence: be32(4),
            timestamp_us: u64::from_be_bytes(buf[8..16].try_into().unwrap()),
            frame_id: be32(16),
            frag_index: be16(20),
            frag_count: be16(22),
        };
        if h.frag_count == 0 || h.frag_index >= h.frag_count {
            return Err(HeaderError::Fragment);
        }
        Ok((h, &buf[HEADER_LEN..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let h = MediaHeader {
            kind: StreamKind::Audio,
            sequence: 0xDEAD_BEEF,
            timestamp_us: 0x0123_4567_89AB_CDEF,
            frame_id: 77,
            frag_index: 3,
            frag_count: 9,
            keyframe: true,
        };
        let mut buf = h.to_bytes().to_vec();
        buf.extend_from_slice(b"payload");
        let (d, payload) = MediaHeader::decode(&buf).unwrap();
        assert_eq!(d, h);
        assert_eq!(payload, b"payload");
    }

    #[test]
    fn layout_is_stable() {
        let h = MediaHeader {
            kind: StreamKind::Video,
            sequence: 1,
            timestamp_us: 2,
            frame_id: 3,
            frag_index: 4,
            frag_count: 5,
            keyframe: false,
        };
        assert_eq!(
            h.to_bytes(),
            [1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 3, 0, 4, 0, 5]
        );
    }

    #[test]
    fn rejects_bad_input() {
        let good = MediaHeader {
            kind: StreamKind::Video,
            sequence: 0,
            timestamp_us: 0,
            frame_id: 0,
            frag_index: 0,
            frag_count: 1,
            keyframe: false,
        }
        .to_bytes();
        assert_eq!(MediaHeader::decode(&good[..10]), Err(HeaderError::Short));
        let mut b = good;
        b[0] = 9;
        assert_eq!(MediaHeader::decode(&b), Err(HeaderError::Version(9)));
        let mut b = good;
        b[1] = 7;
        assert_eq!(MediaHeader::decode(&b), Err(HeaderError::Kind(7)));
        let mut b = good;
        b[21] = 1; // index 1 of count 1
        assert_eq!(MediaHeader::decode(&b), Err(HeaderError::Fragment));
        let mut b = good;
        b[23] = 0; // count 0
        assert_eq!(MediaHeader::decode(&b), Err(HeaderError::Fragment));
    }
}
