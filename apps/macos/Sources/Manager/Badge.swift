import AppKit
import ManagerCore
import SwiftUI

extension OSKind {
    var tint: Color {
        switch self {
        case .windowsXP: .blue
        case .linux: .orange
        case .other: .gray
        }
    }

    var symbol: String {
        switch self {
        case .windowsXP: "macwindow"
        case .linux: "terminal"
        case .other: "desktopcomputer"
        }
    }
}

struct OSBadge: View {
    let os: OSKind
    var showsSymbol = false

    var body: some View {
        HStack(spacing: 4) {
            if showsSymbol { Image(systemName: os.symbol) }
            Text(os.label)
        }
        .font(.caption.weight(.semibold))
        .foregroundStyle(.white)
        .padding(.horizontal, 8)
        .padding(.vertical, 3)
        .background(os.tint.gradient, in: .capsule)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(os.label)
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
        Group {
            if let picture = ThumbnailCache.image(image) {
                Image(nsImage: picture)
                    .resizable()
                    .interpolation(.high)
                    .aspectRatio(contentMode: .fill)
            } else {
                ZStack {
                    Rectangle().fill(os.tint.gradient.opacity(0.25))
                    Image(systemName: os.symbol)
                        .font(.system(size: width * 0.22, weight: .light))
                        .foregroundStyle(.secondary)
                }
            }
        }
        .frame(width: width, height: width * 0.75)
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
        guard let image = NSImage(contentsOf: url) else { return nil }
        cache[url] = (date, image)
        return image
    }
}
