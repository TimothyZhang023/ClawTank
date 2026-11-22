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
              let mainLuma = CVPixelBufferGetBaseAddressOfPlane(first, 0)?.assumingMemoryBound(to: UInt8.self),
              let uv1 = CVPixelBufferGetBaseAddressOfPlane(first, 1)?.assumingMemoryBound(to: UInt8.self),
              let auxLuma = CVPixelBufferGetBaseAddressOfPlane(second, 0)?.assumingMemoryBound(to: UInt8.self),
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
                let topLeft = sample(src + row * srcStride + col * 4)
                let topRight = sample(src + row * srcStride + (col + 1) * 4)
                let bottomLeft = sample(src + (row + 1) * srcStride + col * 4)
                let bottomRight = sample(src + (row + 1) * srcStride + (col + 1) * 4)
                mainLuma[row * y1Stride + col] = topLeft.y
                mainLuma[row * y1Stride + col + 1] = topRight.y
                mainLuma[(row + 1) * y1Stride + col] = bottomLeft.y
                mainLuma[(row + 1) * y1Stride + col + 1] = bottomRight.y

                // Match the FreeRDP AVC444 BGRA decomposition: base UV average.
                uv1[half * uv1Stride + col] =
                    UInt8((Int(topLeft.cb) + Int(topRight.cb) + Int(bottomLeft.cb) + Int(bottomRight.cb)) / 4)
                uv1[half * uv1Stride + col + 1] =
                    UInt8((Int(topLeft.cr) + Int(topRight.cr) + Int(bottomLeft.cr) + Int(bottomRight.cr)) / 4)

                // B45: odd image rows' full-resolution chroma. U and V are
                // stored in separate 8-line bands in auxiliary luma.
                auxLuma[band * y2Stride + col] = bottomLeft.cb
                auxLuma[band * y2Stride + col + 1] = bottomRight.cb
                auxLuma[(band + 8) * y2Stride + col] = bottomLeft.cr
                auxLuma[(band + 8) * y2Stride + col + 1] = bottomRight.cr
                // B6/B7: even-row odd-column chroma in auxiliary UV.
                uv2[half * uv2Stride + col] = topRight.cb
                uv2[half * uv2Stride + col + 1] = topRight.cr
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
        let cb: UInt8
        let cr: UInt8
    }

    /// BT.601 video-range YUV conversion. The paired views use the same matrix.
    private static func sample(_ pixel: UnsafePointer<UInt8>) -> Yuv {
        let blue = Int(pixel[0])
        let green = Int(pixel[1])
        let red = Int(pixel[2])
        func clamp(_ value: Int) -> UInt8 { UInt8(clamping: value) }
        return Yuv(
            y: clamp(((66 * red + 129 * green + 25 * blue + 128) >> 8) + 16),
            cb: clamp(((-38 * red - 74 * green + 112 * blue + 128) >> 8) + 128),
            cr: clamp(((112 * red - 94 * green - 18 * blue + 128) >> 8) + 128))
    }
}
