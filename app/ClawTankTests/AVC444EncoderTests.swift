import CoreMedia
import CoreVideo
import VideoToolbox
import XCTest
@testable import ClawTank

final class AVC444EncoderTests: XCTestCase {
    private func makeBGRA(width: Int = 32, height: Int = 16) -> CVPixelBuffer {
        var buffer: CVPixelBuffer?
        let attrs: [CFString: Any] = [kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary]
        XCTAssertEqual(CVPixelBufferCreate(
            nil, width, height, kCVPixelFormatType_32BGRA, attrs as CFDictionary, &buffer),
            kCVReturnSuccess)
        let result = buffer!
        CVPixelBufferLockBaseAddress(result, [])
        defer { CVPixelBufferUnlockBaseAddress(result, []) }
        let bytes = CVPixelBufferGetBaseAddress(result)!.assumingMemoryBound(to: UInt8.self)
        let stride = CVPixelBufferGetBytesPerRow(result)
        for row in 0..<height {
            for col in 0..<width {
                let offset = row * stride + col * 4
                bytes[offset] = UInt8((col * 9) % 256)
                bytes[offset + 1] = UInt8((row * 17) % 256)
                bytes[offset + 2] = UInt8(((col + row) * 7) % 256)
                bytes[offset + 3] = 255
            }
        }
        return result
    }

    func testTwoPlanesRepresentMissingChroma() throws {
        let source = makeBGRA()
        let (main, aux) = try XCTUnwrap(AVC444Encoder.splitBGRA(source))
        XCTAssertEqual(CVPixelBufferGetPixelFormatType(main),
                       kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
        XCTAssertEqual(CVPixelBufferGetPixelFormatType(aux),
                       kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
        CVPixelBufferLockBaseAddress(main, .readOnly)
        CVPixelBufferLockBaseAddress(aux, .readOnly)
        defer {
            CVPixelBufferUnlockBaseAddress(main, .readOnly)
            CVPixelBufferUnlockBaseAddress(aux, .readOnly)
        }
        let yMain = CVPixelBufferGetBaseAddressOfPlane(main, 0)!.assumingMemoryBound(to: UInt8.self)
        let yAux = CVPixelBufferGetBaseAddressOfPlane(aux, 0)!.assumingMemoryBound(to: UInt8.self)
        let uvAux = CVPixelBufferGetBaseAddressOfPlane(aux, 1)!.assumingMemoryBound(to: UInt8.self)
        let yAuxStride = CVPixelBufferGetBytesPerRowOfPlane(aux, 0)
        XCTAssertNotEqual(yMain[0], yMain[1], "adjacent pixels need separate luma")
        XCTAssertNotEqual(yAux[0], yAux[yAuxStride * 8], "odd row U/V must occupy different bands")
        XCTAssertNotEqual(uvAux[0], uvAux[1], "auxiliary UV carries even-row odd-column values")
    }

    func testAVC444ProducesTwoIndependentH264AccessUnits() throws {
        var settings = H264Encoder.Settings(width: 640, height: 368, frameRate: 30,
                                            averageBitRate: 2_000_000, requireHardware: false)
        settings.keyframeIntervalSeconds = 1
        let dual = try AVC444Encoder(settings: settings)
        let frame = try XCTUnwrap(dual.encode(
            makeBGRA(width: 640, height: 368), presentationTime: .zero, forceKeyframe: true))
        XCTAssertTrue(frame.isKeyframe)
        XCTAssertTrue(frame.luma.starts(with: [0, 0, 0, 1]))
        XCTAssertTrue(frame.chroma.starts(with: [0, 0, 0, 1]))
        XCTAssertGreaterThan(frame.luma.count, 100)
        XCTAssertGreaterThan(frame.chroma.count, 100)
        dual.flush()
    }

    func testRejectUnsupportedSourcePixelFormat() {
        let frame = SyntheticFrames.make(width: 32, height: 16, index: 0)
        XCTAssertNil(AVC444Encoder.splitBGRA(frame))
    }
}
