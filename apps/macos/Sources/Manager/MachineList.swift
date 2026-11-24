import AppKit
import ManagerCore
import SwiftUI

struct MachineList: View {
    @Binding var selection: Machine.ID?
    @Environment(Library.self) private var library
    @Environment(Sessions.self) private var sessions
    @Environment(EditorRequest.self) private var editor

    var body: some View {
        List(selection: $selection) {
            ForEach(library.machines) { machine in
                MachineRow(machine: machine)
                    .tag(machine.id)
            }
        }
        .contextMenu(forSelectionType: Machine.ID.self) { ids in
            if let machine = library.machine(ids.first) {
                MachineMenu(machine: machine)
            }
        } primaryAction: { ids in
            guard let machine = library.machine(ids.first), !sessions.isRunning(machine.id) else { return }
            sessions.start(machine, settings: launchSettings(), folder: library.store.folder(of: machine.id))
        }
        .overlay {
            if library.machines.isEmpty {
                VStack(spacing: 6) {
                    Text("No Machines").font(.headline)
                    Text("Make one with the + button.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
                .multilineTextAlignment(.center)
                .padding()
            }
        }
    }
}

struct MachineRow: View {
    let machine: Machine
    @Environment(Library.self) private var library
    @Environment(Sessions.self) private var sessions

    private var session: MachineSession? { sessions.session(for: machine.id) }
    private var running: Bool { session?.isRunning ?? false }

    var body: some View {
        HStack(spacing: 12) {
            OSIcon(os: machine.os, size: 38)
                .overlay(alignment: .bottomTrailing) {
                    if running {
                        Circle()
                            .fill(session?.isPaused == true ? Color.yellow : Color.green)
                            .frame(width: 11, height: 11)
                            .overlay { Circle().strokeBorder(.background, lineWidth: 2) }
                            .offset(x: 3, y: 3)
                            .transition(.scale.combined(with: .opacity))
                    }
                }
            VStack(alignment: .leading, spacing: 2) {
                Text(machine.name)
                    .font(.body.weight(.semibold))
                    .lineLimit(1)
                Text(running ? (session?.isPaused == true ? "Paused" : "Running") : machine.config.shortSummary)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            Spacer(minLength: 4)
            Button(running ? "Stop" : "Start", systemImage: running ? "stop.fill" : "play.fill", action: toggle)
                .labelStyle(.iconOnly)
                .buttonStyle(.glass)
                .buttonBorderShape(.circle)
                .controlSize(.large)
                .help(running ? "Shut \(machine.name) off" : "Start \(machine.name)")
                .contentTransition(.symbolEffect(.replace))
        }
        .padding(.vertical, 6)
        .animation(.spring(duration: 0.3), value: running)
        .accessibilityElement(children: .combine)
        .accessibilityAction(named: running ? "Stop" : "Start", toggle)
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
        Button("Delete…", systemImage: "trash", role: .destructive) { confirmDelete(machine, library: library) }
            .disabled(sessions.isRunning(machine.id))
    }
}

@MainActor
func confirmDelete(_ machine: Machine, library: Library) {
    let alert = NSAlert()
    alert.messageText = "Delete \(machine.name)?"
    alert.informativeText =
        "This deletes its settings, screenshot, save states and any disk made for it in its folder. Disk and CD images chosen from elsewhere stay where they are."
    alert.alertStyle = .warning
    alert.addButton(withTitle: "Delete")
    alert.addButton(withTitle: "Cancel")
    alert.buttons.first?.hasDestructiveAction = true
    if alert.runModal() == .alertFirstButtonReturn { library.delete(machine.id) }
}
