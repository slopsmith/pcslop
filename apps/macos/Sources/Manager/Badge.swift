import AppKit
import ManagerCore
import SwiftUI

extension OSKind {
    var tint: Color {
        switch self {
        case .windows98, .windows2000: .teal
        case .windowsXP, .windowsVista: .blue
        case .ubuntu: .orange
        case .debian: .pink
        case .fedora: .cyan
        case .linux: .yellow
        case .dos: .gray
        case .other: .indigo
        }
    }

    var symbol: String {
        switch self {
        case .dos: "apple.terminal"
        default: "pc"
        }
    }

    var logoName: String? {
        switch self {
        case .windows98, .windows2000: "windows-9x"
        case .windowsXP, .windowsVista: "windows-xp"
        case .ubuntu: "ubuntu"
        case .debian: "debian"
        case .fedora: "fedora"
        case .linux: "tux"
        case .dos, .other: nil
        }
    }

    @MainActor var logo: NSImage? { logoName.flatMap(Logos.image) }
}

@MainActor
enum Logos {
    private static var cache: [String: NSImage] = [:]

    // SwiftPM puts resources in a bundle next to the executable; bundle-app copies it into Resources.
    private static let folder: URL? = {
        let name = "pcslop-manager_Manager.bundle"
        let places = [
            Bundle.main.resourceURL, Bundle.main.bundleURL, Bundle.main.executableURL?.deletingLastPathComponent(),
        ]
        return places.compactMap { $0?.appendingPathComponent(name) }
            .flatMap { [Bundle(url: $0)?.resourceURL, $0].compactMap { $0?.appendingPathComponent("Logos") } }
            .first { FileManager.default.fileExists(atPath: $0.path) }
    }()

    static func image(_ name: String) -> NSImage? {
        if let hit = cache[name] { return hit }
        guard let url = folder?.appendingPathComponent("\(name).png"), let image = NSImage(contentsOf: url) else {
            return nil
        }
        cache[name] = image
        return image
    }
}

struct StateBadge: View {
    let running: Bool
    let paused: Bool

    var body: some View {
        if running {
            Label(paused ? "Paused" : "Running", systemImage: paused ? "pause.fill" : "play.fill")
                .font(.caption.weight(.semibold))
                .padding(.horizontal, 8)
                .padding(.vertical, 3)
                .glassEffect(.regular.tint((paused ? Color.yellow : Color.green).opacity(0.5)), in: .capsule)
        }
    }
}

// A 4:3 screen: the machine's last screenshot, or its OS symbol.
struct MachineScreen: View {
    let os: OSKind
    let image: URL
    var width: CGFloat
    @Environment(Library.self) private var library

    var body: some View {
        let _ = library.thumbnailVersion
        let picture = ThumbnailCache.image(image)
        Group {
            if let picture {
                Image(nsImage: picture)
                    .resizable()
                    .interpolation(.high)
            } else {
                ZStack {
                    Rectangle().fill(os.tint.gradient.opacity(0.25))
                    Image(systemName: os.symbol)
                        .font(.system(size: width * 0.22, weight: .light))
                        .foregroundStyle(.secondary)
                }
            }
        }
        .frame(width: width, height: width / screenAspect(picture))
        .clipShape(.rect(cornerRadius: max(6, width * 0.05), style: .continuous))
        .accessibilityHidden(true)
    }
}

@MainActor
enum ThumbnailCache {
    private static var cache: [URL: (date: Date, image: NSImage)] = [:]

    static func image(_ url: URL) -> NSImage? {
        guard let attrs = try? FileManager.default.attributesOfItem(atPath: url.path),
            let date = attrs[.modificationDate] as? Date
        else {
            cache[url] = nil
            return nil
        }
        if let hit = cache[url], hit.date == date { return hit.image }
        guard let image = NSImage(contentsOf: url), image.isValid else { return cache[url]?.image }
        cache[url] = (date, image)
        return image
    }
}

struct OSIcon: View {
    let os: OSKind
    var size: CGFloat = 36

    var body: some View {
        Group {
            if let logo = os.logo {
                Image(nsImage: logo)
                    .resizable()
                    .interpolation(.high)
                    .aspectRatio(contentMode: .fit)
                    .padding(size * 0.04)
                    .shadow(color: .black.opacity(0.2), radius: size * 0.04, y: size * 0.02)
            } else {
                Image(systemName: os.symbol)
                    .font(.system(size: size * 0.48, weight: .medium))
                    .foregroundStyle(.white)
                    .frame(width: size, height: size)
                    .background(os.tint.gradient, in: .rect(cornerRadius: size * 0.26, style: .continuous))
            }
        }
        .frame(width: size, height: size)
        .accessibilityLabel(os.label)
    }
}

extension MachineConfig {
    var summary: String {
        let board = gpu == .radeon || board == .bx ? "440BX" : "440FX"
        let display = gpu == .radeon ? "Radeon 9700" : "Standard VGA"
        return [board, display, Self.memory(ramMiB)].joined(separator: " · ")
    }

    var shortSummary: String {
        [gpu == .radeon ? "Radeon 9700" : "VGA", Self.memory(ramMiB)].joined(separator: " · ")
    }

    static func memory(_ mib: Int) -> String {
        mib >= 1024 && mib % 512 == 0
            ? "\((Double(mib) / 1024).formatted()) GB" : "\(mib) MB"
    }
}

func fileName(_ path: String?) -> String? {
    guard let path, !path.isEmpty else { return nil }
    return URL(fileURLWithPath: path).lastPathComponent
}

func fileSize(_ path: String?) -> String? {
    guard let path, !path.isEmpty,
        let size = (try? FileManager.default.attributesOfItem(atPath: path))?[.size] as? NSNumber
    else { return nil }
    return ByteCountFormatter.string(fromByteCount: size.int64Value, countStyle: .file)
}

// The guest's own screen shape once there is a screenshot, 4:3 until then.
func screenAspect(_ picture: NSImage?) -> CGFloat {
    guard let size = picture?.size, size.width > 0, size.height > 0 else { return 4 / 3 }
    return size.width / size.height
}
