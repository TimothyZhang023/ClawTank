//! IronRDP EGFX glue: the factory that builds a graphics pipeline per connection and the
//! handler that reports pipeline state to the [`VideoHub`].
//!
//! The handler runs with IronRDP's pipeline mutex held, so it only calls `VideoHub`'s
//! atomic-only callbacks (see the lock-order note in `video.rs`).

use std::sync::{Arc, Mutex};

use ironrdp_egfx::pdu::{
    CapabilitiesAdvertisePdu, CapabilitiesV8Flags, CapabilitiesV10Flags, CapabilitiesV81Flags,
    CapabilitiesV103Flags, CapabilitiesV104Flags, CapabilitiesV107Flags, CapabilitySet,
    CapabilityVersion, RawCapabilitySet,
};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
use ironrdp_server::{
    GfxDvcBridge, GfxServerFactory, GfxServerHandle, ServerEvent, ServerEventSender,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::info;

use crate::video::VideoHub;

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

/// Capability sets we offer. IronRDP 0.3 confirms `client & server` flags of the best common
/// version, so `AVC_DISABLED` must be in our set for it to survive: clients such as the
/// Windows App / Microsoft Remote Desktop on Android and iOS advertise `AVC_DISABLED`
/// (no H.264 decoder), and if the flag is dropped we confirm AVC and send AVC420 frames,
/// which those clients answer with a "protocol error" disconnect.
/// For clients that don't set it (mstsc, Windows App on macOS/Windows) the result is unchanged.
/// The version ladder is IronRDP's default; only the flags differ.
pub(crate) fn preferred_capabilities() -> Vec<CapabilitySet> {
    // Advertise each independently negotiated 10.x version that our AVC420 or
    // ClearCodec transport supports. A mobile client may offer V10.4 but not
    // V10.7; without the exact version it falls back unnecessarily to V10.
    // V10.1 implies AVC444v2, which we do not encode, so exclude it.
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

/// Whether the confirmed capability set lets us send AVC420 (H.264) frames.
/// Mirrors MS-RDPEGFX 2.2.3: V8 has no AVC, V8.1 needs `AVC420_ENABLED`, V10.x unless
/// `AVC_DISABLED`.
pub(crate) fn avc420_allowed(caps: &CapabilitySet) -> bool {
    match caps {
        CapabilitySet::V8 { .. } => false,
        CapabilitySet::V8_1 { flags } => flags.contains(CapabilitiesV81Flags::AVC420_ENABLED),
        CapabilitySet::V10 { flags } | CapabilitySet::V10_2 { flags } => {
            !flags.contains(CapabilitiesV10Flags::AVC_DISABLED)
        }
        CapabilitySet::V10_1 => true,
        CapabilitySet::V10_3 { flags } => !flags.contains(CapabilitiesV103Flags::AVC_DISABLED),
        CapabilitySet::V10_4 { flags }
        | CapabilitySet::V10_5 { flags }
        | CapabilitySet::V10_6 { flags }
        | CapabilitySet::V10_6Err { flags } => !flags.contains(CapabilitiesV104Flags::AVC_DISABLED),
        CapabilitySet::V10_7 { flags } => !flags.contains(CapabilitiesV107Flags::AVC_DISABLED),
    }
}

/// AVC444 uses the same AVC_DISABLED restriction, but requires V10 or newer.
pub(crate) fn avc444_allowed(caps: &CapabilitySet) -> bool {
    match caps {
        CapabilitySet::V8 { .. } | CapabilitySet::V8_1 { .. } => false,
        _ => avc420_allowed(caps),
    }
}

/// Human-readable wire-level capset diagnostics, including unknown Azure 11.x
/// versions. Never try to interpret an unknown bit pattern as AVC support.
fn capability_summary(raw: &RawCapabilitySet) -> String {
    let version = u32::from(raw.version);
    let flags = if raw.version == CapabilityVersion::V10_1 {
        "n/a".to_owned()
    } else if let Some(bytes) = raw.data.get(..4) {
        let value = u32::from_le_bytes(bytes.try_into().expect("exactly four flag bytes"));
        format!("0x{value:08X}")
    } else {
        "missing".to_owned()
    };
    let support = match raw.parsed() {
        Ok(Some(typed)) => format!(
            "AVC420={},AVC444={}",
            avc420_allowed(&typed),
            avc444_allowed(&typed)
        ),
        Ok(None) if (0x000B_0000..0x000C_0000).contains(&version) => {
            if let Some(bytes) = raw.data.get(..4) {
                let bits = u32::from_le_bytes(bytes.try_into().expect("four flag bytes"));
                format!(
                    "unknown-version,AVC_DISABLED={},HEVC_ENABLE={},AVCMM_ENABLE={},SCP_DISABLE={},SCALEDMAP_DISABLE={}",
                    bits & 0x20 != 0,
                    bits & 0x400 != 0,
                    bits & 0x800 != 0,
                    bits & 0x100 != 0,
                    bits & 0x80 != 0
                )
            } else {
                "unknown-version,invalid-flags".to_owned()
            }
        }
        Ok(None) => "unknown-version".to_owned(),
        Err(_) => "invalid-flags".to_owned(),
    };
    format!(
        "version=0x{version:08X}(flags={flags},{support},bytes={})",
        raw.data.len()
    )
}

/// Server-offered flags are an intersection mask in IronRDP 0.3, not
/// the client's decoder capabilities. In particular, setting AVC_DISABLED
/// in this mask *preserves* that bit when the client sends it; it does not
/// disable AVC for desktop clients that did not set it.
fn server_capability_mask(cap: &CapabilitySet) -> String {
    let raw = RawCapabilitySet::from(cap);
    let version = u32::from(raw.version);
    let flags = if raw.version == CapabilityVersion::V10_1 {
        "n/a".to_owned()
    } else if let Some(bytes) = raw.data.get(..4) {
        let value = u32::from_le_bytes(bytes.try_into().expect("four flag bytes"));
        format!("0x{value:08X}")
    } else {
        "missing".to_owned()
    };
    format!(
        "version=0x{version:08X}(flag_mask={flags},bytes={})",
        raw.data.len()
    )
}

impl GraphicsPipelineHandler for GfxHandler {
    fn capabilities_advertise(&mut self, pdu: &CapabilitiesAdvertisePdu) {
        let capabilities = pdu
            .0
            .iter()
            .map(capability_summary)
            .collect::<Vec<_>>()
            .join("; ");
        info!(
            gfx_session = self.hub.session_id(),
            client_capset_count = pdu.0.len(),
            client_capsets = %capabilities,
            "RDPGFX_CAPS_ADVERTISE"
        );
    }

    fn preferred_capabilities(&self) -> Vec<CapabilitySet> {
        let offered = preferred_capabilities();
        let capabilities = offered
            .iter()
            .map(server_capability_mask)
            .collect::<Vec<_>>()
            .join("; ");
        info!(
            gfx_session = self.hub.session_id(),
            server_capsets = %capabilities,
            "RDPGFX_SERVER_CAPABILITIES"
        );
        offered
    }

    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        let avc420 = avc420_allowed(negotiated);
        let avc444 = avc444_allowed(negotiated);
        let confirmed = capability_summary(&RawCapabilitySet::from(negotiated));
        info!(
            gfx_session = self.hub.session_id(),
            confirmed = %confirmed,
            avc420,
            avc444,
            "RDPGFX_CAPS_CONFIRM"
        );
        self.hub.on_ready(avc420, avc444);
    }

    fn on_frame_ack(&mut self, frame_id: u32, _queue_depth: u32, _total_frames_decoded: u32) {
        self.hub.on_frame_ack(frame_id);
    }

    fn on_close(&mut self) {
        self.hub.on_close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IronRDP 0.3's negotiation (`negotiate_capabilities`, private there): our versions in
    /// priority order, first one the client also offers, flags intersected.
    fn negotiate(client: &[CapabilitySet]) -> Option<CapabilitySet> {
        let ours = preferred_capabilities();
        ours.iter()
            .find_map(|s| {
                client
                    .iter()
                    .find(|c| c.version() == s.version())
                    .map(|c| (s, c))
            })
            .map(|(s, c)| match (c, s) {
                (CapabilitySet::V10_7 { flags: cf }, CapabilitySet::V10_7 { flags: sf }) => {
                    CapabilitySet::V10_7 { flags: *cf & *sf }
                }
                (CapabilitySet::V10 { flags: cf }, CapabilitySet::V10 { flags: sf }) => {
                    CapabilitySet::V10 { flags: *cf & *sf }
                }
                (CapabilitySet::V10_4 { flags: cf }, CapabilitySet::V10_4 { flags: sf }) => {
                    CapabilitySet::V10_4 { flags: *cf & *sf }
                }
                (CapabilitySet::V8_1 { flags: cf }, CapabilitySet::V8_1 { flags: sf }) => {
                    CapabilitySet::V8_1 { flags: *cf & *sf }
                }
                (CapabilitySet::V8 { flags: cf }, CapabilitySet::V8 { flags: sf }) => {
                    CapabilitySet::V8 { flags: *cf & *sf }
                }
                _ => s.clone(),
            })
    }

    #[test]
    fn android_client_keeps_avc_disabled() {
        // What the Windows App on Android/iOS advertises (no V10.7, AVC disabled).
        let client = [
            CapabilitySet::V8 {
                flags: CapabilitiesV8Flags::SMALL_CACHE,
            },
            CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::SMALL_CACHE,
            },
            CapabilitySet::V10 {
                flags: CapabilitiesV10Flags::SMALL_CACHE | CapabilitiesV10Flags::AVC_DISABLED,
            },
            CapabilitySet::V10_4 {
                flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED,
            },
        ];
        let negotiated = negotiate(&client).expect("common version");
        assert_eq!(
            negotiated,
            CapabilitySet::V10_4 {
                flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED
            }
        );
        assert!(!avc420_allowed(&negotiated));
    }

    #[test]
    fn desktop_client_still_gets_avc() {
        let client = [
            CapabilitySet::V10 {
                flags: CapabilitiesV10Flags::SMALL_CACHE,
            },
            CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::SMALL_CACHE,
            },
        ];
        let negotiated = negotiate(&client).expect("common version");
        assert_eq!(
            negotiated,
            CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::SMALL_CACHE
            }
        );
        assert!(avc420_allowed(&negotiated));
    }

    #[test]
    fn desktop_v10_4_without_avc_disabled_still_gets_avc() {
        let client = [CapabilitySet::V10_4 {
            flags: CapabilitiesV104Flags::SMALL_CACHE,
        }];
        let negotiated = negotiate(&client).expect("common version");
        assert_eq!(
            negotiated,
            CapabilitySet::V10_4 {
                flags: CapabilitiesV104Flags::SMALL_CACHE,
            }
        );
        assert!(avc420_allowed(&negotiated));
    }

    #[test]
    fn logs_keep_raw_flags_and_avc_disabled_restriction() {
        let cap = CapabilitySet::V10_4 {
            flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED,
        };
        let text = capability_summary(&RawCapabilitySet::from(&cap));
        assert!(text.contains("version=0x000A0400"));
        assert!(text.contains("flags=0x00000022"));
        assert!(text.contains("AVC420=false,AVC444=false"));
    }

    #[test]
    fn azure_v11_3_does_not_enable_hevc_on_ipad() {
        // Captured from iPad Windows App: V11.1/V11.3 both use 0x000001A2.
        let azure = RawCapabilitySet::new(
            CapabilityVersion(0x000B_0300),
            0x0000_01A2u32.to_le_bytes().to_vec(),
        );
        let summary = capability_summary(&azure);
        assert!(summary.contains("AVC_DISABLED=true"));
        assert!(summary.contains("HEVC_ENABLE=false"));
        assert!(summary.contains("AVCMM_ENABLE=false"));
        assert!(summary.contains("SCP_DISABLE=true"));
        assert!(summary.contains("SCALEDMAP_DISABLE=true"));
    }

    #[test]
    fn server_offer_flags_are_not_reported_as_decoder_capabilities() {
        let offered = CapabilitySet::V10_4 {
            flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED,
        };
        let summary = server_capability_mask(&offered);
        assert!(summary.contains("version=0x000A0400"));
        assert!(summary.contains("flag_mask=0x00000022"));
        assert!(!summary.contains("AVC420=false"));
    }

    #[test]
    fn logs_unknown_version_without_guessing_codec_support() {
        let raw = RawCapabilitySet::new(CapabilityVersion(0x000B0101), vec![0, 0, 0, 0]);
        let text = capability_summary(&raw);
        assert!(text.contains("version=0x000B0101"));
        assert!(text.contains("unknown-version"));
    }

    #[test]
    fn avc_rules_per_version() {
        assert!(!avc420_allowed(&CapabilitySet::V8 {
            flags: CapabilitiesV8Flags::SMALL_CACHE
        }));
        assert!(!avc420_allowed(&CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::SMALL_CACHE
        }));
        assert!(avc420_allowed(&CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::AVC420_ENABLED
        }));
        assert!(avc420_allowed(&CapabilitySet::V10_1));
        assert!(!avc420_allowed(&CapabilitySet::V10_7 {
            flags: CapabilitiesV107Flags::AVC_DISABLED
        }));
    }
}
