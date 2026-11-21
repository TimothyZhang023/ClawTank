// One client's video stream: ScreenCapture → (optional StatsOverlay) → H264Encoder → core.
//
// Backpressure is applied *before* encoding: if the core says a frame would not be sent, the
// captured frame is skipped (nothing was encoded, so the reference chain is intact). If the
// core ever refuses an encoded frame, the next frame is forced to be an IDR.

import CoreMedia
import CoreVideo
import Foundation
import os

final class VideoSession: @unchecked Sendable {
    /// Monotonic counters; the controller turns deltas into per-second rates.
    struct Counters: Equatable {
        var capturedFrames = 0
        var skippedFrames = 0
        var encodedFrames = 0
        var keyframes = 0
        var sentBytes = 0
        var encodeSeconds = 0.0
    }

    let contentWidth: Int
    let contentHeight: Int
    private let server: RdpServer
    private let usesUncompressed: Bool
    private let overlayEnabled: @Sendable () -> Bool
    private let lock = NSLock()
    private var counters = Counters()
    private var needKeyframe = true
    private var overlayText = ""
    private var encoder: H264Encoder?
    private var capture: ScreenCapture?
    private let overlay = StatsOverlay()
    private var adaptive: AdaptiveBitrate?
    private let log = Logger(subsystem: "dev.clawtank.ClawTank", category: "video")

    let target: DisplayTarget

    let frameRate: Int
    let bitRateFactor: Double

    init(server: RdpServer, width: Int, height: Int, target: DisplayTarget, frameRate: Int = 60,
         bitRateFactor: Double = 1, overlayEnabled: @escaping @Sendable () -> Bool) {
        self.server = server
        self.usesUncompressed = server.usesUncompressedVideo
        self.target = target
        self.frameRate = frameRate
        self.bitRateFactor = bitRateFactor
        self.contentWidth = width
        self.contentHeight = height
        self.overlayEnabled = overlayEnabled
    }

    var codedWidth: Int {
        usesUncompressed ? contentWidth : H264Encoder.Settings.aligned(contentWidth)
    }
    var codedHeight: Int {
        usesUncompressed ? contentHeight : H264Encoder.Settings.aligned(contentHeight)
    }

    var isHardwareAccelerated: Bool { encoder?.isHardwareAccelerated ?? false }

    func snapshot() -> Counters {
        lock.withLock { counters }
    }

    /// Text drawn by the overlay (set by the controller once per second).
    func setOverlayText(_ text: String) {
        lock.withLock { overlayText = text }
    }

    /// Feeds one second of network measurements; returns the bit rate in use and whether the
    /// link looked congested.
    func adapt(_ sample: AdaptiveBitrate.Sample) -> (bitRate: Int, congested: Bool)? {
        let (changed, state) = lock.withLock { () -> (Int?, AdaptiveBitrate?) in
            let changed = adaptive?.update(sample)
            return (changed, adaptive)
        }
        if let changed { encoder?.setBitRate(changed) }
        return state.map { ($0.current, $0.congested) }
    }

    func start() async throws {
        if usesUncompressed {
            log.notice("EGFX client disabled AVC420; using uncompressed BGRA at up to 5 fps")
        } else {
            let baseRate = H264Encoder.Settings.defaultBitRate(
                width: codedWidth, height: codedHeight, frameRate: frameRate)
            // Not every Intel/AMD Mac exposes low-latency hardware H.264.
            #if arch(x86_64)
            let allowSoftwareFallback = true
            #else
            let allowSoftwareFallback = false
            #endif
            let settings = H264Encoder.Settings(
                width: codedWidth, height: codedHeight, frameRate: frameRate,
                averageBitRate: max(1_000_000, Int(Double(baseRate) * bitRateFactor)),
                lowLatencyRateControl: true, allowSoftwareFallback: allowSoftwareFallback)
            lock.withLock { adaptive = AdaptiveBitrate(target: settings.averageBitRate) }
            let encoder = try H264Encoder(settings: settings) { [weak self] frame in
                self?.deliver(frame)
            }
            self.encoder = encoder
            log.info("Encoder \(settings.width)x\(settings.height) \(encoder.encoderID, privacy: .public)")
            if !encoder.isHardwareAccelerated {
                log.warning("Hardware H.264 unavailable; using VideoToolbox software encoder (higher CPU usage)")
            }
        }

        let capture = ScreenCapture(
            onFrame: { [weak self] buffer, time in self?.handle(buffer, time) },
            onStop: { [weak self] _ in self?.log.notice("Capture ended") })
        let effectiveFPS = usesUncompressed ? min(frameRate, 5) : frameRate
        try await capture.start(.init(
            contentWidth: contentWidth, contentHeight: contentHeight,
            codedWidth: codedWidth, codedHeight: codedHeight, frameRate: effectiveFPS,
            pixelFormat: usesUncompressed ? kCVPixelFormatType_32BGRA : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            displaySize: target.bounds.size), displayID: target.displayID)
        self.capture = capture
    }

    func stop() async {
        await capture?.stop()
        capture = nil
        encoder?.flush()
        encoder = nil
    }

    private func handle(_ buffer: CVPixelBuffer, _ time: CMTime) {
        lock.withLock { counters.capturedFrames += 1 }
        guard server.canAcceptVideo else {
            lock.withLock { counters.skippedFrames += 1 }
            return
        }
        if usesUncompressed {
            deliverUncompressed(buffer)
            return
        }
        guard let encoder else { return }
        let (forceKeyframe, text) = lock.withLock { () -> (Bool, String) in
            let force = needKeyframe
            needKeyframe = false
            return (force, overlayText)
        }
        var frame = buffer
        if overlayEnabled(), let drawn = overlay.render(text, onto: buffer) {
            frame = drawn
        }
        encoder.encode(frame, presentationTime: time, forceKeyframe: forceKeyframe)
    }

    /// ScreenCaptureKit supplies BGRA for no-AVC EGFX clients. Its IOSurface stride may
    /// include row padding, so repack into exactly width * height * 4 bytes.
    private func deliverUncompressed(_ buffer: CVPixelBuffer) {
        guard CVPixelBufferGetPixelFormatType(buffer) == kCVPixelFormatType_32BGRA,
              CVPixelBufferGetWidth(buffer) == contentWidth,
              CVPixelBufferGetHeight(buffer) == contentHeight else { return }
        guard CVPixelBufferLockBaseAddress(buffer, .readOnly) == kCVReturnSuccess else { return }
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(buffer) else { return }
        let rowBytes = contentWidth * 4
        let stride = CVPixelBufferGetBytesPerRow(buffer)
        guard stride >= rowBytes else { return }

        let data: Data
        if stride == rowBytes {
            data = Data(bytes: base, count: rowBytes * contentHeight)
        } else {
            var packed = Data(count: rowBytes * contentHeight)
            packed.withUnsafeMutableBytes { raw in
                guard let destination = raw.baseAddress else { return }
                for row in 0..<contentHeight {
                    destination.advanced(by: row * rowBytes).copyMemory(
                        from: base.advanced(by: row * stride), byteCount: rowBytes)
                }
            }
            data = packed
        }

        let result = server.submitBGRA(data, width: contentWidth, height: contentHeight)
        lock.withLock {
            if result == .sent {
                counters.encodedFrames += 1
                counters.sentBytes += data.count
            } else {
                counters.skippedFrames += 1
            }
        }
    }

    private func deliver(_ frame: EncodedFrame) {
        let result = server.submitH264(frame.data, keyframe: frame.isKeyframe,
                                       width: contentWidth, height: contentHeight)
        lock.withLock {
            counters.encodedFrames += 1
            counters.encodeSeconds += frame.encodeDuration
            switch result {
            case .sent:
                counters.sentBytes += frame.data.count
                if frame.isKeyframe { counters.keyframes += 1 }
            case .needKeyframe:
                needKeyframe = true
            case .notReady:
                break
            }
        }
    }
}
