import ManagerCore
import SwiftUI

enum SidebarItem: Hashable {
    case all
    case running
    case os(OSKind)
}

struct ContentView: View {
    @Environment(Library.self) private var library
    @Environment(Sessions.self) private var sessions
    @Environment(EditorRequest.self) private var editor
    @AppStorage(Defaults.showInspector) private var showInspector = true
    @State private var sidebar: SidebarItem? = .all
    @State private var selection: Machine.ID?
    @State private var search = ""
    @State private var dropTargeted = false

    var body: some View {
        @Bindable var editor = editor
        NavigationSplitView {
            Sidebar(selection: $sidebar)
                .navigationSplitViewColumnWidth(min: 180, ideal: 210)
        } detail: {
            libraryView
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .navigationTitle(title)
                .navigationSubtitle(subtitle)
                .inspector(isPresented: $showInspector) {
                    inspector
                        .inspectorColumnWidth(min: 300, ideal: 340, max: 460)
                }
        }
        .searchable(text: $search, placement: .toolbar, prompt: "Name or system")
        .toolbar { toolbar }
        .sheet(item: $editor.draft) { draft in
            MachineEditor(machine: draft, isNew: editor.isNew) { saved in
                library.save(saved)
                selection = saved.id
            }
        }
        .dropDestination(for: URL.self) { urls, _ in
            guard let draft = Self.draft(from: urls) else { return false }
            editor.draft = draft
            editor.isNew = true
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
            if let name = ManagerApp.argument("--select") {
                selection = library.machines.first { $0.name == name }?.id
            }
        }
    }

    @ViewBuilder private var libraryView: some View {
        let machines = visibleMachines
        if library.machines.isEmpty {
            ContentUnavailableView {
                Label("No Machines Yet", systemImage: "desktopcomputer")
            } description: {
                Text("Make a machine, or drop a disk image or CD image here.")
            } actions: {
                Button("New Machine…", systemImage: "plus") { editor.new(.windowsXP) }
                    .buttonStyle(.glassProminent)
                    .controlSize(.large)
            }
        } else if machines.isEmpty {
            if !search.isEmpty {
                ContentUnavailableView.search(text: search)
            } else {
                ContentUnavailableView(
                    sidebar == .running ? "Nothing Is Running" : "No Machines Here", systemImage: "desktopcomputer")
            }
        } else {
            MachineGrid(machines: machines, selection: $selection)
        }
    }

    @ViewBuilder private var inspector: some View {
        if let machine = library.machine(selection) {
            MachineInspector(machine: machine)
                .id(machine.id)
        } else {
            ContentUnavailableView(
                "No Machine Selected", systemImage: "desktopcomputer",
                description: Text("Select a machine to see its details."))
        }
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

    @ToolbarContentBuilder private var toolbar: some ToolbarContent {
        ToolbarItem(placement: .primaryAction) {
            Button("New Machine", systemImage: "plus") { editor.new(.windowsXP) }
                .help("Make a new machine")
        }
        ToolbarSpacer(.fixed, placement: .primaryAction)
        ToolbarItem(placement: .primaryAction) {
            if let machine = library.machine(selection), sessions.isRunning(machine.id) {
                Button("Stop", systemImage: "stop.fill") {
                    sessions.session(for: machine.id)?.stop(thumbnail: library.store.thumbnail(of: machine.id))
                }
                .help("Shut the machine off")
            } else {
                Button("Start", systemImage: "play.fill") {
                    if let machine = library.machine(selection) { start(machine) }
                }
                .buttonStyle(.glassProminent)
                .disabled(selection == nil)
                .help("Start the selected machine")
            }
        }
        ToolbarItem(placement: .primaryAction) {
            Button("Inspector", systemImage: "sidebar.trailing") { showInspector.toggle() }
                .help(showInspector ? "Hide the machine details" : "Show the machine details")
        }
    }

    private var visibleMachines: [Machine] {
        library.machines.filter { machine in
            switch sidebar {
            case .running: if !sessions.isRunning(machine.id) { return false }
            case .os(let os): if machine.os != os { return false }
            case .all, nil: break
            }
            return machine.matches(search)
        }
    }

    private var title: String {
        switch sidebar {
        case .running: "Running"
        case .os(let os): os.label
        case .all, nil: "All Machines"
        }
    }

    private var subtitle: String {
        let n = visibleMachines.count
        return n == 1 ? "1 machine" : "\(n) machines"
    }

    private func start(_ machine: Machine) {
        sessions.start(machine, settings: launchSettings(), folder: library.store.folder(of: machine.id))
    }

    private static func draft(from urls: [URL]) -> Machine? {
        let disks = urls.filter { ["img", "raw", "hdd", "bin"].contains($0.pathExtension.lowercased()) }
        let isos = urls.filter { ["iso", "cdr"].contains($0.pathExtension.lowercased()) }
        guard !disks.isEmpty || !isos.isEmpty else { return nil }
        let name = (disks.first ?? isos.first)!.deletingPathExtension().lastPathComponent
        let guess: OSKind =
            name.localizedCaseInsensitiveContains("xp") || name.localizedCaseInsensitiveContains("win")
            ? .windowsXP
            : ["ubuntu", "linux", "debian", "fedora", "arch"].contains { name.localizedCaseInsensitiveContains($0) }
                ? .linux : .other
        var draft = Machine.template(guess)
        draft.name = name
        draft.config.hda = disks.first?.path
        draft.config.hdb = disks.dropFirst().first?.path
        draft.config.cdrom = isos.first?.path
        return draft
    }
}

struct Sidebar: View {
    @Environment(Library.self) private var library
    @Environment(Sessions.self) private var sessions
    @Binding var selection: SidebarItem?

    var body: some View {
        List(selection: $selection) {
            Section("Library") {
                Label("All Machines", systemImage: "square.grid.2x2")
                    .badge(library.machines.count)
                    .tag(SidebarItem.all)
                Label("Running", systemImage: "play.circle")
                    .badge(sessions.runningCount)
                    .tag(SidebarItem.running)
            }
            Section("Systems") {
                ForEach(OSKind.allCases, id: \.self) { os in
                    Label(os.label, systemImage: os.symbol)
                        .badge(library.machines.filter { $0.os == os }.count)
                        .tag(SidebarItem.os(os))
                }
            }
        }
    }
}
