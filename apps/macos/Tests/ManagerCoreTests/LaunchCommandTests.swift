import Foundation
import Testing

@testable import ManagerCore

struct LaunchCommandTests {
    private let firmware = Firmware(bios: "/fw/bios.bin", vgaBIOS: "/fw/vgabios.bin")

    private func args(_ config: MachineConfig, load: URL? = nil) throws -> [String] {
        try LaunchCommand.arguments(for: config, firmware: firmware, control: "/tmp/c.sock", load: load)
    }

    @Test func defaultMachineSpellsOutItsHardware() throws {
        #expect(
            try args(MachineConfig()) == [
                "--bios", "/fw/bios.bin", "--board", "fx", "--vgabios", "/fw/vgabios.bin", "--ram", "256",
                "--capture-mouse", "key", "--control", "/tmp/c.sock",
            ])
    }

    @Test func radeonForcesTheBXBoardAndSkipsTheStdvgaBIOS() throws {
        var config = MachineConfig()
        config.board = .fx
        config.gpu = .radeon
        config.vgaROM = "/roms/r300.rom"
        config.renderer = .cpu
        let argv = try args(config)
        #expect(!argv.contains("--vgabios"))
        #expect(
            argv.contains(sequence: [
                "--board", "bx", "--gpu", "radeon", "--vga-rom", "/roms/r300.rom", "--gpu-3d", "cpu",
            ]))
    }

    @Test func drivesBecomeFlags() throws {
        var config = MachineConfig()
        config.hda = "/d/xp.img"
        config.hdb = "/d/data.img"
        config.cdrom = "/d/cd.iso"
        let argv = try args(config)
        #expect(argv.contains(sequence: ["--hda", "/d/xp.img", "--hdb", "/d/data.img", "--cdrom", "/d/cd.iso"]))
    }

    @Test func emptyDrivePathsAreLeftOut() throws {
        var config = MachineConfig()
        config.hda = ""
        config.cdrom = ""
        let argv = try args(config)
        #expect(!argv.contains("--hda"))
        #expect(!argv.contains("--cdrom"))
    }

    @Test func loadingAStateLeavesTheDrivesToTheSnapshot() throws {
        var config = MachineConfig()
        config.hda = "/d/xp.img"
        config.hdb = "/d/data.img"
        config.cdrom = "/d/cd.iso"
        let argv = try args(config, load: URL(fileURLWithPath: "/m/states/one"))
        #expect(argv.contains(sequence: ["--load", "/m/states/one"]))
        #expect(!argv.contains("--hda"))
        #expect(!argv.contains("--hdb"))
        #expect(!argv.contains("--cdrom"))
    }

    @Test func hostForwardsJoinIntoOneNetSpec() throws {
        var config = MachineConfig()
        config.hostForwards = ["tcp::2222-:22", " ", "udp::5353-:53 "]
        #expect(try args(config).contains(sequence: ["--net", "user,hostfwd=tcp::2222-:22,hostfwd=udp::5353-:53"]))
    }

    @Test func switchedOffDevicesUseTheirNoFlags() throws {
        var config = MachineConfig()
        config.network = false
        config.hostForwards = ["tcp::2222-:22"]
        config.audio = false
        config.jit = false
        config.battery = .none
        let argv = try args(config)
        #expect(argv.contains("--no-net"))
        #expect(!argv.contains("--net"))
        #expect(argv.contains("--no-audio"))
        #expect(argv.contains("--no-jit"))
        #expect(argv.contains("--no-battery"))
    }

    @Test(arguments: [
        (BatteryMode.host, [String]()),
        (.fixed(percent: 40, ac: false), ["--battery", "40"]),
        (.fixed(percent: 100, ac: true), ["--battery", "100:ac"]),
    ])
    func batteryModes(mode: BatteryMode, expected: [String]) throws {
        var config = MachineConfig()
        config.battery = mode
        let argv = try args(config)
        if expected.isEmpty {
            #expect(!argv.contains("--battery"))
        } else {
            #expect(argv.contains(sequence: expected))
        }
    }

    @Test func extraArgumentsGoLastWithTheirQuotesRemoved() throws {
        var config = MachineConfig()
        config.extraArgs = #"--serial "/tmp/com 1.log" --screenshot 5:shot.png"#
        #expect(Array(try args(config).suffix(4)) == ["--serial", "/tmp/com 1.log", "--screenshot", "5:shot.png"])
    }

    @Test func unbalancedExtraArgumentsThrow() {
        var config = MachineConfig()
        config.extraArgs = "--serial 'oops"
        #expect(throws: ShellWordsError.self) { try args(config) }
    }

    @Test func socketPathsFitInSunPath() {
        let deep = URL(fileURLWithPath: "/" + String(repeating: "x", count: 120))
        let path = LaunchCommand.socketPath(in: deep, id: "0123456789abcdef")
        #expect(path.utf8.count < 104)
        #expect(path.hasSuffix("pcslop-01234567.sock"))
    }
}

extension Array where Element: Equatable {
    func contains(sequence: [Element]) -> Bool {
        guard sequence.count <= count else { return false }
        return (0...(count - sequence.count)).contains { Array(self[$0..<($0 + sequence.count)]) == sequence }
    }
}
