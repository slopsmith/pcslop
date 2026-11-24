import Foundation
import Testing

@testable import ManagerCore

struct MachineTests {
    @Test func everySettingSurvivesAJSONRoundTrip() throws {
        var config = MachineConfig()
        config.board = .bx
        config.gpu = .radeon
        config.renderer = .cpu
        config.ramMiB = 768
        config.hda = "/a.img"
        config.hdb = "/b.img"
        config.cdrom = "/c.iso"
        config.network = false
        config.hostForwards = ["tcp::80-:80"]
        config.audio = false
        config.battery = .fixed(percent: 55, ac: true)
        config.jit = false
        config.captureMouse = .auto
        config.extraArgs = "--serial -"
        let machine = Machine(
            name: "XP", os: .windowsXP, notes: "SP3", config: config,
            created: Date(timeIntervalSince1970: 1_700_000_000))
        let data = try MachineStore.encoder.encode(machine)
        #expect(try MachineStore.decoder.decode(Machine.self, from: data) == machine)
    }

    @Test func missingKeysTakeTheDefaults() throws {
        let json = #"{"id": "6F9619FF-8B86-D011-B42D-00C04FC964FF", "name": "Old", "config": {"ramMiB": 64}}"#
        let machine = try MachineStore.decoder.decode(Machine.self, from: Data(json.utf8))
        var expected = MachineConfig()
        expected.ramMiB = 64
        #expect(machine.config == expected)
        #expect(machine.os == .other)
    }

    @Test func unknownOSKindsReadAsOther() throws {
        let json = #"{"id": "6F9619FF-8B86-D011-B42D-00C04FC964FF", "name": "M", "os": "beos"}"#
        #expect(try MachineStore.decoder.decode(Machine.self, from: Data(json.utf8)).os == .other)
    }

    @Test func theRadeonWithoutAVideoBIOSIsAProblem() {
        var config = MachineConfig()
        config.gpu = .radeon
        #expect(config.problems(radeonROM: nil, fileExists: { _ in true }).count == 1)
        #expect(config.problems(radeonROM: "", fileExists: { _ in true }).count == 1)
    }

    @Test func theRadeonWithAVideoBIOSHasNoProblems() {
        var config = MachineConfig()
        config.gpu = .radeon
        #expect(config.problems(radeonROM: "/r.rom", fileExists: { _ in true }).isEmpty)
    }

    @Test func aMissingVideoBIOSFileIsAProblem() {
        var config = MachineConfig()
        config.gpu = .radeon
        #expect(config.problems(radeonROM: "/gone.rom", fileExists: { _ in false }).count == 1)
    }

    @Test func standardVGAIgnoresTheRadeonVideoBIOS() {
        #expect(MachineConfig().problems(radeonROM: "/gone.rom", fileExists: { _ in false }).isEmpty)
    }

    @Test func missingDriveFilesAreProblems() {
        var config = MachineConfig()
        config.hda = "/gone.img"
        config.cdrom = "/here.iso"
        let problems = config.problems(radeonROM: nil, fileExists: { $0 == "/here.iso" })
        #expect(problems.count == 1)
    }

    @Test func aDefaultMachineHasNoProblems() {
        #expect(MachineConfig().problems(radeonROM: nil, fileExists: { _ in false }).isEmpty)
    }
}

struct ShellWordsTests {
    @Test(arguments: [
        ("", [String]()),
        ("  a  b ", ["a", "b"]),
        (#"'a b' "c d""#, ["a b", "c d"]),
        (#"a\ b"#, ["a b"]),
        (#""a\"b""#, [#"a"b"#]),
        ("''", [""]),
        (#"x'y'"z""#, ["xyz"]),
    ])
    func splits(text: String, words: [String]) throws {
        #expect(try ShellWords.split(text).get() == words)
    }

    @Test(arguments: ["'open", #""open"#, #"end\"#])
    func rejectsUnfinishedText(text: String) {
        #expect(throws: ShellWordsError.self) { try ShellWords.split(text).get() }
    }
}
