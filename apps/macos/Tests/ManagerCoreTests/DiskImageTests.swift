import Foundation
import Testing

@testable import ManagerCore

struct DiskImageTests {
    private let url = FileManager.default.temporaryDirectory.appendingPathComponent(
        "pcslop-disk-\(UUID().uuidString).img")

    @Test func aBlankImageHasTheAskedSizeAndReadsAsZeros() throws {
        defer { try? FileManager.default.removeItem(at: url) }
        try DiskImage.createBlank(at: url, mebibytes: 3)
        let attrs = try FileManager.default.attributesOfItem(atPath: url.path)
        #expect(attrs[.size] as? UInt64 == 3 << 20)
        let handle = try FileHandle(forReadingFrom: url)
        defer { try? handle.close() }
        try handle.seek(toOffset: 2 << 20)
        #expect(try handle.read(upToCount: 512) == Data(count: 512))
    }

    @Test func zeroSizeIsRefused() {
        #expect(throws: (any Error).self) { try DiskImage.createBlank(at: url, mebibytes: 0) }
        #expect(!FileManager.default.fileExists(atPath: url.path))
    }
}
