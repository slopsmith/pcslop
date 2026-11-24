import AppKit
import ManagerCore
import SwiftUI

struct SettingsView: View {
    @AppStorage(Defaults.radeonROM) private var radeonROM = ""
    @AppStorage(Defaults.binary) private var binary = ""
    @AppStorage(Defaults.bios) private var bios = ""
    @AppStorage(Defaults.vgaBIOS) private var vgaBIOS = ""
    @AppStorage(Defaults.checkForUpdates) private var checkForUpdates = true

    var body: some View {
        Form {
            Section {
                FileField(
                    title: "Video BIOS", path: $radeonROM.optional, placeholder: "None",
                    message: "Choose a ROM image of a Radeon 9700 Pro's video BIOS.", detail: fileSize(radeonROM))
            } header: {
                Text("ATI Radeon 9700 Pro")
            } footer: {
                Text(
                    "Machines can use an emulated Radeon 9700 Pro for 2D and 3D acceleration. It needs a ROM image of a real 9700 Pro's video BIOS, which pcslop can't include. A plain Radeon 9700's video BIOS does not work: the machine never shows a picture. Until you choose one, machines use standard VGA."
                )
            }
            Section {
                ComponentField(
                    title: "Emulator", override: $binary, builtIn: BinaryLocator.pcslop(),
                    message: "Choose a pcslop binary signed with the Hypervisor entitlement.")
                ComponentField(
                    title: "BIOS", override: $bios, builtIn: BinaryLocator.bios(),
                    message: "Choose a SeaBIOS bios.bin.")
                ComponentField(
                    title: "VGA BIOS", override: $vgaBIOS, builtIn: BinaryLocator.vgaBIOS(),
                    message: "Choose a SeaVGABIOS vgabios.bin.")
            } header: {
                Text("Components")
            } footer: {
                Text("pcslop comes with its emulator and SeaBIOS. Choose other files only to try a build of your own.")
            }
            Section {
                Toggle("Check for updates when pcslop starts", isOn: $checkForUpdates)
            } footer: {
                Text("Compares this build with the newest nightly on GitHub. Local builds never check.")
            }
        }
        .formStyle(.grouped)
        .frame(width: 560)
        .fixedSize(horizontal: false, vertical: true)
    }
}

private struct ComponentField: View {
    let title: String
    @Binding var override: String
    let builtIn: URL?
    let message: String

    var body: some View {
        LabeledContent(title) {
            HStack {
                Text(shown)
                    .foregroundStyle(override.isEmpty ? .secondary : .primary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(override.isEmpty ? builtIn?.path ?? "" : override)
                Spacer(minLength: 0)
                if !override.isEmpty {
                    Button("Use Built-In") { override = "" }
                }
                Button("Choose…") {
                    if let url = FilePicker.open(kinds: [], message: message) { override = url.path }
                }
            }
        }
    }

    private var shown: String {
        if !override.isEmpty { return URL(fileURLWithPath: override).lastPathComponent }
        guard let builtIn else { return "Missing" }
        return builtIn.path.contains(".app/Contents/") ? "Built in" : "\(builtIn.lastPathComponent), from a development build"
    }
}
