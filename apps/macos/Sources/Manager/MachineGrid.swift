import ManagerCore
import SwiftUI

struct MachineGrid: View {
    let machines: [Machine]
    @Binding var selection: Machine.ID?

    var body: some View {
        ScrollView {
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 220, maximum: 280), spacing: 22)], spacing: 26) {
                ForEach(machines) { machine in
                    MachineCard(machine: machine, selected: selection == machine.id) { selection = machine.id }
                }
            }
            .padding(.horizontal, 24)
            .padding(.vertical, 20)
        }
    }
}

struct MachineCard: View {
    let machine: Machine
    let selected: Bool
    let select: () -> Void

    @Environment(Library.self) private var library
    @Environment(Sessions.self) private var sessions
    @Environment(EditorRequest.self) private var editor
    @State private var hovering = false
    @FocusState private var focused: Bool
    @ScaledMetric(relativeTo: .body) private var width: CGFloat = 208

    private var session: MachineSession? { sessions.session(for: machine.id) }
    private var running: Bool { session?.isRunning ?? false }

    var body: some View {
        VStack(spacing: 8) {
            screen
            Text(machine.name)
                .font(.body.weight(.medium))
                .lineLimit(1)
            HStack(spacing: 6) {
                OSBadge(os: machine.os)
                Text(summary)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .frame(maxWidth: .infinity)
        .contentShape(.rect)
        .onTapGesture(count: 2) { toggle() }
        .simultaneousGesture(TapGesture().onEnded(select))
        .onKeyPress(.return) {
            toggle()
            return .handled
        }
        .contextMenu { MachineMenu(machine: machine) }
        .focusable()
        .focused($focused)
        .focusEffectDisabled()
        .onHover { hovering = $0 }
        .onChange(of: focused) { _, isFocused in
            if isFocused { select() }
        }
        .animation(.spring(duration: 0.3, bounce: 0.25), value: hovering)
        .animation(.spring(duration: 0.25), value: selected)
        .animation(.spring(duration: 0.35), value: running)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(machine.name), \(machine.os.label), \(summary)\(running ? ", running" : "")")
        .accessibilityAddTraits(selected ? [.isButton, .isSelected] : [.isButton])
        .accessibilityAction(named: running ? "Stop" : "Start") { toggle() }
        .accessibilityAction(.default) { select() }
    }

    private var screen: some View {
        MachineScreen(os: machine.os, image: library.store.thumbnail(of: machine.id), width: width)
            .overlay {
                RoundedRectangle(cornerRadius: width * 0.05, style: .continuous)
                    .strokeBorder(Color.primary.opacity(0.08), lineWidth: 1)
            }
            .overlay(alignment: .topTrailing) {
                StateBadge(running: running, paused: session?.isPaused ?? false)
                    .padding(8)
                    .transition(.scale.combined(with: .opacity))
            }
            .shadow(color: .black.opacity(hovering ? 0.28 : 0.18), radius: hovering ? 14 : 8, y: hovering ? 8 : 4)
            .padding(5)
            .overlay {
                RoundedRectangle(cornerRadius: width * 0.05 + 5, style: .continuous)
                    .strokeBorder(Color.accentColor, lineWidth: 3)
                    .opacity(selected || focused ? 1 : 0)
            }
            .scaleEffect(hovering ? 1.02 : 1)
            .offset(y: hovering ? -3 : 0)
    }

    private var summary: String {
        let c = machine.config
        let ram = c.ramMiB >= 1024 && c.ramMiB % 1024 == 0 ? "\(c.ramMiB / 1024) GiB" : "\(c.ramMiB) MiB"
        return "\(ram), \(c.gpu == .radeon ? "Radeon 9700" : "Standard VGA")"
    }

    private func toggle() {
        if running {
            session?.stop(thumbnail: library.store.thumbnail(of: machine.id))
        } else {
            sessions.start(machine, settings: launchSettings(), folder: library.store.folder(of: machine.id))
        }
    }
}

struct MachineMenu: View {
    let machine: Machine
    @Environment(Library.self) private var library
    @Environment(Sessions.self) private var sessions
    @Environment(EditorRequest.self) private var editor

    var body: some View {
        if sessions.isRunning(machine.id) {
            Button("Stop", systemImage: "stop.fill") {
                sessions.session(for: machine.id)?.stop(thumbnail: library.store.thumbnail(of: machine.id))
            }
        } else {
            Button("Start", systemImage: "play.fill") {
                sessions.start(machine, settings: launchSettings(), folder: library.store.folder(of: machine.id))
            }
        }
        Divider()
        Button("Edit…", systemImage: "slider.horizontal.3") { editor.edit(machine) }
        Button("Duplicate", systemImage: "plus.square.on.square") { _ = library.duplicate(machine) }
        Button("Show in Finder", systemImage: "folder") {
            NSWorkspace.shared.activateFileViewerSelecting([library.store.folder(of: machine.id)])
        }
        Divider()
        Button("Delete…", systemImage: "trash", role: .destructive) { confirmDelete() }
            .disabled(sessions.isRunning(machine.id))
    }

    private func confirmDelete() {
        let alert = NSAlert()
        alert.messageText = "Delete \(machine.name)?"
        alert.informativeText =
            "This deletes its settings, screenshot and save states. Disk images and CD images stay where they are."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Delete")
        alert.addButton(withTitle: "Cancel")
        alert.buttons.first?.hasDestructiveAction = true
        if alert.runModal() == .alertFirstButtonReturn { library.delete(machine.id) }
    }
}
