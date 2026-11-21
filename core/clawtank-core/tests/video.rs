//! End-to-end EGFX test: an IronRDP client that offers the Graphics Pipeline connects, the
//! core reports `VideoStart`, and H.264 submitted through `VideoHub` reaches the client as
//! AVC420 byte-for-byte. Also checks the keyframe rule (no P-frames before the first IDR).

#[path = "support/client.rs"]
mod client;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clawtank_core::events::CoreEvent;
use clawtank_core::server::{self, ServerConfig};
use clawtank_core::tls::TlsIdentity;
use clawtank_core::video::{StreamCodec, SubmitResult};

const USER: &str = "tester";
const PASSWORD: &str = "video-test-password";

/// Not decodable H.264, just Annex-B-shaped bytes (SPS, PPS, IDR slice); the server treats the
/// payload as opaque, and the test client records rather than decodes.
const FAKE_IDR: &[u8] = &[
    0, 0, 0, 1, 0x67, 0x4d, 0x00, 0x2a, 0xab, //
    0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80, //
    0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33, 0xff,
];
const FAKE_P: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x02, 0x04];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submitted_h264_reaches_an_egfx_client() {
    let events: Arc<Mutex<Vec<CoreEvent>>> = Arc::default();
    let log = Arc::clone(&events);
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        username: USER.to_owned(),
        password: PASSWORD.to_owned(),
        identity: TlsIdentity::generate("video-test.local").unwrap(),
        policy: clawtank_core::access::AccessPolicy::lan_only(),
    };
    let server = server::start(
        config,
        Arc::new(move |e| log.lock().unwrap().push(e)),
        Arc::new(|_| {}),
    )
    .unwrap();
    let video = Arc::clone(server.video());
    assert_eq!(
        video.submit_h264(FAKE_IDR, true, 1512, 982),
        SubmitResult::NotReady,
        "no session yet"
    );

    let recorder = client::EgfxRecorder::default();
    let (result, mut session, _) = client::connect(
        server.local_addr(),
        USER,
        PASSWORD,
        1512,
        982,
        Some(&recorder),
    )
    .await
    .expect("connect with EGFX");

    // Pump until the server reports the pipeline ready (capability exchange runs over DVC).
    let started = {
        let events = Arc::clone(&events);
        move || {
            events.lock().unwrap().iter().any(|e| {
                matches!(
                    e,
                    CoreEvent::VideoStart {
                        width: 1512,
                        height: 982
                    }
                )
            })
        }
    };
    // `pump_until` consumes the connection result, so build a stage once and keep pumping.
    let ok = client::pump_until(result, &mut session, Duration::from_secs(15), {
        let video = Arc::clone(&video);
        let recorder = recorder.clone();
        let mut submitted = false;
        move || {
            if !started() {
                return false;
            }
            if !submitted {
                // The decoder chain must start at an IDR.
                assert_eq!(
                    video.submit_h264(FAKE_P, false, 1512, 982),
                    SubmitResult::NeedKeyframe
                );
                assert_eq!(
                    video.submit_h264(FAKE_IDR, true, 1512, 982),
                    SubmitResult::Sent
                );
                assert_eq!(
                    video.submit_h264(FAKE_P, false, 1512, 982),
                    SubmitResult::Sent
                );
                submitted = true;
            }
            recorder.frame_count() >= 2
        }
    })
    .await
    .expect("session");
    assert!(ok, "timed out; events: {:?}", events.lock().unwrap());

    let streams = recorder.bitstreams.lock().unwrap().clone();
    assert_eq!(
        streams,
        vec![FAKE_IDR.to_vec(), FAKE_P.to_vec()],
        "bitstreams must arrive unchanged"
    );
    assert_eq!(*recorder.reset_size.lock().unwrap(), Some((1512, 982)));
    // Acks drive adaptive bitrate: the client acknowledges each frame and we time it.
    // The client sent its acks while pumping above; give the server a moment to process them.
    let mut ok = false;
    for _ in 0..100 {
        if video.stats().frames_acked >= 2 {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stats = video.stats();
    assert!(ok, "client acknowledged frames: {stats:?}");
    assert_eq!((stats.frames_sent, stats.frames_dropped), (2, 1));
    assert!(stats.ack_latency_us > 0, "ack latency measured: {stats:?}");

    drop(session);
    for _ in 0..100 {
        if events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, CoreEvent::VideoStop))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, CoreEvent::VideoStop))
    );
    assert_eq!(
        video.submit_h264(FAKE_IDR, true, 1512, 982),
        SubmitResult::NotReady,
        "session is gone"
    );
    server.stop();
}

/// Regression: a Retina-sized client must survive the initial bitmap and EGFX handshake.
/// Tests the 2776×1938 size from an Intel Mac beta report without requiring real capture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retina_sized_egfx_session_reaches_first_frame() {
    let events: Arc<Mutex<Vec<CoreEvent>>> = Arc::default();
    let log = Arc::clone(&events);
    let server = server::start(
        ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            username: USER.to_owned(),
            password: PASSWORD.to_owned(),
            identity: TlsIdentity::generate("retina-test.local").unwrap(),
            policy: clawtank_core::access::AccessPolicy::lan_only(),
        },
        Arc::new(move |event| log.lock().unwrap().push(event)),
        Arc::new(|_| {}),
    )
    .unwrap();

    let video = Arc::clone(server.video());
    let recorder = client::EgfxRecorder::default();
    let (result, mut session, _) = client::connect(
        server.local_addr(),
        USER,
        PASSWORD,
        2776,
        1938,
        Some(&recorder),
    )
    .await
    .expect("Retina-sized RDP connection");

    let mut submitted = false;
    let connection_started = std::time::Instant::now();
    let ok = client::pump_until(result, &mut session, Duration::from_secs(20), {
        let events = Arc::clone(&events);
        let recorder = recorder.clone();
        move || {
            if !submitted
                && events.lock().unwrap().iter().any(|event| {
                    matches!(
                        event,
                        CoreEvent::VideoStart {
                            width: 2776,
                            height: 1938
                        }
                    )
                })
            {
                assert_eq!(
                    video.submit_h264(FAKE_IDR, true, 2776, 1938),
                    SubmitResult::Sent
                );
                submitted = true;
            }
            // Stay connected beyond the initial legacy-frame grace period to detect
            // premature protocol disconnects rather than stopping at the first H.264 frame.
            recorder.frame_count() >= 1
                && connection_started.elapsed() >= Duration::from_millis(1500)
        }
    })
    .await
    .expect("Retina-sized EGFX session did not terminate unexpectedly");
    assert!(
        ok,
        "No first EGFX frame at 2776×1938: {:?}",
        events.lock().unwrap()
    );
    assert_eq!(*recorder.reset_size.lock().unwrap(), Some((2776, 1938)));

    drop(session);
    server.stop();
}

/// The real iOS Windows App advertises AVC_DISABLED. It must NOT receive an EGFX
/// CapabilitiesConfirm: its traditional RDP BitmapUpdate path can display a live
/// compressed desktop instead of stalling on huge uncompressed EGFX packets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ios_no_avc_declines_egfx_and_streams_live_legacy_bitmap() {
    let events: Arc<Mutex<Vec<CoreEvent>>> = Arc::default();
    let log = Arc::clone(&events);
    let server = server::start(
        ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            username: USER.to_owned(),
            password: PASSWORD.to_owned(),
            identity: TlsIdentity::generate("ios-no-avc.local").unwrap(),
            policy: clawtank_core::access::AccessPolicy::lan_only(),
        },
        Arc::new(move |event| log.lock().unwrap().push(event)),
        Arc::new(|_| {}),
    )
    .unwrap();
    let video = Arc::clone(server.video());
    let recorder = client::EgfxRecorder::default();
    recorder.advertise_no_avc.store(true, Ordering::Release);
    let (result, mut session, _) = client::connect(
        server.local_addr(),
        USER,
        PASSWORD,
        320,
        240,
        Some(&recorder),
    )
    .await
    .expect("iOS-like RDP connection");

    let send = tokio::spawn(async move {
        let start = std::time::Instant::now();
        while video.codec() != StreamCodec::Uncompressed {
            assert!(
                start.elapsed() < Duration::from_secs(8),
                "no codec negotiation"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // The initial legacy test pattern goes out at 1 second; follow it with
        // a different live image to verify the screen is not stuck on color bars.
        tokio::time::sleep(Duration::from_millis(1400)).await;
        let bgra = [0_u8, 0, 255, 255].repeat(320 * 240);
        assert_eq!(
            video.submit_h264(FAKE_IDR, true, 320, 240),
            SubmitResult::NotReady,
        );
        assert_eq!(
            video.submit_uncompressed(&bgra, 320, 240),
            SubmitResult::Sent,
        );
    });
    client::receive_legacy_live_color(result, &mut session, Duration::from_secs(20), [255, 0, 0])
        .await
        .expect("iOS-like client did not render live legacy desktop");
    send.await.expect("legacy frame submission panicked");
    // The client never got a CapsConfirm. Sending legacy BitmapUpdate while an
    // EGFX graphics pipeline is confirmed is the exact iOS protocol error.
    assert!(recorder.negotiated.lock().unwrap().is_none());
    assert_eq!(recorder.frame_count(), 0, "EGFX frames must not be sent");
    drop(session);
    server.stop();
}
