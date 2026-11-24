import Foundation

public enum BinaryLocator {
    // Order: the setting, the app bundle's copy, PCSLOP_BIN, then a repo build
    // above the current folder or the app.
    public static func pcslop(
        override: String = "",
        bundle: Bundle = .main,
        environment: [String: String] = ProcessInfo.processInfo.environment,
        searchFrom: [URL] = defaultSearchFolders
    ) -> URL? {
        if !override.isEmpty { return URL(fileURLWithPath: override) }
        if let url = bundle.url(forAuxiliaryExecutable: "pcslop") { return url }
        if let path = environment["PCSLOP_BIN"], !path.isEmpty {
            return URL(fileURLWithPath: path)
        }
        for start in searchFrom {
            for relative in ["target/release/pcslop", "target/vm/pcslop"] {
                if let url = findUpwards(relative, from: start, isUsable: FileManager.default.isExecutableFile) {
                    return url
                }
            }
        }
        return nil
    }

    public static func bios(bundle: Bundle = .main, searchFrom: [URL] = defaultSearchFolders) -> URL? {
        firmware("bios.bin", in: "references/seabios/share/seabios", bundle: bundle, searchFrom: searchFrom)
    }

    public static func vgaBIOS(bundle: Bundle = .main, searchFrom: [URL] = defaultSearchFolders) -> URL? {
        firmware("vgabios.bin", in: "references/seavgabios/share/seabios", bundle: bundle, searchFrom: searchFrom)
    }

    // A packaged app uses only what it carries; the search is for `swift run` in a checkout.
    public static var defaultSearchFolders: [URL] {
        if Bundle.main.bundleURL.pathExtension == "app" { return [] }
        var folders = [URL(fileURLWithPath: FileManager.default.currentDirectoryPath)]
        if let exe = Bundle.main.executableURL { folders.append(exe.deletingLastPathComponent()) }
        return folders
    }

    private static func firmware(_ name: String, in folder: String, bundle: Bundle, searchFrom: [URL]) -> URL? {
        if let url = bundle.url(forResource: name, withExtension: nil) { return url }
        for start in searchFrom {
            if let url = findUpwards("\(folder)/\(name)", from: start, isUsable: FileManager.default.fileExists) {
                return url
            }
        }
        return nil
    }

    static func findUpwards(_ relative: String, from start: URL, isUsable: (String) -> Bool) -> URL? {
        var dir = start.standardizedFileURL
        for _ in 0..<7 {
            let candidate = dir.appendingPathComponent(relative)
            if isUsable(candidate.path) { return candidate }
            let parent = dir.deletingLastPathComponent()
            if parent == dir { break }
            dir = parent
        }
        return nil
    }
}
