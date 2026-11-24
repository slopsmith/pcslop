import Foundation

public struct SavedState: Sendable, Hashable, Identifiable {
    public var folder: URL
    public var date: Date

    public var id: URL { folder }
    public var name: String { folder.lastPathComponent }
    public var thumbnail: URL { folder.appendingPathComponent(MachineStore.thumbnailFile) }
}

public struct MachineStore: Sendable {
    public let root: URL
    static let machineFile = "machine.json"
    static let thumbnailFile = "thumbnail.png"
    static let stateFile = "state.bin"

    public init(root: URL) {
        self.root = root
    }

    public static var standard: MachineStore {
        MachineStore(root: Paths.supportFolder.appendingPathComponent("machines", isDirectory: true))
    }

    public func folder(of id: Machine.ID) -> URL {
        root.appendingPathComponent(id.uuidString, isDirectory: true)
    }

    public func thumbnail(of id: Machine.ID) -> URL {
        folder(of: id).appendingPathComponent(Self.thumbnailFile)
    }

    public func statesFolder(of id: Machine.ID) -> URL {
        folder(of: id).appendingPathComponent("states", isDirectory: true)
    }

    public func loadAll() -> [Machine] {
        let fm = FileManager.default
        guard let dirs = try? fm.contentsOfDirectory(at: root, includingPropertiesForKeys: nil) else { return [] }
        let decoder = Self.decoder
        return dirs.compactMap { dir in
            guard let data = try? Data(contentsOf: dir.appendingPathComponent(Self.machineFile)) else { return nil }
            return try? decoder.decode(Machine.self, from: data)
        }
        .sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
    }

    public func save(_ machine: Machine) throws {
        let dir = folder(of: machine.id)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try Self.encoder.encode(machine).write(to: dir.appendingPathComponent(Self.machineFile), options: .atomic)
    }

    public func delete(_ id: Machine.ID) throws {
        let dir = folder(of: id)
        guard FileManager.default.fileExists(atPath: dir.path) else { return }
        try FileManager.default.removeItem(at: dir)
    }

    // pcslop writes state.bin last, so a folder without it is an unfinished or failed save.
    public func states(of id: Machine.ID) -> [SavedState] {
        let fm = FileManager.default
        let keys: [URLResourceKey] = [.contentModificationDateKey]
        guard let dirs = try? fm.contentsOfDirectory(at: statesFolder(of: id), includingPropertiesForKeys: keys) else {
            return []
        }
        return dirs.compactMap { dir in
            let file = dir.appendingPathComponent(Self.stateFile)
            guard let attrs = try? fm.attributesOfItem(atPath: file.path) else { return nil }
            return SavedState(folder: dir, date: attrs[.modificationDate] as? Date ?? .distantPast)
        }
        .sorted { $0.date > $1.date }
    }

    public func deleteState(_ state: SavedState) throws {
        try FileManager.default.removeItem(at: state.folder)
    }

    public func newStateFolder(of id: Machine.ID, date: Date = .now) -> URL {
        let base = Self.stateName.string(from: date)
        let parent = statesFolder(of: id)
        var name = base
        var n = 2
        while FileManager.default.fileExists(atPath: parent.appendingPathComponent(name).path) {
            name = "\(base) \(n)"
            n += 1
        }
        return parent.appendingPathComponent(name, isDirectory: true)
    }

    private static let stateName: DateFormatter = {
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "yyyy-MM-dd HH.mm.ss"
        return f
    }()

    static var encoder: JSONEncoder {
        let e = JSONEncoder()
        e.outputFormatting = [.prettyPrinted, .sortedKeys]
        e.dateEncodingStrategy = .iso8601
        return e
    }

    static var decoder: JSONDecoder {
        let d = JSONDecoder()
        d.dateDecodingStrategy = .iso8601
        return d
    }
}

public enum Paths {
    public static var supportFolder: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/pcslop", isDirectory: true)
    }
}
