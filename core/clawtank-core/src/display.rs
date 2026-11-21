//! Legacy (non-EGFX) display source: one static test frame per connection. With EGFX the
//! live H.264 stream (see `video.rs`) replaces it as soon as the first frame arrives.
//!
//! The frame is sized to the client's requested desktop (clamped) and sent once, so an idle
//! session costs no bandwidth. Live capture replaces this in Phase 2.
//!
//! After the frame, the same stream carries cursor shapes from [`PointerHub`].
//!
//! IronRDP calls [`RdpServerDisplay::updates`] only after the client has authenticated and
//! finished capability exchange, so that call doubles as our "session started" signal.

use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat, RdpServerDisplay,
    RdpServerDisplayUpdates,
};
use tracing::info;

use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::sync::watch;

use crate::access::SessionTracker;
use crate::pointer::PointerHub;
use crate::test_pattern;
use crate::video::{StreamCodec, VideoHub};

const DEFAULT_SIZE: DesktopSize = DesktopSize {
    width: 1920,
    height: 1080,
};
// RDP limits (MS-RDPBCGR 2.2.1.3.2). We only clamp sizes no valid client sends, so the size we
// report always matches what the acceptor negotiated (see `with_honor_client_desktop_size`).
const MAX_WIDTH: u16 = 8192;
const MAX_HEIGHT: u16 = 8192;
const MIN_SIDE: u16 = 200;

/// Some RDP clients reject the legacy SurfaceBits test frame even when it is small,
/// and will disconnect before the negotiated EGFX H.264 surface can be initialized.
/// Give EGFX a short window to come online and avoid mixing the two paths.
/// Clients that do not negotiate EGFX continue to receive the legacy test pattern.
const EGFX_NEGOTIATION_GRACE: Duration = Duration::from_millis(1000);

/// Serves the static test pattern.
pub struct TestPatternDisplay {
    size: DesktopSize,
    tracker: Arc<SessionTracker>,
    video: Arc<VideoHub>,
    pointer: Arc<PointerHub>,
}

impl TestPatternDisplay {
    pub fn new(
        tracker: Arc<SessionTracker>,
        video: Arc<VideoHub>,
        pointer: Arc<PointerHub>,
    ) -> Self {
        Self {
            size: DEFAULT_SIZE,
            tracker,
            video,
            pointer,
        }
    }
}

fn clamp_size(size: DesktopSize) -> DesktopSize {
    DesktopSize {
        width: size.width.clamp(MIN_SIDE, MAX_WIDTH),
        height: size.height.clamp(MIN_SIDE, MAX_HEIGHT),
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for TestPatternDisplay {
    async fn size(&mut self) -> DesktopSize {
        self.size
    }

    async fn request_initial_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        self.size = clamp_size(client_size);
        self.video
            .set_session_size(self.size.width, self.size.height);
        info!(?client_size, chosen = ?self.size, "Client requested desktop size");
        self.size
    }

    /// Display Control (MS-RDPEDISP): the client window was resized or moved to another monitor.
    /// We follow the primary monitor; the host resizes its virtual display and restarts video.
    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let Some(monitor) = layout
            .monitors()
            .iter()
            .find(|m| m.is_primary())
            .or_else(|| layout.monitors().first())
        else {
            return;
        };
        let (width, height) = monitor.dimensions();
        let requested = DesktopSize {
            // MS-RDPEDISP 2.2.2.2.1: width must be even.
            width: u16::try_from(width & !1).unwrap_or(MAX_WIDTH),
            height: u16::try_from(height).unwrap_or(MAX_HEIGHT),
        };
        let size = clamp_size(requested);
        let scale = monitor.desktop_scale_factor().unwrap_or(100);
        info!(?size, scale, "Client requested new layout");
        self.size = size;
        self.video.request_resize(size.width, size.height, scale);
    }

    async fn updates(&mut self) -> anyhow::Result<Box<dyn RdpServerDisplayUpdates>> {
        self.tracker.session_started();
        let (tx, rx) = unbounded_channel();
        self.pointer.attach(tx);
        Ok(Box::new(TestPatternUpdates {
            size: self.size,
            sent: false,
            pointer: rx,
            video: Arc::clone(&self.video),
            legacy_frames: self.video.subscribe_legacy(),
            live_logged: false,
            initial_frame_deadline: tokio::time::Instant::now() + EGFX_NEGOTIATION_GRACE,
        }))
    }
}

struct TestPatternUpdates {
    size: DesktopSize,
    sent: bool,
    pointer: UnboundedReceiver<DisplayUpdate>,
    video: Arc<VideoHub>,
    legacy_frames: watch::Receiver<Option<Bytes>>,
    live_logged: bool,
    /// Fixed deadline makes the wait cancellation-safe when IronRDP polls this future in select!.
    initial_frame_deadline: tokio::time::Instant,
}

impl TestPatternUpdates {
    fn frame(&self) -> DisplayUpdate {
        let (w, h) = (self.size.width, self.size.height);
        let data = test_pattern::render_bgra(w, h);
        DisplayUpdate::Bitmap(BitmapUpdate {
            x: 0,
            y: 0,
            width: NonZeroU16::new(w).expect("clamped to >= MIN_SIDE"),
            height: NonZeroU16::new(h).expect("clamped to >= MIN_SIDE"),
            format: PixelFormat::BgrA32,
            data: Bytes::from(data),
            stride: NonZeroUsize::new(usize::from(w) * 4).expect("non-zero width"),
        })
    }
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for TestPatternUpdates {
    async fn next_update(&mut self) -> anyhow::Result<Option<DisplayUpdate>> {
        if !self.sent {
            // Give EGFX capabilities a chance to settle. Only AVC420 activates EGFX;
            // clients explicitly disabling AVC receive the known-working legacy test
            // pattern first, followed by live compressed BitmapUpdate frames.
            tokio::time::sleep_until(self.initial_frame_deadline).await;
            self.sent = true;
            if self.video.codec() == StreamCodec::Avc420 {
                info!("EGFX AVC ready; skipping legacy test frame");
            } else {
                info!(
                    width = self.size.width,
                    height = self.size.height,
                    "Legacy graphics mode; sending test frame"
                );
                return Ok(Some(self.frame()));
            }
        }
        // Both watch::Receiver::changed and mpsc::Receiver::recv are cancellation-safe:
        // pointer traffic cannot discard the latest live BGRA image.
        loop {
            tokio::select! {
                pointer = self.pointer.recv() => {
                    return match pointer {
                        Some(update) => Ok(Some(update)),
                        None => std::future::pending().await,
                    };
                }
                changed = self.legacy_frames.changed() => {
                    if changed.is_err() {
                        return std::future::pending().await;
                    }
                    let pixels = self.legacy_frames.borrow_and_update().clone();
                    if self.video.codec() != StreamCodec::Uncompressed {
                        continue;
                    }
                    let Some(pixels) = pixels else { continue };
                    let (w, h) = (self.size.width, self.size.height);
                    if pixels.len() != usize::from(w) * usize::from(h) * 4 {
                        continue;
                    }
                    if !self.live_logged {
                        info!(width = w, height = h, bytes = pixels.len(),
                              "Sending first live legacy desktop frame");
                        self.live_logged = true;
                    }
                    return Ok(Some(DisplayUpdate::Bitmap(BitmapUpdate {
                        x: 0,
                        y: 0,
                        width: NonZeroU16::new(w).expect("clamped width"),
                        height: NonZeroU16::new(h).expect("clamped height"),
                        format: PixelFormat::BgrA32,
                        data: pixels,
                        stride: NonZeroUsize::new(usize::from(w) * 4).expect("non-zero width"),
                    })));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn skips_legacy_frame_for_small_egfx_clients() {
        let video = Arc::new(VideoHub::new(Arc::new(|_| {})));
        let (pointer_tx, pointer_rx) = unbounded_channel();
        let mut updates = TestPatternUpdates {
            size: DesktopSize {
                width: 640,
                height: 480,
            },
            sent: false,
            pointer: pointer_rx,
            video: Arc::clone(&video),
            legacy_frames: video.subscribe_legacy(),
            live_logged: false,
            initial_frame_deadline: tokio::time::Instant::now() + Duration::from_millis(40),
        };
        // The EGFX channel becomes ready while the initial legacy frame is deferred.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            video.on_ready(crate::video::StreamCodec::Avc420);
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), updates.next_update())
                .await
                .is_err(),
            "EGFX must not receive an early legacy bitmap at any resolution"
        );
        assert!(updates.sent);
        drop(pointer_tx);
    }

    #[tokio::test]
    async fn small_legacy_client_still_receives_test_pattern() {
        let video = Arc::new(VideoHub::new(Arc::new(|_| {})));
        let (_pointer_tx, pointer_rx) = unbounded_channel();
        let mut updates = TestPatternUpdates {
            size: DesktopSize {
                width: 640,
                height: 480,
            },
            sent: false,
            pointer: pointer_rx,
            legacy_frames: video.subscribe_legacy(),
            video,
            live_logged: false,
            initial_frame_deadline: tokio::time::Instant::now(),
        };
        let update = updates.next_update().await.unwrap().unwrap();
        match update {
            DisplayUpdate::Bitmap(bitmap) => {
                assert_eq!(bitmap.width.get(), 640);
                assert_eq!(bitmap.height.get(), 480);
                assert_eq!(bitmap.data.len(), 640 * 480 * 4);
            }
            _ => panic!("expected an initial bitmap update"),
        }
    }

    #[test]
    fn sizes_are_clamped() {
        let s = clamp_size(DesktopSize {
            width: 9000,
            height: 10,
        });
        assert_eq!((s.width, s.height), (MAX_WIDTH, MIN_SIDE));
        let s = clamp_size(DesktopSize {
            width: 1512,
            height: 982,
        });
        assert_eq!((s.width, s.height), (1512, 982));
    }
}
