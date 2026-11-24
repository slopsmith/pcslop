import Foundation

public enum OSKind: String, Sendable, Hashable, Codable, CaseIterable {
    case windows98 = "windows-98"
    case windows2000 = "windows-2000"
    case windowsXP = "windows-xp"
    case windowsVista = "windows-vista"
    case ubuntu
    case debian
    case fedora
    case linux
    case dos
    case other

    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = OSKind(rawValue: raw) ?? .other
    }

    public var label: String {
        switch self {
        case .windows98: "Windows 98"
        case .windows2000: "Windows 2000"
        case .windowsXP: "Windows XP"
        case .windowsVista: "Windows Vista"
        case .ubuntu: "Ubuntu"
        case .debian: "Debian"
        case .fedora: "Fedora"
        case .linux: "Linux"
        case .dos: "DOS"
        case .other: "Other"
        }
    }
}

public enum Board: String, Sendable, Hashable, Codable, CaseIterable {
    case fx, bx
}

public enum GPU: String, Sendable, Hashable, Codable, CaseIterable {
    case stdvga, radeon
}

public enum Renderer: String, Sendable, Hashable, Codable, CaseIterable {
    case metal, cpu
}

public enum CaptureMode: String, Sendable, Hashable, Codable, CaseIterable {
    case key, click, auto
}

public enum BatteryMode: Sendable, Hashable, Codable {
    case host
    case none
    case fixed(percent: Int, ac: Bool)

    public var argument: String? {
        switch self {
        case .host: "host"
        case .none: nil
        case .fixed(let percent, let ac): ac ? "\(percent):ac" : "\(percent)"
        }
    }
}

public struct MachineConfig: Sendable, Hashable, Codable {
    public var board: Board = .fx
    public var gpu: GPU = .stdvga
    public var renderer: Renderer = .metal
    public var ramMiB: Int = 256
    public var hda: String?
    public var hdb: String?
    public var cdrom: String?
    public var network = true
    public var hostForwards: [String] = []
    public var audio = true
    public var battery: BatteryMode = .host
    public var jit = true
    public var captureMouse: CaptureMode = .key
    public var extraArgs = ""

    public init() {}

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let d = MachineConfig()
        board = try c.decodeIfPresent(Board.self, forKey: .board) ?? d.board
        gpu = try c.decodeIfPresent(GPU.self, forKey: .gpu) ?? d.gpu
        renderer = try c.decodeIfPresent(Renderer.self, forKey: .renderer) ?? d.renderer
        ramMiB = try c.decodeIfPresent(Int.self, forKey: .ramMiB) ?? d.ramMiB
        hda = try c.decodeIfPresent(String.self, forKey: .hda)
        hdb = try c.decodeIfPresent(String.self, forKey: .hdb)
        cdrom = try c.decodeIfPresent(String.self, forKey: .cdrom)
        network = try c.decodeIfPresent(Bool.self, forKey: .network) ?? d.network
        hostForwards = try c.decodeIfPresent([String].self, forKey: .hostForwards) ?? d.hostForwards
        audio = try c.decodeIfPresent(Bool.self, forKey: .audio) ?? d.audio
        battery = try c.decodeIfPresent(BatteryMode.self, forKey: .battery) ?? d.battery
        jit = try c.decodeIfPresent(Bool.self, forKey: .jit) ?? d.jit
        captureMouse = try c.decodeIfPresent(CaptureMode.self, forKey: .captureMouse) ?? d.captureMouse
        extraArgs = try c.decodeIfPresent(String.self, forKey: .extraArgs) ?? d.extraArgs
    }

    public func problems(
        radeonROM: String?, fileExists: (String) -> Bool = { FileManager.default.fileExists(atPath: $0) }
    ) -> [String] {
        var out: [String] = []
        if gpu == .radeon, radeonROM?.isEmpty ?? true {
            out.append("The Radeon 9700 needs its video BIOS. Choose it in Settings, or switch to standard VGA.")
        }
        if ramMiB < 4 { out.append("The machine needs at least 4 MiB of RAM.") }
        let files: [(String, String?)] = [
            ("Radeon video BIOS", gpu == .radeon ? radeonROM : nil), ("primary disk", hda),
            ("secondary disk", hdb), ("CD-ROM image", cdrom),
        ]
        for (what, path) in files {
            if let path, !path.isEmpty, !fileExists(path) {
                out.append("The \(what) \(path) is missing.")
            }
        }
        if case .failure(let error) = ShellWords.split(extraArgs) {
            out.append("Extra arguments: \(error.message)")
        }
        return out
    }
}

public struct Machine: Sendable, Hashable, Codable, Identifiable {
    public var id: UUID
    public var name: String
    public var os: OSKind
    public var notes: String
    public var config: MachineConfig
    public var created: Date

    public init(
        id: UUID = UUID(), name: String, os: OSKind = .other, notes: String = "",
        config: MachineConfig = MachineConfig(), created: Date = .now
    ) {
        self.id = id
        self.name = name
        self.os = os
        self.notes = notes
        self.config = config
        // The JSON keeps whole seconds only.
        self.created = Date(timeIntervalSince1970: created.timeIntervalSince1970.rounded(.down))
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(UUID.self, forKey: .id)
        name = try c.decode(String.self, forKey: .name)
        os = try c.decodeIfPresent(OSKind.self, forKey: .os) ?? .other
        notes = try c.decodeIfPresent(String.self, forKey: .notes) ?? ""
        config = try c.decodeIfPresent(MachineConfig.self, forKey: .config) ?? MachineConfig()
        created = try c.decodeIfPresent(Date.self, forKey: .created) ?? .distantPast
    }

    public func matches(_ query: String) -> Bool {
        let q = query.trimmingCharacters(in: .whitespaces)
        return q.isEmpty || name.localizedCaseInsensitiveContains(q) || os.label.localizedCaseInsensitiveContains(q)
    }
}
