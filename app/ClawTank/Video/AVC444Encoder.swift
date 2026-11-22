// AVC444 (MS-RDPEGFX 3.3.8.3.2): decompose BGRA into two NV12/H.264 views.
// The first view transports Y and subsampled UV; the auxiliary view restores the
// missing chroma at odd pixels. These are distinct H.264 streams, NOT H.264 4:4:4.
// Keep the encoders on one serial queue: flush() pairs access units by capture frame.

import CoreMedia
import CoreVideo
import Foundation

private final class AVC444OutputSink: @unchecked Sendable {
    private let lock = NSLock()
    private var main: EncodedFrame?
    private var auxiliary: EncodedFrame?

    func reset() {
        lock.withLock {
            main = nil
            auxiliary = nil
        }
    }

    func setMain(_ frame: EncodedFrame) { lock.withLock { main = frame } }
    func setAuxiliary(_ frame: EncodedFrame) { lock.withLock { auxiliary = frame } }

    func take() -> (EncodedFrame, EncodedFrame)? {
        lock.withLock {
            guard let main, let auxiliary else { return nil }
            return (main, auxiliary)
        }
    }
}

final class AVC444Encoder: @unchecked Sendable {
    struct Output {
        let luma: Data
        let chroma: Data
        let isKeyframe: Bool
        let encodeDuration: TimeInterval
    }

    private let sink: AVC444OutputSink
    private let main: H264Encoder
    private let auxiliary: H264Encoder

    init(settings: H264Encoder.Settings) throws {
        let collector = AVC444OutputSink()
        sink = collector
        var perStream = settings
        perStream.averageBitRate = max(1_000_000, settings.averageBitRate / 2)
        main = try H264Encoder(settings: perStream) { collector.setMain($0) }
        auxiliary = try H264Encoder(settings: perStream) { collector.setAuxiliary($0) }
    }

    var isHardwareAccelerated: Bool {
        main.isHardwareAccelerated && auxiliary.isHardwareAccelerated
    }

    func setBitRate(_ combinedBitsPerSecond: Int) {
        let each = max(500_000, combinedBitsPerSecond / 2)
        main.setBitRate(each)
        auxiliary.setBitRate(each)
    }

    /// Must be called serially. Synchronous pairing is intentional: mixing
    /// different timestamps would yield color trails and decoder corruption.
    func encode(_ buffer: CVPixelBuffer, presentationTime: CMTime, forceKeyframe: Bool) -> Output? {
        guard let (mainBuffer, auxiliaryBuffer) = Self.splitBGRA(buffer) else { return nil }
        sink.reset()
        main.encode(mainBuffer, presentationTime: presentationTime, forceKeyframe: forceKeyframe)
        auxiliary.encode(auxiliaryBuffer, presentationTime: presentationTime, forceKeyframe: forceKeyframe)
        main.flush()
        auxiliary.flush()
        guard let (primary, chroma) = sink.take() else { return nil }
        // Both views must start with a keyframe; if either fails, restart both.
        guard primary.isKeyframe == chroma.isKeyframe else { return nil }
        return Output(luma: primary.data, chroma: chroma.data,
                      isKeyframe: primary.isKeyframe,
                      encodeDuration: max(primary.encodeDuration, chroma.encodeDuration))
    }

    func flush() {
        main.flush()
        auxiliary.flush()
    }

    /// The two streams are planar NV12 with video-range YUV values. AVC444 v1
    /// interleaves auxiliary odd-row U/V values in 8-row bands of its Y plane.
    /// The auxiliary UV plane contains even-row, odd-column chroma samples.
    static func splitBGRA(_ source: CVPixelBuffer) -> (CVPixelBuffer, CVPixelBuffer)? {
        guard CVPixelBufferGetPixelFormatType(source) == kCVPixelFormatType_32BGRA else { return nil }
        let width = CVPixelBufferGetWidth(source)
        let height = CVPixelBufferGetHeight(source)
        guard width > 0, height > 0, width % 16 == 0, height % 16 == 0 else { return nil }
        guard let first = makeNV12(width: width, height: height),
              let second = makeNV12(width: width, height: height) else { return nil }
        guard CVPixelBufferLockBaseAddress(source, .readOnly) == kCVReturnSuccess else { return nil }
        defer { CVPixelBufferUnlockBaseAddress(source, .readOnly) }
        guard CVPixelBufferLockBaseAddress(first, []) == kCVReturnSuccess else { return nil }
        defer { CVPixelBufferUnlockBaseAddress(first, []) }
        guard CVPixelBufferLockBaseAddress(second, []) == kCVReturnSuccess else { return nil }
        defer { CVPixelBufferUnlockBaseAddress(second, []) }

        guard let src = CVPixelBufferGetBaseAddress(source)?.assumingMemoryBound(to: UInt8.self),
              let y1 = CVPixelBufferGetBaseAddressOfPlane(first, 0)?.assumingMemoryBound(to: UInt8.self),
              let uv1 = CVPixelBufferGetBaseAddressOfPlane(first, 1)?.assumingMemoryBound(to: UInt8.self),
              let y2 = CVPixelBufferGetBaseAddressOfPlane(second, 0)?.assumingMemoryBound(to: UInt8.self),
              let uv2 = CVPixelBufferGetBaseAddressOfPlane(second, 1)?.assumingMemoryBound(to: UInt8.self)
        else { return nil }

        let srcStride = CVPixelBufferGetBytesPerRow(source)
        let y1Stride = CVPixelBufferGetBytesPerRowOfPlane(first, 0)
        let uv1Stride = CVPixelBufferGetBytesPerRowOfPlane(first, 1)
        let y2Stride = CVPixelBufferGetBytesPerRowOfPlane(second, 0)
        let uv2Stride = CVPixelBufferGetBytesPerRowOfPlane(second, 1)

        for row in stride(from: 0, to: height, by: 2) {
            let half = row / 2
            let band = (half & ~7) + half
            for col in stride(from: 0, to: width, by: 2) {
                let a = sample(src + row * srcStride + col * 4)
                let b = sample(src + row * srcStride + (col + 1) * 4)
                let c = sample(src + (row + 1) * srcStride + col * 4)
                let d = sample(src + (row + 1) * srcStride + (col + 1) * 4)
                y1[row * y1Stride + col] = a.y
                y1[row * y1Stride + col + 1] = b.y
                y1[(row + 1) * y1Stride + col] = c.y
                y1[(row + 1) * y1Stride + col + 1] = d.y

                // Match the FreeRDP AVC444 BGRA decomposition: base UV average.
                uv1[half * uv1Stride + col] =
                    UInt8((Int(a.u) + Int(b.u) + Int(c.u) + Int(d.u)) / 4)
                uv1[half * uv1Stride + col + 1] =
                    UInt8((Int(a.v) + Int(b.v) + Int(c.v) + Int(d.v)) / 4)

                // B45: odd image rows' full-resolution chroma. U and V are
                // stored in separate 8-line bands in auxiliary luma.
                y2[band * y2Stride + col] = c.u
                y2[band * y2Stride + col + 1] = d.u
                y2[(band + 8) * y2Stride + col] = c.v
                y2[(band + 8) * y2Stride + col + 1] = d.v
                // B6/B7: even-row odd-column chroma in auxiliary UV.
                uv2[half * uv2Stride + col] = b.u
                uv2[half * uv2Stride + col + 1] = b.v
            }
        }
        return (first, second)
    }

    private static func makeNV12(width: Int, height: Int) -> CVPixelBuffer? {
        var buffer: CVPixelBuffer?
        let attributes: [CFString: Any] = [
            kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary
        ]
        let status = CVPixelBufferCreate(
            nil, width, height, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            attributes as CFDictionary, &buffer)
        return status == kCVReturnSuccess ? buffer : nil
    }

    private struct Yuv {
        let y: UInt8
        let u: UInt8
        let v: UInt8
    }

    /// BT.601 video-range YUV conversion. The paired views use the same matrix.
    private static func sample(_ pixel: UnsafePointer<UInt8>) -> Yuv {
        let b = Int(pixel[0])
        let g = Int(pixel[1])
        let r = Int(pixel[2])
        func clamp(_ value: Int) -> UInt8 { UInt8(clamping: value) }
        return Yuv(
            y: clamp(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16),
            u: clamp(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128),
            v: clamp(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128))
    }
}
