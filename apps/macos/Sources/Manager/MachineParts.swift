import AppKit
import ManagerCore
import SwiftUI

struct RunningControls: View {
    let machine: Machine
    let session: MachineSession
    @Environment(Library.self) private var library

    var body: some View {
        DetailSection("Machine") {
            GlassEffectContainer(spacing: 8) {
                HStack(spacing: 8) {
                    control(session.isPaused ? "Resume" : "Pause", session.isPaused ? "play.fill" : "pause.fill") {
                        await session.setPaused(!session.isPaused)
                    }
                    control("Power Button", "power") { await session.perform("power") }
                    control("Ctrl+Alt+Delete", "command.square") { await session.perform("key ctrl+alt+delete") }
                    control("Update Screenshot", "camera") {
                        await session.screenshot(to: library.store.thumbnail(of: machine.id))
                        library.thumbnailsChanged()
                    }
                    control("Save State", "square.and.arrow.down") {
                        let folder = library.store.newStateFolder(of: machine.id)
                        if await session.saveState(into: folder) {
                            library.refreshStates(of: machine.id)
                            library.thumbnailsChanged()
                        }
                    }
                    Menu("More", systemImage: "ellipsis") {
                        Group {
                            Button("Change CD…", systemImage: "opticaldisc") {
                                if let iso = FilePicker.open(kinds: FileKind.cdImage) {
                                    Task { await session.changeCD(iso.path) }
                                }
                            }
                            Button("Eject CD", systemImage: "eject") { Task { await session.changeCD(nil) } }
                                .disabled(session.cdrom == nil)
                            Button("Release Mouse", systemImage: "cursorarrow") {
                                Task { await session.perform("capture off") }
                            }
                        }
                        .disabled(session.busy != nil)
                        Divider()
                        Button("Force Quit", systemImage: "xmark.octagon", role: .destructive) { session.forceQuit() }
                    }
                    .labelStyle(.iconOnly)
                    .menuIndicator(.hidden)
                    .buttonStyle(.glass)
                    .help("More actions")
                }
            }
            if let busy = session.busy {
                HStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text(busy).foregroundStyle(.secondary)
                }
            }
            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 6) {
                DetailRow("CD-ROM", session.cdrom.flatMap(fileName) ?? "Empty")
                if let status = session.status {
                    DetailRow("Speed", String(format: "%.1f MIPS", status.mips), monospaced: true)
                    DetailRow("CPU", status.halted ? "\(status.mode), halted" : status.mode, monospaced: true)
                    DetailRow("Instructions", status.instructions.formatted(), monospaced: true)
                    DetailRow(
                        "Guest time", Duration.seconds(status.uptime).formatted(.time(pattern: .hourMinuteSecond)),
                        monospaced: true)
                } else {
                    DetailRow("Status", "Waiting for the machine")
                }
            }
            if let details = session.status?.details, !details.isEmpty {
                DisclosureGroup("Status report") {
                    Text(details)
                        .font(.system(.caption2, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
        }
    }

    private func control(_ title: String, _ symbol: String, action: @escaping () async -> Void) -> some View {
        Button(title, systemImage: symbol) { Task { await action() } }
            .labelStyle(.iconOnly)
            .buttonStyle(.glass)
            .help(title)
            .disabled(session.busy != nil)
    }
}

struct SavedStates: View {
    let machine: Machine
    @Environment(Library.self) private var library
    @Environment(Sessions.self) private var sessions

    private var session: MachineSession? { sessions.session(for: machine.id) }

    var body: some View {
        let states = library.states[machine.id] ?? []
        DetailSection("Saved States") {
            if states.isEmpty {
                Text("Save the state of a running machine to start it from there later.")
                    .foregroundStyle(.secondary)
            }
            ForEach(states) { state in
                HStack(spacing: 10) {
                    MachineScreen(os: machine.os, image: state.thumbnail, width: 64)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(state.date, format: .dateTime.day().month().hour().minute())
                            .font(.callout.weight(.medium))
                        Text(state.name)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                    Spacer()
                    if let session, session.isRunning {
                        Button("Restore", systemImage: "clock.arrow.circlepath") {
                            Task { await session.restore(state) }
                        }
                        .help("Put the running machine back in this state")
                        .disabled(session.busy != nil)
                    } else {
                        Button("Start", systemImage: "play.fill") {
                            sessions.start(
                                machine, settings: launchSettings(), folder: library.store.folder(of: machine.id),
                                load: state)
                        }
                        .help("Start the machine from this state")
                    }
                    Button("Delete", systemImage: "trash") { confirmDelete(state) }
                        .help("Delete this state")
                }
                .labelStyle(.iconOnly)
                .buttonStyle(.borderless)
                .contextMenu {
                    Button("Show in Finder", systemImage: "folder") {
                        NSWorkspace.shared.activateFileViewerSelecting([state.folder])
                    }
                }
            }
        }
    }

    private func confirmDelete(_ state: SavedState) {
        let alert = NSAlert()
        alert.messageText = "Delete this saved state?"
        alert.informativeText = "\(state.name) and the disk copy saved with it are deleted."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Delete")
        alert.addButton(withTitle: "Cancel")
        alert.buttons.first?.hasDestructiveAction = true
        if alert.runModal() == .alertFirstButtonReturn { library.delete(state, of: machine.id) }
    }
}

struct DetailSection<Content: View>: View {
    let title: String
    @ViewBuilder let content: Content

    init(_ title: String, @ViewBuilder content: () -> Content) {
        self.title = title
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title)
                .font(.headline)
                .padding(.leading, 4)
            VStack(alignment: .leading, spacing: 10) {
                content.font(.callout)
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(.background.secondary, in: .rect(cornerRadius: 14, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: 14, style: .continuous)
                    .strokeBorder(.separator.opacity(0.5), lineWidth: 0.5)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

struct DetailRow: View {
    let label: String
    let value: String?
    var monospaced = false

    init(_ label: String, _ value: String?, monospaced: Bool = false) {
        self.label = label
        self.value = value
        self.monospaced = monospaced
    }

    var body: some View {
        GridRow {
            Text(label)
                .foregroundStyle(.secondary)
                .gridColumnAlignment(.trailing)
            Text(value ?? "None")
                .font(monospaced ? .callout.monospaced() : .callout)
                .foregroundStyle(value == nil ? .secondary : .primary)
                .lineLimit(3)
                .truncationMode(.middle)
                .textSelection(.enabled)
        }
    }
}

struct LogPane: View {
    let session: MachineSession

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Label("Output", systemImage: "text.alignleft")
                    .font(.subheadline.weight(.semibold))
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Copy Output", systemImage: "doc.on.doc") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(session.log.lines.joined(separator: "\n"), forType: .string)
                }
                Button("Clear Output", systemImage: "trash") { session.clearLog() }
            }
            .labelStyle(.iconOnly)
            .buttonStyle(.borderless)
            .padding(.horizontal, 14)
            .padding(.vertical, 6)
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 1) {
                        ForEach(Array(session.log.lines.enumerated()), id: \.offset) { item in
                            Text(item.element)
                                .font(.system(.caption, design: .monospaced))
                                .textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .id(item.offset)
                        }
                    }
                    .padding(10)
                }
                .onChange(of: session.log.lines.count) { _, count in
                    proxy.scrollTo(count - 1, anchor: .bottom)
                }
            }
            .background(.background.secondary)
        }
    }
}
