//! Where decoded media goes besides the preview. tp-sink (shared memory into
//! the virtual camera and microphone drivers) implements this; until then the
//! receiver runs with [`NullSink`].

/// One decoded NV12 frame: the Y plane, then the interleaved UV plane starting
/// at `uv_offset`, every row `stride` bytes wide.
#[derive(Debug, Clone, Copy)]
pub struct VideoFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub uv_offset: usize,
    pub data: &'a [u8],
    pub pts_us: u64,
    /// Source frame rate as announced by the sender, (0, 0) when unknown.
    pub fps_num: u32,
    pub fps_den: u32,
}

/// Interleaved f32 samples.
#[derive(Debug, Clone, Copy)]
pub struct AudioBlock<'a> {
    pub rate: u32,
    pub channels: u32,
    pub samples: &'a [f32],
    pub pts_us: u64,
}

/// Called from GStreamer streaming threads, so implementations must not block
/// for long: a slow sink stalls decode.
pub trait FrameSink: Send + Sync + 'static {
    fn video(&self, frame: &VideoFrame<'_>);
    fn audio(&self, block: &AudioBlock<'_>);
}

/// Feeds the virtual camera through tp-sink shared memory.
///
/// On Windows the section is created by the camera's media source inside
/// Frame Server (LocalService holds SeCreateGlobalPrivilege, we do not), and
/// only once some app opens the camera. So the writer is opened lazily and
/// retried; frames before then are dropped, which is fine because nobody is
/// watching yet.
pub struct ShmSink {
    state: std::sync::Mutex<ShmState>,
}

struct ShmState {
    writer: Option<tp_sink::VideoWriter>,
    last_attempt: Option<std::time::Instant>,
    scratch: Vec<u8>,
}

// The camera sizes its section for this ceiling; asking for more fails.
const MAX_W: u32 = tp_sink::VIDEO_MAX_WIDTH;
const MAX_H: u32 = tp_sink::VIDEO_MAX_HEIGHT;
const RETRY: std::time::Duration = std::time::Duration::from_secs(1);

impl ShmSink {
    pub fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(ShmState { writer: None, last_attempt: None, scratch: Vec::new() }),
        }
    }

    fn open() -> tp_sink::Result<tp_sink::VideoWriter> {
        // Creating a Local\ fallback here would succeed and feed a section the
        // camera never reads, so Windows only ever attaches to the real one.
        #[cfg(windows)]
        {
            tp_sink::VideoWriter::open_existing(tp_sink::DEFAULT_VIDEO_NAME, MAX_W, MAX_H)
        }
        #[cfg(not(windows))]
        {
            tp_sink::VideoWriter::open_or_create(tp_sink::DEFAULT_VIDEO_NAME, MAX_W, MAX_H)
        }
    }
}

impl Default for ShmSink {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameSink for ShmSink {
    fn video(&self, frame: &VideoFrame<'_>) {
        // A decode thread must never wait on another one; skip the frame instead.
        let Ok(mut st) = self.state.try_lock() else { return };
        let st = &mut *st;
        if st.writer.is_none() {
            if st.last_attempt.is_some_and(|t| t.elapsed() < RETRY) {
                return;
            }
            st.last_attempt = Some(std::time::Instant::now());
            match Self::open() {
                Ok(w) => {
                    tracing::info!("virtual camera attached");
                    st.writer = Some(w);
                }
                Err(e) => {
                    tracing::debug!("virtual camera not available yet: {e}");
                    return;
                }
            }
        }
        let Some(w) = st.writer.as_mut() else { return };
        if frame.width > MAX_W || frame.height > MAX_H {
            tracing::warn!(
                "frame {}x{} exceeds the virtual camera's {MAX_W}x{MAX_H} ceiling, dropped",
                frame.width,
                frame.height
            );
            return;
        }
        if let Err(e) = w.set_format_with_stride(frame.width, frame.height, frame.stride) {
            tracing::warn!("virtual camera format rejected: {e}");
            return;
        }
        // Lets the camera advertise the source's real rate as its native type.
        if let Err(e) = w.set_frame_rate(frame.fps_num, frame.fps_den) {
            tracing::debug!("virtual camera frame rate rejected: {e}");
        }
        let stride = frame.stride as usize;
        let y_len = stride * frame.height as usize;
        let uv_len = stride * frame.height.div_ceil(2) as usize;
        let res = if frame.uv_offset == y_len && frame.data.len() >= y_len + uv_len {
            w.write_frame(&frame.data[..y_len + uv_len], frame.pts_us)
        } else if frame.data.len() >= frame.uv_offset + uv_len {
            // Decoders may pad between planes; the sink format is packed.
            st.scratch.clear();
            st.scratch.extend_from_slice(&frame.data[..y_len]);
            st.scratch.extend_from_slice(&frame.data[frame.uv_offset..frame.uv_offset + uv_len]);
            w.write_frame(&st.scratch, frame.pts_us)
        } else {
            return;
        };
        if let Err(e) = res {
            tracing::warn!("virtual camera write failed, reattaching: {e}");
            st.writer = None;
        }
    }

    fn audio(&self, _block: &AudioBlock<'_>) {}
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl FrameSink for NullSink {
    fn video(&self, _frame: &VideoFrame<'_>) {}
    fn audio(&self, _block: &AudioBlock<'_>) {}
}
