import ManagerCore
import SwiftUI

struct WelcomeView: View {
    @Environment(Library.self) private var library
    @Environment(EditorRequest.self) private var editor

    var body: some View {
        ScrollView {
            VStack(spacing: 28) {
                VStack(spacing: 8) {
                    Text("Welcome to pcslop")
                        .font(.largeTitle.weight(.semibold))
                    Text(
                        library.machines.isEmpty
                            ? "An x86 PC for Windows XP and Linux. Make your first machine to get started."
                            : "Pick a machine on the left, or make a new one."
                    )
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                }
                Grid(horizontalSpacing: 14, verticalSpacing: 14) {
                    GridRow {
                        WelcomeTile("New Machine", systemImage: "plus") { editor.newMachine() }
                        WelcomeTile("Use a Disk Image", systemImage: "internaldrive") { useDiskImage() }
                    }
                    GridRow {
                        WelcomeTile("Machines Folder", systemImage: "folder") { openMachinesFolder(library.store) }
                        SettingsLink {
                            WelcomeTileLabel(title: "Settings", systemImage: "gearshape")
                        }
                        .buttonStyle(WelcomeTileStyle())
                    }
                }
                .frame(maxWidth: 480)
            }
            .padding(32)
            .frame(maxWidth: .infinity)
        }
        .scrollBounceBehavior(.basedOnSize)
        .defaultScrollAnchor(.center)
    }

    private func useDiskImage() {
        guard let url = FilePicker.open(kinds: [], message: "Choose a disk image or a CD image."),
            let request = ContentView.request(from: [url])
        else { return }
        editor.newMachine(request)
    }
}

struct WelcomeTile: View {
    let title: String
    let systemImage: String
    let action: () -> Void

    init(_ title: String, systemImage: String, action: @escaping () -> Void) {
        self.title = title
        self.systemImage = systemImage
        self.action = action
    }

    var body: some View {
        Button(action: action) {
            WelcomeTileLabel(title: title, systemImage: systemImage)
        }
        .buttonStyle(WelcomeTileStyle())
    }
}

struct WelcomeTileLabel: View {
    let title: String
    let systemImage: String

    var body: some View {
        VStack(spacing: 14) {
            Image(systemName: systemImage)
                .font(.system(size: 30, weight: .regular))
                .frame(width: 64, height: 64)
                .overlay { Circle().strokeBorder(.secondary.opacity(0.6), lineWidth: 2.5) }
            Text(title)
                .font(.title3.weight(.medium))
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity, minHeight: 150)
    }
}

struct WelcomeTileStyle: ButtonStyle {
    @State private var hovering = false

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .foregroundStyle(.primary)
            .contentShape(.rect(cornerRadius: 18))
            .glassEffect(
                .regular.tint(hovering ? Color.accentColor.opacity(0.18) : .clear).interactive(),
                in: .rect(cornerRadius: 18, style: .continuous)
            )
            .scaleEffect(configuration.isPressed ? 0.97 : 1)
            .onHover { hovering = $0 }
            .animation(.spring(duration: 0.25), value: hovering)
            .animation(.spring(duration: 0.2), value: configuration.isPressed)
    }
}
