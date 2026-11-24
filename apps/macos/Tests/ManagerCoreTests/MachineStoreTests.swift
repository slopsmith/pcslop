import Foundation
import Testing

@testable import ManagerCore

struct MachineStoreTests {
    private let store = MachineStore(
        root: FileManager.default.temporaryDirectory
            .appendingPathComponent("pcslop-manager-tests-\(UUID().uuidString)", isDirectory: true))

    @Test func emptyWhenNothingWasSaved() {
        #expect(store.loadAll().isEmpty)
    }

    @Test func machinesSurviveAReloadSortedByName() throws {
        defer { try? FileManager.default.removeItem(at: store.root) }
        let b = Machine(name: "beta", created: Date(timeIntervalSince1970: 1))
        let a = Machine(name: "Alpha", created: Date(timeIntervalSince1970: 2))
        try store.save(b)
        try store.save(a)
        #expect(MachineStore(root: store.root).loadAll() == [a, b])
    }

    @Test func savingAgainReplacesTheMachine() throws {
        defer { try? FileManager.default.removeItem(at: store.root) }
        var m = Machine(name: "one")
        try store.save(m)
        m.name = "two"
        try store.save(m)
        #expect(store.loadAll().map(\.name) == ["two"])
    }

    @Test func corruptMachineFilesAreSkipped() throws {
        defer { try? FileManager.default.removeItem(at: store.root) }
        let good = Machine(name: "good")
        try store.save(good)
        let bad = store.folder(of: UUID())
        try FileManager.default.createDirectory(at: bad, withIntermediateDirectories: true)
        try Data("{".utf8).write(to: bad.appendingPathComponent("machine.json"))
        #expect(store.loadAll() == [good])
    }

    @Test func deleteRemovesTheMachineAndItsStates() throws {
        defer { try? FileManager.default.removeItem(at: store.root) }
        let m = Machine(name: "gone")
        try store.save(m)
        try makeState(store.newStateFolder(of: m.id))
        try store.delete(m.id)
        #expect(store.loadAll().isEmpty)
        #expect(store.states(of: m.id).isEmpty)
    }

    @Test func statesListNewestFirstAndSkipUnfinishedSaves() throws {
        defer { try? FileManager.default.removeItem(at: store.root) }
        let id = UUID()
        let old = store.newStateFolder(of: id, date: Date(timeIntervalSince1970: 1_000))
        try makeState(old, date: Date(timeIntervalSince1970: 1_000))
        let new = store.newStateFolder(of: id, date: Date(timeIntervalSince1970: 2_000))
        try makeState(new, date: Date(timeIntervalSince1970: 2_000))
        try FileManager.default.createDirectory(
            at: store.newStateFolder(of: id), withIntermediateDirectories: true)
        #expect(store.states(of: id).map(\.folder.lastPathComponent) == [new.lastPathComponent, old.lastPathComponent])
    }

    @Test func newStateFoldersNeverReuseAName() throws {
        defer { try? FileManager.default.removeItem(at: store.root) }
        let id = UUID()
        let date = Date(timeIntervalSince1970: 5_000)
        let first = store.newStateFolder(of: id, date: date)
        try makeState(first)
        let second = store.newStateFolder(of: id, date: date)
        #expect(first != second)
        #expect(!FileManager.default.fileExists(atPath: second.path))
    }

    @Test func deleteStateRemovesItsFolder() throws {
        defer { try? FileManager.default.removeItem(at: store.root) }
        let id = UUID()
        try makeState(store.newStateFolder(of: id))
        let state = try #require(store.states(of: id).first)
        try store.deleteState(state)
        #expect(store.states(of: id).isEmpty)
    }

    private func makeState(_ folder: URL, date: Date = .now) throws {
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let file = folder.appendingPathComponent("state.bin")
        try Data("PCSLOP".utf8).write(to: file)
        try FileManager.default.setAttributes([.modificationDate: date], ofItemAtPath: file.path)
    }
}
