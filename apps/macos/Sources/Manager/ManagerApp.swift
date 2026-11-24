import AppKit
import ManagerCore
import SwiftUI

@main
struct ManagerApp: App {
    @NSApplicationDelegateAdaptor private var delegate: AppDelegate
    @State private var library = Library(store: ManagerApp.store())
    @State private var sessions = Sessions()
    @State private var editor = EditorRequest()
    @State private var updates = Updates()

    var body: some Scene {
        Window("pcslop", id: "library") {
            if Self.showsAbout {
                AboutView()
                    .task { await Snapshot.takeIfAsked() }
            } else {
                libraryView
            }
        }
        .defaultSize(width: Self.showsAbout ? 860 : 1080, height: Self.showsAbout ? 600 : 720)
        .commands {
            CommandGroup(replacing: .appInfo) {
                AboutButton()
                Button("Check for Updates…") { Task { await updates.checkNow() } }
            }
            CommandGroup(replacing: .newItem) {
                Button("New Machine…") { editor.newMachine() }
                    .keyboardShortcut("n")
                Divider()
                Button("Open Machines Folder") { openMachinesFolder(library.store) }
            }
        }

        Window("About pcslop", id: "about") {
            AboutView()
        }
        .defaultSize(width: 860, height: 600)
        .windowResizability(.contentMinSize)
        .restorationBehavior(.disabled)
        .defaultLaunchBehavior(.suppressed)

        Settings {
            SettingsView()
        }
    }

    private var libraryView: some View {
        ContentView()
            .environment(library)
            .environment(sessions)
            .environment(editor)
            .frame(minWidth: 760, minHeight: 520)
            .task { await Snapshot.takeIfAsked() }
            .onAppear {
                delegate.sessions = sessions
                Snapshot.sessions = sessions
                sessions.onExit = { _ in library.thumbnailsChanged() }
                sessions.onThumbnail = { _ in library.thumbnailsChanged() }
            }
            .task { await updates.checkOnLaunch() }
            .modifier(UpdateAlert(updates: updates))
    }

    /// `--about` shows the About view in the main window instead of the library, for `--snapshot`.
    static let showsAbout = CommandLine.arguments.contains("--about")

    static func argument(_ name: String) -> String? {
        let args = CommandLine.arguments
        guard let i = args.firstIndex(of: name), i + 1 < args.count else { return nil }
        return args[i + 1]
    }

    // `--store DIR` keeps machines in DIR instead of Application Support, for screenshots and tests.
    private static func store() -> MachineStore {
        if let path = argument("--store") { return MachineStore(root: URL(fileURLWithPath: path)) }
        return .standard
    }
}

struct AboutButton: View {
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button("About pcslop") { openWindow(id: "about") }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    @MainActor var sessions: Sessions?
    @MainActor static var showLibrary: (() -> Void)?

    @MainActor
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        (sessions?.runningCount ?? 0) == 0
    }

    @MainActor
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard let sessions, sessions.runningCount > 0 else { return .terminateNow }
        let alert = NSAlert()
        alert.messageText =
            sessions.runningCount == 1
            ? "A machine is still running." : "\(sessions.runningCount) machines are still running."
        alert.informativeText = "Quitting turns them off without saving their state."
        alert.addButton(withTitle: "Stop and Quit")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else {
            // SwiftUI has already closed the window on its way to quitting.
            DispatchQueue.main.async { AppDelegate.showLibrary?() }
            return .terminateCancel
        }
        sessions.terminateAll()
        return .terminateNow
    }
}

struct NewMachineRequest: Identifiable {
    let id = UUID()
    var preset: Preset?
    var disk: String?
    var cdrom: String?
}

@MainActor
@Observable
final class EditorRequest {
    var creating: NewMachineRequest?
    var editing: Machine?

    func newMachine(_ request: NewMachineRequest = NewMachineRequest()) {
        creating = request
    }

    func edit(_ machine: Machine) {
        editing = machine
    }
}

@MainActor
func openMachinesFolder(_ store: MachineStore) {
    try? FileManager.default.createDirectory(at: store.root, withIntermediateDirectories: true)
    NSWorkspace.shared.open(store.root)
}

@MainActor
func launchSettings() -> LaunchSettings? {
    let defaults = UserDefaults.standard
    let override = defaults.string(forKey: Defaults.binary) ?? ""
    guard let binary = BinaryLocator.pcslop(override: override) else { return nil }
    let biosPath = defaults.string(forKey: Defaults.bios) ?? ""
    guard let bios = biosPath.isEmpty ? BinaryLocator.bios()?.path : biosPath else { return nil }
    let vgaPath = defaults.string(forKey: Defaults.vgaBIOS) ?? ""
    let vga = vgaPath.isEmpty ? BinaryLocator.vgaBIOS()?.path : vgaPath
    return LaunchSettings(binary: binary, firmware: Firmware(bios: bios, vgaBIOS: vga, radeonROM: appRadeonROM()))
}

@MainActor
func appRadeonROM() -> String? {
    let path = UserDefaults.standard.string(forKey: Defaults.radeonROM) ?? ""
    return path.isEmpty ? nil : path
}

enum Defaults {
    static let radeonROM = "radeonROM"
    static let binary = "pcslopBinary"
    static let bios = "biosPath"
    static let vgaBIOS = "vgaBIOSPath"
    static let checkForUpdates = "checkForUpdates"
}
