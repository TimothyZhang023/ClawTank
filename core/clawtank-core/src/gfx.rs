//! IronRDP EGFX glue: the factory that builds a graphics pipeline per connection and the
//! handler that reports pipeline state to the [`VideoHub`].
//!
//! The handler runs with IronRDP's pipeline mutex held, so it only calls `VideoHub`'s
//! atomic-only callbacks (see the lock-order note in `video.rs`).

use std::sync::{Arc, Mutex};

use ironrdp_egfx::pdu::{
    CapabilitiesAdvertisePdu, CapabilitiesV8Flags, CapabilitiesV10Flags, CapabilitiesV81Flags,
    CapabilitiesV103Flags, CapabilitiesV104Flags, CapabilitiesV107Flags, CapabilitySet,
};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
use ironrdp_server::{
    GfxDvcBridge, GfxServerFactory, GfxServerHandle, ServerEvent, ServerEventSender,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::info;

use crate::video::{StreamCodec, VideoHub};

pub struct GfxFactory {
    hub: Arc<VideoHub>,
}

impl GfxFactory {
    pub fn new(hub: Arc<VideoHub>) -> Self {
        Self { hub }
    }
}

impl ServerEventSender for GfxFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        self.hub.set_sender(sender);
    }
}

impl GfxServerFactory for GfxFactory {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(GfxHandler {
            hub: Arc::clone(&self.hub),
        })
    }

    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        let server: GfxServerHandle = Arc::new(Mutex::new(GraphicsPipelineServer::new(
            self.build_gfx_handler(),
        )));
        self.hub.attach(Arc::clone(&server));
        Some((GfxDvcBridge::new(Arc::clone(&server)), server))
    }
}

struct GfxHandler {
    hub: Arc<VideoHub>,
}

impl GraphicsPipelineHandler for GfxHandler {
    fn capabilities_advertise(&mut self, pdu: &CapabilitiesAdvertisePdu) {
        // The iOS Windows App advertises AVC_DISABLED. Log the full raw capabilities so
        // a client-side protocol error is diagnosable without a packet capture.
        info!(count = pdu.0.len(), ?pdu, "Client EGFX capabilities");
    }

    fn preferred_capabilities(&self) -> Vec<CapabilitySet> {
        // IronRDP 0.3.0 intersects client/server flags. AVC_DISABLED is a NEGATIVE
        // capability; leaving it out of our advertised mask silently clears the client's
        // explicit no-AVC requirement and makes iOS Windows App reject CapsConfirm.
        // Include the bit here, so the intersection preserves it only when the client
        // actually requests it. Normal H.264 clients still negotiate AVC420.
        vec![
            CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::SMALL_CACHE | CapabilitiesV107Flags::AVC_DISABLED,
            },
            CapabilitySet::V10_6Err {
                flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED,
            },
            CapabilitySet::V10_6 {
                flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED,
            },
            CapabilitySet::V10_5 {
                flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED,
            },
            CapabilitySet::V10_4 {
                flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED,
            },
            CapabilitySet::V10_3 {
                flags: CapabilitiesV103Flags::AVC_DISABLED,
            },
            CapabilitySet::V10_2 {
                flags: CapabilitiesV10Flags::SMALL_CACHE | CapabilitiesV10Flags::AVC_DISABLED,
            },
            CapabilitySet::V10_1,
            CapabilitySet::V10 {
                flags: CapabilitiesV10Flags::SMALL_CACHE | CapabilitiesV10Flags::AVC_DISABLED,
            },
            CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::AVC420_ENABLED | CapabilitiesV81Flags::SMALL_CACHE,
            },
            CapabilitySet::V8 {
                flags: CapabilitiesV8Flags::SMALL_CACHE,
            },
        ]
    }

    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        let codec = match negotiated {
            CapabilitySet::V8 { .. } => StreamCodec::Uncompressed,
            CapabilitySet::V8_1 { flags }
                if !flags.contains(CapabilitiesV81Flags::AVC420_ENABLED) =>
            {
                StreamCodec::Uncompressed
            }
            CapabilitySet::V10 { flags } | CapabilitySet::V10_2 { flags }
                if flags.contains(CapabilitiesV10Flags::AVC_DISABLED) =>
            {
                StreamCodec::Uncompressed
            }
            CapabilitySet::V10_3 { flags }
                if flags.contains(CapabilitiesV103Flags::AVC_DISABLED) =>
            {
                StreamCodec::Uncompressed
            }
            CapabilitySet::V10_4 { flags }
            | CapabilitySet::V10_5 { flags }
            | CapabilitySet::V10_6 { flags }
            | CapabilitySet::V10_6Err { flags }
                if flags.contains(CapabilitiesV104Flags::AVC_DISABLED) =>
            {
                StreamCodec::Uncompressed
            }
            CapabilitySet::V10_7 { flags }
                if flags.contains(CapabilitiesV107Flags::AVC_DISABLED) =>
            {
                StreamCodec::Uncompressed
            }
            _ => StreamCodec::Avc420,
        };
        info!(?negotiated, ?codec, "EGFX capabilities negotiated");
        self.hub.on_ready(codec);
    }

    fn on_frame_ack(&mut self, frame_id: u32, _queue_depth: u32, _total_frames_decoded: u32) {
        self.hub.on_frame_ack(frame_id);
    }

    fn on_close(&mut self) {
        self.hub.on_close();
    }
}
