# iPad Windows App: RDP 10 Server Core version A/B experiment

**Experimental, do not merge into main before iPad interop testing.**

The existing ClawTank release uses IronRDP acceptor 0.10.0, which sends
`TS_UD_SC_CORE.version = 0x00080004` (RDP 8.1 or older).
This experiment changes **only that on-wire field** to `0x00080005` (RDP 10.0).
It does not change EGFX flags, video encoding, security or H.264/H.265 capability checks.
Source: pinned IronRDP acceptor 0.10.0, vendored with one-line change in
`core/vendor/ironrdp-acceptor/src/connection.rs`.
The checkout uses a Cargo path override; the feature branch remains unchanged.

Compare the base ClawTank build and this experiment on the same Mac and iPad
Windows App, same address and same connection settings (Auto codec).
Check both runs with:

```bash
log stream --info --style compact \
  --predicate 'subsystem == "dev.clawtank.ClawTank" AND category == "core" AND (eventMessage CONTAINS "RDPGFX_" OR eventMessage CONTAINS "RDP_HANDSHAKE_")'
```

The experiment should log `server_core_version="0x00080005"`.
Check whether the **client's** V8.1 flags now contain `0x10` (AVC420),
or whether V10/V11 stop containing `0x20` (AVC_DISABLED).
If neither changes, the server core version alone isn't the trigger.
Never manually clear a client-provided AVC_DISABLED bit.
If any client fails to connect, return to the base build.

Ref: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/379a020e-9925-4b4f-98f3-7d634e10b411
