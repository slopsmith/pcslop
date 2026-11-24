import ManagerCore
import SwiftUI

struct ContentView: View {
    @Environment(Library.self) private var library
    @Environment(Sessions.self) private var sessions
    @Environment(EditorRequest.self) private var editor
    @State private var selection: Machine.ID?
    @State private var dropTargeted = false
    @Environment(\.openSettings) private var openSettings
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        @Bindable var editor = editor
        NavigationSplitView {
            MachineList(selection: $selection)
                .navigationSplitViewColumnWidth(min: 250, ideal: 290, max: 380)
                .toolbar {
                    ToolbarItem(placement: .primaryAction) {
                        Button("New Machine", systemImage: "plus") { editor.newMachine() }
                            .help("Make a new machine")
                    }
                }
        } detail: {
            detail
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .sheet(item: $editor.creating) { request in
            NewMachineWizard(request: request) { machine in selection = machine.id }
        }
        .sheet(item: $editor.editing) { machine in
            MachineSettings(machine: machine) { library.save($0) }
        }
        .dropDestination(for: URL.self) { urls, _ in
            guard let request = Self.request(from: urls) else { return false }
            editor.newMachine(request)
            return true
        } isTargeted: {
            dropTargeted = $0
        }
        .overlay { dropHighlight }
        .animation(.spring(duration: 0.3), value: dropTargeted)
        .alert(
            "Cannot start the machine",
            isPresented: Binding(get: { sessions.lastError != nil }, set: { if !$0 { sessions.lastError = nil } }),
            presenting: sessions.lastError
        ) { _ in
            Button("OK") {}
        } message: {
            Text($0)
        }
        .alert(
            "Something went wrong",
            isPresented: Binding(get: { library.lastError != nil }, set: { if !$0 { library.lastError = nil } }),
            presenting: library.lastError
        ) { _ in
            Button("OK") {}
        } message: {
            Text($0)
        }
        .onAppear {
            let openWindow = openWindow
            AppDelegate.showLibrary = { openWindow(id: "library") }
            applyLaunchArguments()
        }
        .onChange(of: library.machines.map(\.id)) { _, ids in
            if let id = selection, !ids.contains(id) { selection = nil }
        }
    }

    @ViewBuilder private var detail: some View {
        if let machine = library.machine(selection) {
            MachineDetail(machine: machine)
                .id(machine.id)
                .navigationTitle(machine.name)
                .navigationSubtitle(machine.config.summary)
        } else {
            WelcomeView()
                .navigationTitle("pcslop")
                .navigationSubtitle(library.machines.isEmpty ? "" : machineCount)
        }
    }

    private var machineCount: String {
        let n = library.machines.count
        return n == 1 ? "1 machine" : "\(n) machines"
    }

    @ViewBuilder private var dropHighlight: some View {
        if dropTargeted {
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .strokeBorder(Color.accentColor, style: StrokeStyle(lineWidth: 3, dash: [10, 6]))
                .background(Color.accentColor.opacity(0.08), in: .rect(cornerRadius: 16, style: .continuous))
                .overlay {
                    Label("New Machine", systemImage: "plus.circle.fill")
                        .font(.title3.weight(.semibold))
                        .padding(.horizontal, 18)
                        .padding(.vertical, 10)
                        .glassEffect(.regular.tint(.accentColor.opacity(0.4)), in: .capsule)
                }
                .padding(8)
                .allowsHitTesting(false)
                .transition(.opacity)
        }
    }

    // `--select NAME` and `--open new|settings` set up the window for Tools/screenshots.
    private func applyLaunchArguments() {
        if let name = ManagerApp.argument("--select") {
            selection = library.machines.first { $0.name == name }?.id
        }
        switch ManagerApp.argument("--open") {
        case "new": editor.newMachine()
        case "new-drives": editor.newMachine(NewMachineRequest(preset: .windowsXPRadeon))
        case "preferences": openSettings()
        case "start":
            if let machine = library.machine(selection) {
                sessions.start(machine, settings: launchSettings(), folder: library.store.folder(of: machine.id))
            }
        case "settings": if let machine = library.machine(selection) { editor.edit(machine) }
        default: break
        }
    }

    static func request(from urls: [URL]) -> NewMachineRequest? {
        let disks = urls.filter { ["img", "raw", "hdd", "bin"].contains($0.pathExtension.lowercased()) }
        let isos = urls.filter { ["iso", "cdr"].contains($0.pathExtension.lowercased()) }
        guard !disks.isEmpty || !isos.isEmpty else { return nil }
        let name = (disks.first ?? isos.first)!.deletingPathExtension().lastPathComponent
        let preset: Preset =
            name.localizedCaseInsensitiveContains("xp") || name.localizedCaseInsensitiveContains("win")
            ? .windowsXPRadeon
            : ["ubuntu", "linux", "debian", "fedora"].contains { name.localizedCaseInsensitiveContains($0) }
                ? .ubuntuRadeon : .custom
        return NewMachineRequest(preset: preset, disk: disks.first?.path, cdrom: isos.first?.path)
    }
}
