import Foundation

public struct Firmware: Sendable, Hashable {
    public var bios: String
    public var vgaBIOS: String?

    public init(bios: String, vgaBIOS: String? = nil) {
        self.bios = bios
        self.vgaBIOS = vgaBIOS
    }
}

public enum LaunchCommand {
    // pcslop's --load conflicts with --hda and --hdb: the snapshot brings its own disk.
    public static func arguments(
        for config: MachineConfig, firmware: Firmware, control: String, load: URL? = nil
    ) throws(ShellWordsError) -> [String] {
        var args = ["--bios", firmware.bios]
        switch config.gpu {
        case .stdvga:
            args += ["--board", config.board.rawValue]
            if let vga = firmware.vgaBIOS, !vga.isEmpty { args += ["--vgabios", vga] }
        case .radeon:
            args += ["--board", Board.bx.rawValue, "--gpu", "radeon"]
            if let rom = config.vgaROM, !rom.isEmpty { args += ["--vga-rom", rom] }
            args += ["--gpu-3d", config.renderer.rawValue]
        }
        args += ["--ram", String(config.ramMiB)]
        if let load {
            args += ["--load", load.path]
        } else {
            for (flag, path) in [("--hda", config.hda), ("--hdb", config.hdb), ("--cdrom", config.cdrom)] {
                if let path, !path.isEmpty { args += [flag, path] }
            }
        }
        if !config.network {
            args.append("--no-net")
        } else {
            let rules = config.hostForwards.map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
            if !rules.isEmpty {
                args += ["--net", (["user"] + rules.map { "hostfwd=\($0)" }).joined(separator: ",")]
            }
        }
        if !config.audio { args.append("--no-audio") }
        switch config.battery.argument {
        case nil: args.append("--no-battery")
        case "host": break
        case let value?: args += ["--battery", value]
        }
        if !config.jit { args.append("--no-jit") }
        args += ["--capture-mouse", config.captureMouse.rawValue]
        args += ["--control", control]
        args += try ShellWords.split(config.extraArgs).get()
        return args
    }

    // sun_path holds 104 bytes on macOS.
    public static func socketPath(in folder: URL = FileManager.default.temporaryDirectory, id: String) -> String {
        let name = "pcslop-\(id.prefix(8)).sock"
        let path = folder.appendingPathComponent(name).path
        return path.utf8.count < 100 ? path : "/tmp/\(name)"
    }
}
