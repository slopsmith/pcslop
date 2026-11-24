import Foundation

public enum DiskImage {
    // APFS keeps the file sparse, so a new image takes no space until the guest writes to it.
    public static func createBlank(at url: URL, mebibytes: UInt64) throws {
        guard mebibytes > 0 else { throw CocoaError(.fileWriteInvalidFileName) }
        guard FileManager.default.createFile(atPath: url.path, contents: nil) else {
            throw CocoaError(.fileWriteNoPermission)
        }
        let handle = try FileHandle(forWritingTo: url)
        defer { try? handle.close() }
        try handle.truncate(atOffset: mebibytes << 20)
    }
}
