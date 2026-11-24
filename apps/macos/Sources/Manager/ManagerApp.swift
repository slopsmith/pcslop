import AppKit
import ManagerCore
import SwiftUI

@main
struct ManagerApp: App {
    @NSApplicationDelegateAdaptor private var delegate: AppDelegate
    @State private var library = Library(store: ManagerApp.store())
    @State private var sessions = Sessions()
    @State private var editor = EditorRequest()

    var body: some Scene {
        Window("pcslop", id: "library") {
            ContentView()
                .environment(library)
                .environment(sessions)
                .environment(editor)
                .frame(minWidth: 820, minHeight: 520)
                .task { await Snapshot.takeIfAsked() }
                .onAppear {
                    delegate.sessions = sessions
                    sessions.onExit = { _ in library.thumbnailsChanged() }
                }
        }
        .defaultSize(width: 1180, height: 740)
        .commands {
            CommandGroup(replacing: .newItem) {
                Button("New Machine…") { editor.new(.windowsXP) }
                    .keyboardShortcut("n")
                Divider()
                Button("Open Machines Folder") { openMachinesFolder(library.store) }
            }
        }

        Settings {
            SettingsView()
        }
    }

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

final class AppDelegate: NSObject, NSApplicationDelegate {
    @MainActor var sessions: Sessions?

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
        guard alert.runModal() == .alertFirstButtonReturn else { return .terminateCancel }
        sessions.terminateAll()
        return .terminateNow
    }
}

@MainActor
@Observable
final class EditorRequest {
    var draft: Machine?
    var isNew = false

    func new(_ os: OSKind) {
        draft = .template(os)
        isNew = true
    }

    func edit(_ machine: Machine) {
        draft = machine
        isNew = false
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
    return LaunchSettings(binary: binary, firmware: Firmware(bios: bios, vgaBIOS: vga))
}

enum Defaults {
    static let binary = "pcslopBinary"
    static let bios = "biosPath"
    static let vgaBIOS = "vgaBIOSPath"
    static let showInspector = "showInspector"
}
