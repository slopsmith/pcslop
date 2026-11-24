import AppKit
import ManagerCore
import SwiftUI

struct MachineDetail: View {
    let machine: Machine
    @Environment(Library.self) private var library
    @Environment(Sessions.self) private var sessions
    @Environment(EditorRequest.self) private var editor
    @State private var showsOutput = false

    private var session: MachineSession? { sessions.session(for: machine.id) }
    private var running: Bool { session?.isRunning ?? false }
    private var store: MachineStore { library.store }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                ScreenPreview(machine: machine, running: running, paused: session?.isPaused ?? false, start: start)
                if let session, running {
                    RunningControls(machine: machine, session: session)
                }
                problems
                details
                SavedStates(machine: machine)
                if !machine.notes.isEmpty {
                    DetailSection("Notes") {
                        Text(machine.notes).textSelection(.enabled)
                    }
                }
                if let session {
                    output(session)
                }
            }
            .padding(24)
            .frame(maxWidth: 760)
            .frame(maxWidth: .infinity)
        }
        .toolbar { toolbar }
        .onAppear { library.refreshStates(of: machine.id) }
        .animation(.spring(duration: 0.35), value: running)
    }

    @ToolbarContentBuilder private var toolbar: some ToolbarContent {
        ToolbarItemGroup(placement: .primaryAction) {
            Button("Delete", systemImage: "trash") { confirmDelete(machine, library: library) }
                .disabled(running)
                .help("Delete this machine")
            Button("Duplicate", systemImage: "plus.square.on.square") { _ = library.duplicate(machine) }
                .help("Make a copy of this machine's settings")
            Button("Show in Finder", systemImage: "folder") {
                NSWorkspace.shared.activateFileViewerSelecting([store.folder(of: machine.id)])
            }
            .help("Show the machine's folder in Finder")
        }
        ToolbarSpacer(.fixed, placement: .primaryAction)
        ToolbarItemGroup(placement: .primaryAction) {
            if running {
                Button("Stop", systemImage: "stop.fill") {
                    session?.stop(thumbnail: store.thumbnail(of: machine.id))
                }
                .disabled(session?.busy == "Stopping")
                .help("Take a screenshot and shut the machine off")
            } else {
                Button("Start", systemImage: "play.fill", action: start)
                    .help("Start the machine")
            }
            Button("Settings", systemImage: "slider.horizontal.3") { editor.edit(machine) }
                .help(running ? "Edit the machine; changes apply on the next start" : "Edit the machine")
        }
    }

    @ViewBuilder private var problems: some View {
        let problems = machine.config.problems()
        if !problems.isEmpty {
            DetailSection("Needs Attention") {
                ForEach(problems, id: \.self) { problem in
                    Label(problem, systemImage: "exclamationmark.triangle.fill")
                        .foregroundStyle(.orange)
                }
                Button("Open Settings…") { editor.edit(machine) }
            }
        }
    }

    private var details: some View {
        let c = machine.config
        return DetailSection("Hardware") {
            VStack(spacing: 0) {
                InfoRow("cpu", "Board", c.gpu == .radeon || c.board == .bx ? "Intel 440BX with AGP" : "Intel 440FX")
                InfoRow("memorychip", "Memory", MachineConfig.memory(c.ramMiB))
                InfoRow(
                    "display", "Display",
                    c.gpu == .radeon ? "ATI Radeon 9700, \(c.renderer == .metal ? "Metal" : "CPU") 3D" : "Standard VGA")
                InfoRow("internaldrive", "Primary disk", diskText(c.hda))
                if c.hdb?.isEmpty == false { InfoRow("internaldrive", "Secondary disk", diskText(c.hdb)) }
                InfoRow("opticaldisc", "CD-ROM", fileName(session?.cdrom ?? c.cdrom) ?? "Empty")
                InfoRow(
                    "network", "Network",
                    c.network
                        ? (c.hostForwards.isEmpty ? "User mode" : "User mode, \(c.hostForwards.joined(separator: ", "))")
                        : "Off")
                InfoRow("speaker.wave.2", "Sound", c.audio ? "AC'97" : "Off", last: true)
            }
        }
    }

    private func diskText(_ path: String?) -> String {
        guard let name = fileName(path) else { return "None" }
        return fileSize(path).map { "\(name), \($0)" } ?? "\(name) (missing)"
    }

    private func output(_ session: MachineSession) -> some View {
        DisclosureGroup(isExpanded: $showsOutput) {
            LogPane(session: session)
                .frame(height: 240)
                .clipShape(.rect(cornerRadius: 12, style: .continuous))
                .padding(.top, 8)
        } label: {
            Text("Output").font(.headline)
        }
        .padding(.leading, 4)
    }

    private func start() {
        sessions.start(machine, settings: launchSettings(), folder: store.folder(of: machine.id))
    }
}

struct ScreenPreview: View {
    let machine: Machine
    let running: Bool
    let paused: Bool
    let start: () -> Void
    @Environment(Library.self) private var library
    @State private var hovering = false

    var body: some View {
        let _ = library.thumbnailVersion
        ZStack {
            if let picture = ThumbnailCache.image(library.store.thumbnail(of: machine.id)) {
                Image(nsImage: picture)
                    .resizable()
                    .interpolation(.high)
                    .aspectRatio(contentMode: .fill)
            } else {
                Rectangle().fill(machine.os.tint.gradient.opacity(0.22))
                OSIcon(os: machine.os, size: 44)
                    .padding(16)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            }
            if !running {
                Button(action: start) {
                    Image(systemName: "play.fill")
                        .font(.system(size: 30, weight: .semibold))
                        .offset(x: 2)
                        .frame(width: 76, height: 76)
                        .contentShape(.circle)
                }
                .buttonStyle(.plain)
                .glassEffect(.regular.interactive(), in: .circle)
                .scaleEffect(hovering ? 1.08 : 1)
                .help("Start \(machine.name)")
                .accessibilityLabel("Start")
            }
        }
        .aspectRatio(4 / 3, contentMode: .fit)
        .frame(maxWidth: 520)
        .clipShape(.rect(cornerRadius: 16, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .strokeBorder(.separator.opacity(0.6), lineWidth: 0.5)
        }
        .overlay(alignment: .topTrailing) {
            StateBadge(running: running, paused: paused).padding(12)
        }
        .shadow(color: .black.opacity(0.2), radius: 18, y: 8)
        .frame(maxWidth: .infinity)
        .onHover { hovering = $0 }
        .animation(.spring(duration: 0.25), value: hovering)
    }
}

struct InfoRow: View {
    let symbol: String
    let label: String
    let value: String
    var last = false

    init(_ symbol: String, _ label: String, _ value: String, last: Bool = false) {
        self.symbol = symbol
        self.label = label
        self.value = value
        self.last = last
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Image(systemName: symbol)
                    .foregroundStyle(.secondary)
                    .frame(width: 22)
                Text(label)
                    .fontWeight(.medium)
                Spacer(minLength: 16)
                Text(value)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .textSelection(.enabled)
            }
            .padding(.vertical, 8)
            if !last { Divider().padding(.leading, 32) }
        }
    }
}
