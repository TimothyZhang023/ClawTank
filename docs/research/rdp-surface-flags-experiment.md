# iPad Windows App: Surface Commands flags A/B experiment

**Draft experiment. Do not merge into main without a real-client result.**

## Finding

The old IronRDP server code uses `CmdFlags::all()` for its Demand Active
`TS_SURFCMDS_CAPABILITYSET`. `ironrdp-pdu 0.9.0` defines the three
documented flags, but also has a special `const _ = !0` bitflags mask.
As a result, `all()` serializes `0xFFFFFFFF`, including 29 unspecified
bits. The Microsoft MS-RDPBCGR 2.2.7.2.9 protocol lists only:

- `SET_SURFACE_BITS = 0x00000002`
- `FRAME_MARKER = 0x00000010`
- `STREAM_SURFACE_BITS = 0x00000040`

This experiment advertises `0x00000052` instead. This corrects the
declaration; **there is no evidence yet that this makes iPad enable AVC**.
The base branch, codec selection, H.264 encoder, resolution and
`Server Core Data.version = 0x00080004` stay unchanged.

## A/B test

1. On the same Mac, run the base PR #8 `feat/rdp-video-codec-policy`
   build (with short handshake logging), and record the iPad Windows App
   client `RDPGFX_CAPS_ADVERTISE`.
2. Run this draft branch's `ClawTank-arm64` build. Use the same
   iPad Windows App version, remote PC entry, codec mode `Auto`, network
   and display resolution.
3. In the handshake log, verify
   `RDP_HANDSHAKE_CAP_DETAIL rdp_stage="demand_active" cap_type="SurfaceCommands" detail="flags=0x00000052"`.
4. Compare the client `RDPGFX_CAPS_ADVERTISE`:
   V8.1 `flags=0x02` vs new `0x12`, and V10.x `0x22` vs `0x02`.
   If the client no longer sends `AVC_DISABLED`, confirm the resulting
   selected codec, actual decoded image and reconnect stability.
5. If client flags are unchanged, this was a conformance improvement,
   **not** the trigger for AVC enablement. Keep H.264 disabled for this
   client and investigate other Demand Active sets.

Filter:

```bash
log stream --info --style compact \
  --predicate 'subsystem == "dev.clawtank.ClawTank" AND category == "core" AND (eventMessage CONTAINS "RDPGFX_" OR eventMessage CONTAINS "RDP_HANDSHAKE_")'
```

Spec: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/aa953018-c0a8-4761-bb12-86586c2cd56a
