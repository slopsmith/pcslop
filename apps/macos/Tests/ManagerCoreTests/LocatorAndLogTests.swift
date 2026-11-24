import Foundation
import Testing

@testable import ManagerCore

struct BinaryLocatorTests {
    private let empty = Bundle(for: Marker.self)

    @Test func theSettingWins() {
        let url = BinaryLocator.pcslop(
            override: "/opt/pcslop", environment: ["PCSLOP_BIN": "/env/pcslop"], searchFrom: [])
        #expect(url?.path == "/opt/pcslop")
    }

    @Test func theEnvironmentComesBeforeTheRepoSearch() {
        let url = BinaryLocator.pcslop(environment: ["PCSLOP_BIN": "/env/pcslop"], searchFrom: [])
        #expect(url?.path == "/env/pcslop")
    }

    @Test func findsARepoBuildAboveTheStartFolder() throws {
        let repo = FileManager.default.temporaryDirectory.appendingPathComponent("pcslop-repo-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: repo) }
        let bin = repo.appendingPathComponent("target/release/pcslop")
        let deep = repo.appendingPathComponent("apps/macos/.build/debug")
        try FileManager.default.createDirectory(at: bin.deletingLastPathComponent(), withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: deep, withIntermediateDirectories: true)
        try Data().write(to: bin)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: bin.path)
        let found = BinaryLocator.pcslop(bundle: empty, environment: [:], searchFrom: [deep])
        #expect(found?.standardizedFileURL.path == bin.standardizedFileURL.path)
    }

    @Test func findsTheSeaBIOSOutLink() throws {
        let repo = FileManager.default.temporaryDirectory.appendingPathComponent("pcslop-repo-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: repo) }
        let bios = repo.appendingPathComponent("references/seabios/share/seabios/bios.bin")
        try FileManager.default.createDirectory(at: bios.deletingLastPathComponent(), withIntermediateDirectories: true)
        try Data().write(to: bios)
        #expect(BinaryLocator.bios(bundle: empty, searchFrom: [repo])?.path == bios.path)
        #expect(BinaryLocator.vgaBIOS(bundle: empty, searchFrom: [repo]) == nil)
    }
}

private final class Marker {}

struct LogBufferTests {
    @Test func joinsSplitLinesAndKeepsTheLastOnes() {
        var log = LogBuffer(capacity: 3)
        log.append(Data("one\ntw".utf8))
        log.append(Data("o\nthree\nfour\nfi".utf8))
        #expect(log.lines == ["two", "three", "four"])
        log.flush()
        #expect(log.lines == ["three", "four", "fi"])
    }
}
