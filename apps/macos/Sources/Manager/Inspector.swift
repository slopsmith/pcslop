import AppKit
import ManagerCore
import SwiftUI

struct MachineInspector: View {
    let machine: Machine
    @Environment(Library.self) private var library
    @Environment(Sessions.self) private var sessions
    @Environment(EditorRequest.self) private var editor
    @Namespace private var glass

    private var session: MachineSession? { sessions.session(for: machine.id) }
    private var running: Bool { session?.isRunning ?? false }
    private var store: MachineStore { library.store }

    var body: some View {
        VStack(spacing: 0) {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    header
                    actions
                    if let session, running {
                        RunningControls(machine: machine, session: session)
                    }
                    problems
                    hardware
                    SavedStates(machine: machine)
                    if !machine.notes.isEmpty {
                        InspectorSection("Notes") {
                            Text(machine.notes).textSelection(.enabled)
                        }
                    }
                }
                .padding(.horizontal, 18)
                .padding(.bottom, 18)
            }
            if let session {
                Divider()
                LogPane(session: session)
                    .frame(minHeight: 160, idealHeight: 220)
                    .transition(.move(edge: .bottom).combined(with: .opacity))
            }
        }
        .animation(.spring(duration: 0.35), value: session != nil)
        .onAppear { library.refreshStates(of: machine.id) }
    }

    private var header: some View {
        VStack(spacing: 12) {
            MachineScreen(os: machine.os, image: store.thumbnail(of: machine.id), width: 280)
                .shadow(color: .black.opacity(0.25), radius: 16, y: 8)
                .overlay(alignment: .topTrailing) {
                    StateBadge(running: running, paused: session?.isPaused ?? false).padding(10)
                }
                .padding(.top, 20)
            VStack(spacing: 6) {
                Text(machine.name)
                    .font(.title2.weight(.semibold))
                    .multilineTextAlignment(.center)
                OSBadge(os: machine.os, showsSymbol: true)
            }
        }
        .frame(maxWidth: .infinity)
    }

    private var actions: some View {
        GlassEffectContainer(spacing: 12) {
            HStack(spacing: 12) {
                if running {
                    Button("Stop", systemImage: "stop.fill") {
                        session?.stop(thumbnail: store.thumbnail(of: machine.id))
                    }
                    .buttonStyle(.glass)
                    .controlSize(.extraLarge)
                    .frame(maxWidth: .infinity)
                    .disabled(session?.busy == "Stopping")
                    .glassEffectID("primary", in: glass)
                    .help("Take a screenshot for the library and shut the machine off")
                    if let started = session?.startedAt {
                        Label {
                            Text(started, style: .timer).monospacedDigit()
                        } icon: {
                            Image(systemName: "clock")
                        }
                        .font(.callout)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 8)
                        .glassEffect(.regular, in: .capsule)
                        .glassEffectID("timer", in: glass)
                        .accessibilityLabel("Running time")
                    }
                } else {
                    Button("Start", systemImage: "play.fill") {
                        sessions.start(machine, settings: launchSettings(), folder: store.folder(of: machine.id))
                    }
                    .buttonStyle(.glassProminent)
                    .controlSize(.extraLarge)
                    .frame(maxWidth: .infinity)
                    .glassEffectID("primary", in: glass)
                }
                Button("Edit", systemImage: "slider.horizontal.3") { editor.edit(machine) }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.glass)
                    .controlSize(.extraLarge)
                    .help(running ? "Edit the machine; changes apply on the next start" : "Edit the machine")
                    .glassEffectID("edit", in: glass)
                Button("Show in Finder", systemImage: "folder") {
                    NSWorkspace.shared.activateFileViewerSelecting([store.folder(of: machine.id)])
                }
                .labelStyle(.iconOnly)
                .buttonStyle(.glass)
                .controlSize(.extraLarge)
                .help("Show the machine's folder in Finder")
                .glassEffectID("finder", in: glass)
            }
        }
        .animation(.spring(duration: 0.4, bounce: 0.2), value: running)
    }

    @ViewBuilder private var problems: some View {
        let problems = machine.config.problems()
        if !problems.isEmpty {
            InspectorSection("Problems") {
                ForEach(problems, id: \.self) { problem in
                    Label(problem, systemImage: "exclamationmark.triangle.fill")
                        .foregroundStyle(.orange)
                }
            }
        }
    }

    private var hardware: some View {
        let c = machine.config
        return InspectorSection("Hardware") {
            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 6) {
                DetailRow("Board", c.gpu == .radeon || c.board == .bx ? "440BX (AGP)" : "440FX")
                DetailRow("Memory", "\(c.ramMiB) MiB")
                DetailRow(
                    "Display",
                    c.gpu == .radeon ? "Radeon 9700, \(c.renderer == .metal ? "Metal" : "CPU") 3D" : "Standard VGA")
                DetailRow("Primary disk", c.hda.flatMap(Self.fileName))
                if c.hdb?.isEmpty == false { DetailRow("Secondary disk", c.hdb.flatMap(Self.fileName)) }
                DetailRow("CD-ROM", c.cdrom.flatMap(Self.fileName) ?? "Empty")
                DetailRow(
                    "Network",
                    c.network ? (c.hostForwards.isEmpty ? "User mode" : c.hostForwards.joined(separator: ", ")) : "Off")
                DetailRow("Sound", c.audio ? "AC'97" : "Off")
                DetailRow("Battery", Self.describe(c.battery))
                DetailRow("CPU", c.jit ? "JIT" : "Interpreter")
                DetailRow("Mouse capture", Self.describe(c.captureMouse))
                if !c.extraArgs.isEmpty { DetailRow("Extra arguments", c.extraArgs, monospaced: true) }
            }
        }
    }

    static func fileName(_ path: String) -> String? {
        path.isEmpty ? nil : URL(fileURLWithPath: path).lastPathComponent
    }

    static func describe(_ battery: BatteryMode) -> String {
        switch battery {
        case .host: "Like this Mac"
        case .none: "None"
        case .fixed(let percent, let ac): "\(percent)%\(ac ? ", on AC" : "")"
        }
    }

    static func describe(_ mode: CaptureMode) -> String {
        switch mode {
        case .key: "Ctrl+Option+G only"
        case .click: "Click in the window"
        case .auto: "Click, until the tablet driver loads"
        }
    }
}

struct RunningControls: View {
    let machine: Machine
    let session: MachineSession
    @Environment(Library.self) private var library

    var body: some View {
        InspectorSection("Machine") {
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
                DetailRow("CD-ROM", session.cdrom.flatMap(MachineInspector.fileName) ?? "Empty")
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
        InspectorSection("Saved States") {
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

struct InspectorSection<Content: View>: View {
    let title: String
    @ViewBuilder let content: Content

    init(_ title: String, @ViewBuilder content: () -> Content) {
        self.title = title
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.headline)
            content.font(.callout)
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
