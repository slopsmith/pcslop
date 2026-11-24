import Foundation

public enum Preset: String, Sendable, Hashable, CaseIterable, Identifiable {
    case windowsXPRadeon
    case ubuntuRadeon
    case custom

    public var id: String { rawValue }

    public var os: OSKind {
        switch self {
        case .windowsXPRadeon: .windowsXP
        case .ubuntuRadeon: .ubuntu
        case .custom: .other
        }
    }

    public var needsVideoBIOS: Bool { self != .custom }

    public var diskGiB: UInt64 {
        switch self {
        case .windowsXPRadeon: 16
        case .ubuntuRadeon: 8
        case .custom: 4
        }
    }

    public func machine(name: String, disk: String?, cdrom: String?, vgaROM: String?) -> Machine {
        var config = MachineConfig()
        config.hda = disk
        config.cdrom = cdrom
        switch self {
        case .windowsXPRadeon:
            config.board = .bx
            config.gpu = .radeon
            config.vgaROM = vgaROM
            config.ramMiB = 512
            config.captureMouse = .auto
        case .ubuntuRadeon:
            config.board = .bx
            config.gpu = .radeon
            config.vgaROM = vgaROM
            config.ramMiB = 512
            config.captureMouse = .click
        case .custom:
            break
        }
        return Machine(name: name, os: os, config: config)
    }
}

public enum MemorySize {
    // The PCI hole starts at 3.5 GiB; 3 GiB leaves room for the Radeon's aperture.
    public static let range = 32...3072

    public static let steps = [32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048, 3072]

    public static func nearestStep(to mib: Int) -> Int {
        steps.indices.min { abs(steps[$0] - mib) < abs(steps[$1] - mib) } ?? 0
    }

    public static func clamped(_ mib: Int) -> Int {
        min(max(mib, range.lowerBound), range.upperBound)
    }
}

public enum DiskSize {
    public static let range: ClosedRange<UInt64> = 1...128
}
