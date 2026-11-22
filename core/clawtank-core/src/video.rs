//! Bridge between the host's H.264 encoder and the EGFX graphics pipeline (MS-RDPEGFX AVC420).
//!
//! The host (VideoToolbox) submits Annex-B access units with [`VideoHub::submit_h264`]. We
//! create the EGFX surface lazily on the first keyframe, queue the frame on IronRDP's
//! `GraphicsPipelineServer`, drain its output and post it to the session loop.
//!
//! Two rules keep the decoder healthy:
//! - A frame that can't be sent (backpressure) is dropped, and every following frame is
//!   refused until the host sends a keyframe. A dropped P-frame would break the reference
//!   chain.
//! - Lock order is always `inner` then the pipeline mutex. IronRDP calls our
//!   `GraphicsPipelineHandler` while holding the pipeline mutex, so handler callbacks only
//!   touch atomics (never `inner`).
//!
//! Clients that negotiate EGFX without AVC (`AVC_DISABLED`: the Windows App on Android and
//! iOS) can't decode H.264. For them [`VideoHub::codec`] reports [`VideoCodec::Raw`] and the
//! host submits raw BGRA frames with [`VideoHub::submit_bgra`] instead: we diff them against
//! the last frame the client received and send the changed tiles losslessly with ClearCodec
//! (MS-RDPEGFX 2.2.4.1), which every EGFX client must support.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use ironrdp_egfx::pdu::Avc420Region;
use ironrdp_egfx::server::{GraphicsPipelineServer, MixedTilePayload};
use ironrdp_graphics::clearcodec::ClearCodecEncoder;
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_pdu::geometry::ExclusiveRectangle;
use ironrdp_server::{EgfxServerMessage, GfxServerHandle, ServerEvent};
use ironrdp_svc::ChannelFlags;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

use crate::events::{CoreEvent, EventSink};

/// Outcome of [`VideoHub::submit_h264`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitResult {
    /// Queued to the client.
    Sent,
    /// No EGFX session yet (or it closed); nothing was sent and nothing is owed.
    NotReady,
    /// Not sent; the host must send a keyframe next.
    NeedKeyframe,
}

/// How the host must feed video for the current session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    /// H.264 access units via [`VideoHub::submit_h264`] (EGFX AVC420).
    H264,
    /// H.264 AVC444 dual-stream transport.
    Avc444,
    /// Raw BGRA frames via [`VideoHub::submit_bgra`] (EGFX ClearCodec tiles); the client
    /// negotiated EGFX with AVC disabled.
    Raw,
}

/// Video preference. Never override a client AVC_DISABLED advertisement.
/// HEVC requires an Azure protocol extension that is not implemented here.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoPreference {
    Auto = 0,
    ClearCodec = 1,
    Avc420 = 2,
    Avc444 = 3,
}

impl VideoPreference {
    /// Converts a C ABI value into a supported policy.
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Auto),
            1 => Some(Self::ClearCodec),
            2 => Some(Self::Avc420),
            3 => Some(Self::Avc444),
            _ => None,
        }
    }
}

/// Decide wire codec from the confirmed capabilities, never from a user
/// preference alone. The reason is included in the once-per-session INFO log.
fn selected_codec(
    preference: VideoPreference,
    client_avc420: bool,
    client_avc444: bool,
) -> (VideoCodec, &'static str) {
    match preference {
        VideoPreference::ClearCodec => (VideoCodec::Raw, "requested-clearcodec"),
        VideoPreference::Auto if client_avc420 => (VideoCodec::H264, "auto-client-avc420"),
        VideoPreference::Auto => (VideoCodec::Raw, "auto-client-avc-unavailable"),
        VideoPreference::Avc420 if client_avc420 => (VideoCodec::H264, "requested-avc420"),
        VideoPreference::Avc420 => (VideoCodec::Raw, "fallback-client-avc-unavailable"),
        VideoPreference::Avc444 if client_avc444 => (VideoCodec::Avc444, "requested-avc444"),
        VideoPreference::Avc444 if client_avc420 => {
            (VideoCodec::H264, "fallback-avc444-to-avc420")
        }
        VideoPreference::Avc444 => (VideoCodec::Raw, "fallback-client-avc-unavailable"),
    }
}

/// Small tiles minimize recompression around text and cursors; larger tiles reduce
/// ClearCodec headers, allocations and DVC fragmentation for broad screen changes.
/// The codec uses a glyph cache only for regions at most 1024 pixels, so the final
/// short strip must be merged with its previous tile.
const TILE_SMALL: usize = 64;
const TILE_DENSE: usize = 128;
const MIN_TILE: usize = 33;

/// Splits `0..len` into spans with no narrow trailing remainder.
fn tile_spans(len: usize, edge: usize) -> Vec<(usize, usize)> {
    let mut spans = Vec::with_capacity(len / edge + 1);
    let mut start = 0;
    while start < len {
        let remaining = len - start;
        let size = if remaining < edge + MIN_TILE {
            remaining
        } else {
            edge
        };
        spans.push((start, size));
        start += size;
    }
    spans
}

/// Sample current and last queued pixels to choose a tiling strategy. A wrong
/// *prediction* only selects a less efficient tile size: every tile is still
/// compared byte-for-byte, so this never loses a changed pixel. This remains
/// cheap even at Retina resolutions (64 sampled pixels).
fn use_dense_tiles(data: &[u8], shown: &[u8], w: usize, h: usize, stride: usize) -> bool {
    const GRID: usize = 8;
    const THRESHOLD: usize = 16;
    let mut changed = 0;
    for gy in 0..GRID {
        let y = (2 * gy + 1) * h / (2 * GRID);
        for gx in 0..GRID {
            let x = (2 * gx + 1) * w / (2 * GRID);
            let src = y * stride + x * 4;
            let old = (y * w + x) * 4;
            if data[src..src + 3] != shown[old..old + 3] {
                changed += 1;
                if changed >= THRESHOLD {
                    return true;
                }
            }
        }
    }
    false
}

/// Counters for the stats readout. All monotonic within a session.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VideoStats {
    pub frames_sent: u64,
    pub bytes_sent: u64,
    pub frames_dropped: u64,
    pub frames_acked: u64,
    pub frames_in_flight: u32,
    /// Smoothed time from sending a frame to the client acknowledging it (decode included),
    /// in microseconds; 0 until the first ack. The main congestion signal for adaptive bitrate.
    pub ack_latency_us: u32,
}

/// Shared between the EGFX factory/handler (IronRDP side) and the FFI (host side).
pub struct VideoHub {
    sink: EventSink,
    inner: Mutex<Inner>,
    // Touched from handler callbacks (pipeline mutex held): atomics only.
    channel_open: AtomicBool,
    session_size: AtomicU32,
    frames_acked: AtomicU64,
    /// Send time per in-flight frame id. Its own lock: taken from the handler (pipeline mutex
    /// held) and from `submit_h264` *after* the pipeline mutex is released, never nested.
    send_times: Mutex<HashMap<u32, Instant>>,
    ack_latency_us: AtomicU32,
    /// Increments each time a new EGFX pipeline is attached, for grouped logs.
    session_id: AtomicU64,
    /// True only when this session is both AVC-capable and policy-selected for H.264.
    avc: AtomicBool,
    /// Current negotiated graphics path uses dual-stream AVC444.
    avc444: AtomicBool,
    /// Persistent preference for future sessions; updates never switch an active codec.
    preference: AtomicU8,
}

struct Inner {
    handle: Option<GfxServerHandle>,
    sender: Option<UnboundedSender<ServerEvent>>,
    surface: Option<u16>,
    need_keyframe: bool,
    started: Instant,
    frames_sent: u64,
    bytes_sent: u64,
    frames_dropped: u64,
    /// Raw path: what the client's surface currently shows (tightly packed BGRA at the session
    /// size), or empty if the next frame must be sent whole.
    shown: Vec<u8>,
    clear: ClearCodecEncoder,
    /// Next ClearCodec `seqNumber`; advanced only for streams actually sent.
    clear_seq: u8,
    /// Rolling diagnostics: capture-to-EGFX encode + queue time, not wire ACK time.
    raw_profile_frames: u64,
    raw_profile_us: u64,
    raw_profile_tiles: u64,
    raw_profile_bytes: u64,
}

fn pack(width: u16, height: u16) -> u32 {
    (u32::from(width) << 16) | u32::from(height)
}

fn unpack(v: u32) -> (u16, u16) {
    ((v >> 16) as u16, (v & 0xffff) as u16)
}

impl VideoHub {
    pub fn new(sink: EventSink) -> Self {
        Self {
            sink,
            inner: Mutex::new(Inner {
                handle: None,
                sender: None,
                surface: None,
                need_keyframe: true,
                started: Instant::now(),
                frames_sent: 0,
                bytes_sent: 0,
                frames_dropped: 0,
                shown: Vec::new(),
                clear: ClearCodecEncoder::new(),
                clear_seq: 0,
                raw_profile_frames: 0,
                raw_profile_us: 0,
                raw_profile_tiles: 0,
                raw_profile_bytes: 0,
            }),
            channel_open: AtomicBool::new(false),
            session_size: AtomicU32::new(pack(1920, 1080)),
            frames_acked: AtomicU64::new(0),
            send_times: Mutex::new(HashMap::new()),
            ack_latency_us: AtomicU32::new(0),
            session_id: AtomicU64::new(0),
            avc: AtomicBool::new(true),
            avc444: AtomicBool::new(false),
            preference: AtomicU8::new(VideoPreference::Auto as u8),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The session loop's event sender (set once when the server is built).
    pub fn set_sender(&self, sender: UnboundedSender<ServerEvent>) {
        self.lock().sender = Some(sender);
    }

    /// Identifier shared by this graphics session's negotiation and frame logs.
    pub(crate) fn session_id(&self) -> u64 {
        self.session_id.load(Ordering::Acquire)
    }

    /// Desktop size negotiated for the current session; the EGFX surface uses it.
    pub fn set_session_size(&self, width: u16, height: u16) {
        self.session_size
            .store(pack(width, height), Ordering::Release);
    }

    pub fn session_size(&self) -> (u16, u16) {
        unpack(self.session_size.load(Ordering::Acquire))
    }

    /// A new connection built its pipeline: forget the previous session's state.
    pub fn attach(&self, handle: GfxServerHandle) {
        let mut inner = self.lock();
        self.session_id.fetch_add(1, Ordering::AcqRel);
        inner.handle = Some(handle);
        inner.surface = None;
        inner.need_keyframe = true;
        inner.started = Instant::now();
        inner.frames_sent = 0;
        inner.bytes_sent = 0;
        inner.frames_dropped = 0;
        inner.shown.clear();
        inner.clear = ClearCodecEncoder::new();
        inner.clear_seq = 0;
        inner.raw_profile_frames = 0;
        inner.raw_profile_us = 0;
        inner.raw_profile_tiles = 0;
        inner.raw_profile_bytes = 0;
        self.avc.store(true, Ordering::Release);
        self.avc444.store(false, Ordering::Release);
        self.frames_acked.store(0, Ordering::Release);
        self.ack_latency_us.store(0, Ordering::Release);
        self.send_times
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// The client asked for a new desktop size (Display Control). The next keyframe at that size
    /// recreates the surface (ResetGraphics); frames still arriving at the old size are refused.
    pub fn request_resize(&self, width: u16, height: u16, scale_percent: u32) {
        if self.session_size() == (width, height) {
            return;
        }
        self.set_session_size(width, height);
        {
            let mut inner = self.lock();
            inner.surface = None;
            inner.need_keyframe = true;
            inner.shown.clear();
        }
        if self.channel_open.load(Ordering::Acquire) {
            info!(width, height, scale_percent, "Resizing video");
            (self.sink)(CoreEvent::VideoResize {
                width,
                height,
                scale_percent,
            });
        }
    }

    /// Select a policy for the next client connection (or reconnect).
    /// ClearCodec is mandatory for EGFX, and AVC420 is used only if advertised.
    pub fn set_preference(&self, preference: VideoPreference) {
        self.preference.store(preference as u8, Ordering::Release);
    }

    pub fn preference(&self) -> VideoPreference {
        match self.preference.load(Ordering::Acquire) {
            1 => VideoPreference::ClearCodec,
            2 => VideoPreference::Avc420,
            3 => VideoPreference::Avc444,
            _ => VideoPreference::Auto,
        }
    }

    /// Select an actual wire codec only from confirmed client capabilities.
    pub(crate) fn on_ready(&self, client_avc420: bool, client_avc444: bool) {
        let (w, h) = self.session_size();
        let preference = self.preference();
        let (codec, reason) = selected_codec(preference, client_avc420, client_avc444);
        self.avc.store(codec != VideoCodec::Raw, Ordering::Release);
        self.avc444.store(codec == VideoCodec::Avc444, Ordering::Release);
        if !self.channel_open.swap(true, Ordering::AcqRel) {
            info!(
                gfx_session = self.session_id(),
                width = w,
                height = h,
                requested = ?preference,
                selected = ?codec,
                reason,
                client_avc420,
                client_avc444,
                "RDPGFX_CODEC_SELECTED"
            );
            (self.sink)(CoreEvent::VideoStart {
                width: w,
                height: h,
            });
        }
    }

    /// Handler callback: the client acknowledged a frame.
    pub(crate) fn on_frame_ack(&self, frame_id: u32) {
        self.frames_acked.fetch_add(1, Ordering::AcqRel);
        let sent_at = self
            .send_times
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&frame_id);
        if let Some(sent_at) = sent_at {
            let sample = u32::try_from(sent_at.elapsed().as_micros()).unwrap_or(u32::MAX);
            // EWMA with alpha 1/8: smooth enough to ignore single hiccups, quick enough to react.
            let old = self.ack_latency_us.load(Ordering::Acquire);
            let new = if old == 0 {
                sample
            } else {
                old - old / 8 + sample / 8
            };
            self.ack_latency_us.store(new, Ordering::Release);
        }
    }

    /// Handler callback, or session end: the graphics channel is gone.
    pub(crate) fn on_close(&self) {
        if self.channel_open.swap(false, Ordering::AcqRel) {
            info!(gfx_session = self.session_id(), selected = ?self.codec(), "RDPGFX_CHANNEL_CLOSED");
            (self.sink)(CoreEvent::VideoStop);
        }
    }

    /// Called by the accept loop when a connection ends.
    pub fn session_ended(&self) {
        self.on_close();
        let mut inner = self.lock();
        inner.handle = None;
        inner.surface = None;
        inner.need_keyframe = true;
        inner.shown.clear();
    }

    /// How the host must submit frames for the current session.
    pub fn codec(&self) -> VideoCodec {
        if self.avc444.load(Ordering::Acquire) {
            VideoCodec::Avc444
        } else if self.avc.load(Ordering::Acquire) {
            VideoCodec::H264
        } else {
            VideoCodec::Raw
        }
    }

    /// Cheap check the host can make before spending time encoding.
    pub fn can_accept(&self) -> bool {
        if !self.channel_open.load(Ordering::Acquire) {
            return false;
        }
        let handle = self.lock().handle.clone();
        let Some(handle) = handle else { return false };
        // Never block the capture thread on the pipeline; if it's busy, say "not now".
        match handle.try_lock() {
            Ok(gfx) => gfx.is_ready() && !gfx.should_backpressure(),
            Err(_) => false,
        }
    }

    pub fn stats(&self) -> VideoStats {
        let inner = self.lock();
        let acked = self.frames_acked.load(Ordering::Acquire);
        VideoStats {
            frames_sent: inner.frames_sent,
            bytes_sent: inner.bytes_sent,
            frames_dropped: inner.frames_dropped,
            frames_acked: acked,
            frames_in_flight: u32::try_from(inner.frames_sent.saturating_sub(acked))
                .unwrap_or(u32::MAX),
            ack_latency_us: self.ack_latency_us.load(Ordering::Acquire),
        }
    }

    /// Queues one H.264 access unit (Annex-B, with SPS/PPS on keyframes) encoded for a
    /// `width`×`height` desktop. Frames for any other size (in flight across a resize) are
    /// refused with `NotReady`, since they would not fit the client's surface.
    pub fn submit_h264(
        &self,
        data: &[u8],
        keyframe: bool,
        width: u16,
        height: u16,
    ) -> SubmitResult {
        if data.is_empty() || !self.channel_open.load(Ordering::Acquire) {
            return SubmitResult::NotReady;
        }
        if !self.avc.load(Ordering::Acquire) || self.avc444.load(Ordering::Acquire) {
            // Never send AVC420 into an AVC444 or ClearCodec session.
            return SubmitResult::NotReady;
        }
        if (width, height) != self.session_size() {
            debug!(width, height, session = ?self.session_size(), "Dropping frame for a stale size");
            return SubmitResult::NotReady;
        }
        let mut inner = self.lock();
        let (Some(handle), Some(sender)) = (inner.handle.clone(), inner.sender.clone()) else {
            return SubmitResult::NotReady;
        };
        if inner.need_keyframe && !keyframe {
            inner.frames_dropped += 1;
            return SubmitResult::NeedKeyframe;
        }

        let (w, h) = self.session_size();
        let timestamp_ms = u32::try_from(inner.started.elapsed().as_millis()).unwrap_or(u32::MAX);
        let (frame_id, channel_id, messages) = {
            let mut gfx = handle.lock().unwrap_or_else(|e| e.into_inner());
            if !gfx.is_ready() {
                return SubmitResult::NotReady;
            }
            let Some(surface) = Self::ensure_surface(&mut inner, &mut gfx, w, h) else {
                return SubmitResult::NotReady;
            };
            let region = Avc420Region {
                left: 0,
                top: 0,
                right: w.saturating_sub(1),
                bottom: h.saturating_sub(1),
                quantization_parameter: 22,
                quality: 100,
            };
            let frame_id = gfx.send_avc420_frame(surface, data, &[region], timestamp_ms);
            (frame_id, gfx.channel_id(), gfx.drain_output())
        };
        let sent = frame_id.is_some();
        if let Some(frame_id) = frame_id {
            self.record_send(frame_id);
        }

        Self::post(&sender, channel_id, messages);

        if sent {
            if inner.frames_sent == 0 {
                info!(gfx_session = self.session_id(), codec = "AVC420", "RDPGFX_FIRST_FRAME_SENT");
            }
            inner.need_keyframe = false;
            inner.frames_sent += 1;
            inner.bytes_sent += data.len() as u64;
            SubmitResult::Sent
        } else {
            inner.need_keyframe = true;
            inner.frames_dropped += 1;
            SubmitResult::NeedKeyframe
        }
    }

    /// Sends two matching H.264 Annex-B access units as one AVC444 frame.
    /// Rejects unsupported codecs and stale capture dimensions.
    pub fn submit_avc444(
        &self,
        luma: &[u8],
        chroma: &[u8],
        keyframe: bool,
        width: u16,
        height: u16,
    ) -> SubmitResult {
        if luma.is_empty() || chroma.is_empty() || !self.channel_open.load(Ordering::Acquire) {
            return SubmitResult::NotReady;
        }
        if !self.avc444.load(Ordering::Acquire) || (width, height) != self.session_size() {
            return SubmitResult::NotReady;
        }
        let mut inner = self.lock();
        let (Some(handle), Some(sender)) = (inner.handle.clone(), inner.sender.clone()) else {
            return SubmitResult::NotReady;
        };
        if inner.need_keyframe && !keyframe {
            inner.frames_dropped += 1;
            return SubmitResult::NeedKeyframe;
        }
        let timestamp_ms = u32::try_from(inner.started.elapsed().as_millis()).unwrap_or(u32::MAX);
        let (frame_id, channel_id, messages) = {
            let mut gfx = handle.lock().unwrap_or_else(|e| e.into_inner());
            if !gfx.is_ready() {
                return SubmitResult::NotReady;
            }
            let Some(surface) = Self::ensure_surface(&mut inner, &mut gfx, width, height) else {
                return SubmitResult::NotReady;
            };
            let region = Avc420Region {
                left: 0,
                top: 0,
                right: width.saturating_sub(1),
                bottom: height.saturating_sub(1),
                quantization_parameter: 22,
                quality: 100,
            };
            let id = gfx.send_avc444_frame(
                surface,
                luma,
                std::slice::from_ref(&region),
                Some(chroma),
                Some(std::slice::from_ref(&region)),
                timestamp_ms,
            );
            (id, gfx.channel_id(), gfx.drain_output())
        };
        if let Some(id) = frame_id {
            self.record_send(id);
        }
        Self::post(&sender, channel_id, messages);
        if frame_id.is_some() {
            if inner.frames_sent == 0 {
                info!(gfx_session = self.session_id(), codec = "AVC444", "RDPGFX_FIRST_FRAME_SENT");
            }
            inner.need_keyframe = false;
            inner.frames_sent += 1;
            inner.bytes_sent += (luma.len() + chroma.len()) as u64;
            SubmitResult::Sent
        } else {
            inner.need_keyframe = true;
            inner.frames_dropped += 1;
            SubmitResult::NeedKeyframe
        }
    }

    /// The EGFX surface for the session size, creating it (ResetGraphics with one primary
    /// monitor, then a surface mapped at (0,0)) if there is none yet.
    fn ensure_surface(
        inner: &mut Inner,
        gfx: &mut GraphicsPipelineServer,
        w: u16,
        h: u16,
    ) -> Option<u16> {
        if let Some(id) = inner.surface {
            return Some(id);
        }
        let monitor = Monitor {
            left: 0,
            top: 0,
            right: i32::from(w) - 1,
            bottom: i32::from(h) - 1,
            flags: MonitorFlags::PRIMARY,
        };
        gfx.resize_with_monitors(w, h, vec![monitor]);
        let Some(id) = gfx.create_surface(w, h) else {
            warn!("EGFX surface creation failed");
            return None;
        };
        gfx.map_surface_to_output(id, 0, 0);
        debug!(surface = id, w, h, "EGFX surface ready");
        inner.surface = Some(id);
        // A new surface starts blank: the raw path must repaint everything.
        inner.shown.clear();
        Some(id)
    }

    fn record_send(&self, frame_id: u32) {
        let mut times = self.send_times.lock().unwrap_or_else(|e| e.into_inner());
        // Bounded: a client that stops acking must not grow this forever.
        if times.len() > 256 {
            times.clear();
        }
        times.insert(frame_id, Instant::now());
    }

    /// Posts drained EGFX output to the session loop.
    fn post(
        sender: &UnboundedSender<ServerEvent>,
        channel_id: Option<u32>,
        messages: Vec<ironrdp_dvc::DvcMessage>,
    ) {
        if !messages.is_empty()
            && let Some(channel_id) = channel_id
        {
            match ironrdp_dvc::encode_dvc_messages(
                channel_id,
                messages,
                ChannelFlags::SHOW_PROTOCOL,
            ) {
                Ok(messages) => {
                    let _ = sender.send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                        messages,
                    }));
                }
                Err(e) => warn!(error = %e, "Failed to encode EGFX messages"),
            }
        }
    }

    /// Queues one raw frame for a client without AVC (see [`VideoCodec::Raw`]): `data` is
    /// 32-bit BGRA (alpha ignored), rows top-down, `stride` bytes apart; only the top-left
    /// `width`×`height` pixels are read. Frames for any size other than the session size are
    /// refused with `NotReady`, as is everything while the session uses H.264.
    ///
    /// Only tiles that differ from what the client already shows are encoded. A frame that
    /// can't be sent (backpressure) is simply dropped: the next one is diffed against what the
    /// client really has, so nothing is lost. `Sent` is also returned when nothing changed.
    pub fn submit_bgra(&self, data: &[u8], width: u16, height: u16, stride: usize) -> SubmitResult {
        let started = Instant::now();
        if !self.channel_open.load(Ordering::Acquire) || self.avc.load(Ordering::Acquire) {
            return SubmitResult::NotReady;
        }
        if (width, height) != self.session_size() {
            debug!(width, height, session = ?self.session_size(), "Dropping frame for a stale size");
            return SubmitResult::NotReady;
        }
        let (w, h) = (usize::from(width), usize::from(height));
        let row = w * 4;
        if w == 0 || h == 0 || stride < row || data.len() < stride * (h - 1) + row {
            warn!(
                width,
                height,
                stride,
                len = data.len(),
                "Raw frame has a bad layout"
            );
            return SubmitResult::NotReady;
        }

        let mut inner = self.lock();
        let (Some(handle), Some(sender)) = (inner.handle.clone(), inner.sender.clone()) else {
            return SubmitResult::NotReady;
        };
        // Avoid spending milliseconds encoding a large frame if EGFX is already
        // blocked on client ACKs. Check again before posting in case state changes.
        {
            let gfx = handle.lock().unwrap_or_else(|e| e.into_inner());
            if !gfx.is_ready() || gfx.should_backpressure() {
                return SubmitResult::NotReady;
            }
        }
        if inner.surface.is_none() {
            inner.shown.clear();
        }
        let full = inner.shown.len() != row * h;
        let tile_size = if !full && use_dense_tiles(data, &inner.shown, w, h, stride) {
            TILE_DENSE
        } else {
            TILE_SMALL
        };

        // Encode changed tiles before taking the pipeline lock (encoding is the slow part).
        let mut tiles = Vec::new();
        let mut changed_rects = Vec::new();
        let mut bytes = 0usize;
        let mut tile_buf =
            Vec::with_capacity((TILE_DENSE + MIN_TILE) * (TILE_DENSE + MIN_TILE) * 4);
        for &(ty, th) in &tile_spans(h, tile_size) {
            for &(tx, tw) in &tile_spans(w, tile_size) {
                let changed = full
                    || (ty..ty + th).any(|y| {
                        let src = &data[y * stride + tx * 4..y * stride + (tx + tw) * 4];
                        src != &inner.shown[y * row + tx * 4..y * row + (tx + tw) * 4]
                    });
                if !changed {
                    continue;
                }
                tile_buf.clear();
                for y in ty..ty + th {
                    tile_buf
                        .extend_from_slice(&data[y * stride + tx * 4..y * stride + (tx + tw) * 4]);
                }
                // Tile sizes are bounded by the u16 session size.
                let bitmap_data = inner.clear.encode(&tile_buf, tw as u16, th as u16);
                bytes += bitmap_data.len();
                changed_rects.push((tx, ty, tw, th));
                tiles.push(MixedTilePayload::ClearCodec {
                    destination: ExclusiveRectangle {
                        left: tx as u16,
                        top: ty as u16,
                        right: (tx + tw) as u16,
                        bottom: (ty + th) as u16,
                    },
                    bitmap_data,
                });
            }
        }
        if tiles.is_empty() {
            return SubmitResult::Sent;
        }
        // ClearCodec streams carry a sequence number the client checks for continuity. The
        // encoder bumps its own counter even for frames we end up dropping, so we number the
        // streams ourselves and only commit the counter once the frame is queued.
        let mut seq = inner.clear_seq;
        for tile in &mut tiles {
            if let MixedTilePayload::ClearCodec { bitmap_data, .. } = tile {
                bitmap_data[1] = seq;
                seq = seq.wrapping_add(1);
            }
        }

        let timestamp_ms = u32::try_from(inner.started.elapsed().as_millis()).unwrap_or(u32::MAX);
        let (frame_id, channel_id, messages) = {
            let mut gfx = handle.lock().unwrap_or_else(|e| e.into_inner());
            if !gfx.is_ready() || gfx.should_backpressure() {
                return SubmitResult::NotReady;
            }
            // No surface yet implies `full` (`shown` was cleared above under the same lock).
            let Some(surface) = Self::ensure_surface(&mut inner, &mut gfx, width, height) else {
                return SubmitResult::NotReady;
            };
            let frame_id = gfx.send_mixed_frame(surface, tiles, timestamp_ms);
            (frame_id, gfx.channel_id(), gfx.drain_output())
        };
        if let Some(frame_id) = frame_id {
            self.record_send(frame_id);
        }
        Self::post(&sender, channel_id, messages);

        if frame_id.is_some() {
            if inner.frames_sent == 0 {
                info!(gfx_session = self.session_id(), codec = "ClearCodec", "RDPGFX_FIRST_FRAME_SENT");
            }
            inner.clear_seq = seq;
            if full {
                inner.shown.resize(row * h, 0);
            }
            // When a small window changes, copying the entire Retina screen
            // (~20 MiB at 2776x1938) on every frame is unnecessary. Cache only
            // the tiles actually queued, keeping the diff baseline accurate.
            for &(tx, ty, tw, th) in &changed_rects {
                let byte_x = tx * 4;
                let byte_w = tw * 4;
                for y in ty..ty + th {
                    let src = y * stride + byte_x;
                    let dst = y * row + byte_x;
                    inner.shown[dst..dst + byte_w].copy_from_slice(&data[src..src + byte_w]);
                }
            }
            inner.frames_sent += 1;
            inner.bytes_sent += bytes as u64;
            inner.raw_profile_frames += 1;
            inner.raw_profile_us = inner
                .raw_profile_us
                .saturating_add(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
            inner.raw_profile_tiles += changed_rects.len() as u64;
            inner.raw_profile_bytes += bytes as u64;
            if inner.raw_profile_frames >= 30 {
                let frames = inner.raw_profile_frames;
                info!(
                    avg_encode_ms = inner.raw_profile_us / frames / 1000,
                    avg_tiles = inner.raw_profile_tiles / frames,
                    avg_kib = inner.raw_profile_bytes / frames / 1024,
                    ack_ms = self.ack_latency_us.load(Ordering::Acquire) / 1000,
                    in_flight = inner
                        .frames_sent
                        .saturating_sub(self.frames_acked.load(Ordering::Acquire)),
                    "ClearCodec mobile performance"
                );
                inner.raw_profile_frames = 0;
                inner.raw_profile_us = 0;
                inner.raw_profile_tiles = 0;
                inner.raw_profile_bytes = 0;
            }
            SubmitResult::Sent
        } else {
            inner.frames_dropped += 1;
            SubmitResult::NotReady
        }
    }
}

#[cfg(test)]
mod video_preference_tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn only_implemented_graphics_modes_are_accepted() {
        assert_eq!(VideoPreference::from_u32(0), Some(VideoPreference::Auto));
        assert_eq!(
            VideoPreference::from_u32(1),
            Some(VideoPreference::ClearCodec)
        );
        assert_eq!(VideoPreference::from_u32(2), Some(VideoPreference::Avc420));
        assert_eq!(VideoPreference::from_u32(3), Some(VideoPreference::Avc444));
        // Azure HEVC is not part of this pinned EGFX protocol version.
        assert_eq!(VideoPreference::from_u32(4), None);
        assert_eq!(VideoPreference::from_u32(u32::MAX), None);
    }

    #[test]
    fn codec_selection_logs_explicit_fallback_reasons() {
        assert_eq!(
            selected_codec(VideoPreference::Avc444, true, false),
            (VideoCodec::H264, "fallback-avc444-to-avc420")
        );
        assert_eq!(
            selected_codec(VideoPreference::Avc444, false, false),
            (VideoCodec::Raw, "fallback-client-avc-unavailable")
        );
        assert_eq!(
            selected_codec(VideoPreference::Avc444, true, true),
            (VideoCodec::Avc444, "requested-avc444")
        );
        assert_eq!(
            selected_codec(VideoPreference::Auto, false, false),
            (VideoCodec::Raw, "auto-client-avc-unavailable")
        );
        assert_eq!(
            selected_codec(VideoPreference::ClearCodec, true, true),
            (VideoCodec::Raw, "requested-clearcodec")
        );
    }

    #[test]
    fn policy_does_not_override_mobile_avc_disabled() {
        let hub = VideoHub::new(Arc::new(|_| {}));
        hub.set_preference(VideoPreference::Avc420);
        hub.on_ready(false, false);
        assert_eq!(hub.codec(), VideoCodec::Raw);
        hub.on_close();

        hub.on_ready(true, true);
        assert_eq!(hub.codec(), VideoCodec::H264);
        hub.on_close();

        hub.set_preference(VideoPreference::Avc444);
        hub.on_ready(true, true);
        assert_eq!(hub.codec(), VideoCodec::Avc444);
        hub.on_close();

        hub.set_preference(VideoPreference::Avc444);
        hub.on_ready(false, false);
        assert_eq!(hub.codec(), VideoCodec::Raw);
        hub.on_close();

        hub.set_preference(VideoPreference::ClearCodec);
        hub.on_ready(true, true);
        assert_eq!(hub.codec(), VideoCodec::Raw);
        hub.on_close();

        hub.set_preference(VideoPreference::Auto);
        hub.on_ready(true, true);
        assert_eq!(hub.codec(), VideoCodec::H264);
        hub.on_close();
    }
}
