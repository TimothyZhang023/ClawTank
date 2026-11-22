# ClawTank RDP graphics codec matrix and rollout

The RDP server is [Devolutions/IronRDP](https://github.com/Devolutions/IronRDP):
\`ironrdp-server 0.13.0\` (locally patched), \`ironrdp-egfx 0.3.0\`,
\`ironrdp-graphics 0.9.0\`. These crates are pinned deliberately.

## Two distinct graphics paths

* **Legacy surface commands:** IronRDP \`RdpServerDisplayUpdates\` negotiates
  RDP 6.0 bitmap compression and legacy RemoteFX (RFX), if the client offers
  them. The builder's default \`server_codecs_capabilities(&[])\` offers RFX.
  The current \`TestPatternDisplay\` provides a static bitmap, not live capture.
  **Legacy RFX support does not accelerate ClawTank's live desktop today.**
* **RDPGFX / EGFX DVC:** ClawTank's \`VideoHub\` supplies live screen content
  through IronRDP \`GraphicsPipelineServer\`. Supported wire paths:
  \`ClearCodec\` BGRA changed tiles, \`AVC420\`, and \`AVC444\` paired H.264
  access units. Capabilities, frame acknowledgements and backpressure are
  coordinated here.

## Preferred codec choices

| User mode | Negotiated EGFX capability | Live wire format | Status |
| --- | --- | --- | --- |
| Auto | V8.1 AVC420 or V10.x without AVC_DISABLED | AVC420 | Implemented |
| Auto | AVC_DISABLED / older EGFX | ClearCodec | Implemented |
| ClearCodec | Any supported EGFX version | Lossless changed tiles, adaptive 64/128 | Implemented |
| AVC420 | Client permits AVC420 | VideoToolbox H.264 | Implemented |
| AVC444 | Client permits AVC444 (V10+) | VideoToolbox dual H.264 + AVC444 v1 EGFX | Experimental; verify desktop clients |
| AVC444 | Client sets AVC_DISABLED | ClearCodec | Safety fallback |
| HEVC | Azure EGFX 11.x extension | HEVC420/444 | Not yet implemented; no negotiated support in pinned \`ironrdp-egfx\` |

On the tested iPad Windows App the advertised V10.4 capabilities contain
\`AVC_DISABLED\`. That client **must** receive ClearCodec. Do not send either
AVC420 or AVC444 by overriding the server preference. Keep Auto as default.

AVC444 is the RDP dual-view format, *not* a single H.264 High 4:4:4 profile
bitstream. ClawTank produces the two YUV420 views and synchronizes them
before passing them to \`GraphicsPipelineServer::send_avc444_frame\`. Its
CPU color conversion and per-frame flush are correct-first and need profiling.

## Advanced codecs: migration options

**RemoteFX classic (MS-RDPRFX):** already in IronRDP's legacy bitmap encoder.
It is unrelated to RemoteFX **vGPU** (removed from Windows in 2021).
A live fallback would require feeding captured frames through
\`RdpServerDisplayUpdates\`; the current test-pattern source is not enough.

**RemoteFX Progressive (MS-RDPEGFX):** IronRDP implements encode primitives and
supports the \`WireToSurface2\` PDU and \`MixedTilePayload::RemoteFxProgressive\`.
To enable for real screen updates, implement a stateful encoder of progressive
blocks, quantization, tile-cache lifecycle and frame/region updates, then
verify clients understand both first/upgrade passes. It cannot be enabled by
setting a codec ID alone.

**Hybrid/mixed EGFX:** the transport already supports ClearCodec, AVC420,
AVC444 and progressive tiles in one frame. Policy should classify changed
regions (static text/solid UI, gradients/photos, motion), choose a codec by
region, and maintain reference state and per-codec frame ordering. Never send
AVC beside another codec on a capability version that does not promise mixing.
For AVC444 especially, avoid mixing overlapping tiles until tested.

**Azure HEVC (H.265):** newer Windows App/Azure sessions can use undocumented
Azure graphics capability versions and codec IDs (\`HEVC420=0x0010\` and
\`HEVC444\` in FreeRDP \`WITH_GFX_AZURE\`). Standard IronRDP 0.3 EGFX stops
at V10.7 and cannot accept those Azure capability sets or encode their
HEVC metadata/NAL-size table. First extend/fork the EGFX PDU/state machine and
test a client which actually advertises Azure 11.x; then add the macOS
VideoToolbox HEVC encoder and verified decoder/interoperability tests.
**Do not reinterpret V10.4 as HEVC-capable.**

## Verification gates

1. Rust tests: advertised/confirmed codec restrictions; V10.4 iOS
   AVC_DISABLED; desktop AVC420/AVC444; no H.264 on raw sessions.
2. macOS tests: both AVC444 planes and H.264 access units; Intel software
   fallback and arm64 hardware support.
3. FreeRDP interoperability, dual-architecture macOS CI, artifact packaging.
4. Manual: compare iPad before/after adaptive ClearCodec at fixed resolution
   (fps, encode ms, tile count, kB/frame, ACK latency, decode errors).
5. Manual: AVC444 desktop client screenshot with thin colored text/grids;
   verify there are no rainbow artifacts and that resize/reconnect recovers.
6. Azure HEVC only after an Azure-capability capture and client decode test.

## References

* IronRDP: https://github.com/Devolutions/IronRDP
* FreeRDP Azure extension: https://github.com/FreeRDP/FreeRDP/blob/master/include/freerdp/channels/rdpgfx.h
* Microsoft RemoteFX vGPU removal: https://support.microsoft.com/en-us/topic/kb4570006-update-to-disable-and-remove-the-remotefx-vgpu-component-in-windows


## Negotiation diagnostics (INFO; one set per EGFX connection)

Run this on the Mac while connecting with Windows App or another RDP client:

\`\`\`bash
log stream --info --style compact \
  --predicate 'subsystem == "dev.clawtank.ClawTank" AND category == "core" AND eventMessage CONTAINS "RDPGFX_"'
\`\`\`

The macOS app forwards Rust \`tracing\` INFO events through \`CoreBridge\` to
the \`dev.clawtank.ClawTank/core\` unified log. No debug mode is required.
Entries share a \`gfx_session\` number (starts at 1 per server process):

1. \`RDPGFX_CAPS_ADVERTISE\`: all *client-offered* versions, raw flag values and,
   for known versions, AVC420/AVC444 decoder availability. Unknown Azure 11.x
   versions are logged as \`unknown-version\` rather than assumed supported.
2. \`RDPGFX_SERVER_CAPABILITIES\`: all server-offered EGFX versions and flags.
3. \`RDPGFX_CAPS_CONFIRM\`: the single negotiated version/flags and allowed
   codec families.
4. \`RDPGFX_CODEC_SELECTED\`: requested setting, effective wire codec, display
   size, and explicit fallback reason (for example
   \`fallback-client-avc-unavailable\` or \`fallback-avc444-to-avc420\`).
5. \`RDPGFX_FIRST_FRAME_SENT\`: actual first successful video submission
   (ClearCodec, AVC420 or AVC444), not just a completed negotiation.
6. \`RDPGFX_CHANNEL_CLOSED\`: EGFX session end.

For the tested iPad with V10.4 + \`AVC_DISABLED\`, step 3 should show
\`AVC420=false,AVC444=false\`; step 4 should select \`Raw\`, and step 5 should
send \`ClearCodec\`. If the first-frame entry never appears, investigate
capture permissions, backpressure and the VideoToolbox/CG output rather than
blaming capability negotiation.

This tracing logs codec metadata, not screen pixels or passwords.
