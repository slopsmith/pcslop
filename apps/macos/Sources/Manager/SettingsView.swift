import AppKit
import ManagerCore
import SwiftUI

struct SettingsView: View {
    @AppStorage(Defaults.binary) private var binary = ""
    @AppStorage(Defaults.bios) private var bios = ""
    @AppStorage(Defaults.vgaBIOS) private var vgaBIOS = ""

    var body: some View {
        Form {
            Section {
                FileField(
                    title: "pcslop", path: $binary.optional,
                    placeholder: BinaryLocator.pcslop()?.path ?? "Not found",
                    message: "Choose the pcslop binary.")
            } footer: {
                Text(
                    "By default the copy in this app, then PCSLOP_BIN, then target/release/pcslop in the repository. It needs the Hypervisor entitlement; cargo run signs it."
                )
            }
            Section {
                FileField(
                    title: "BIOS", path: $bios.optional,
                    placeholder: BinaryLocator.bios()?.path ?? "Not found",
                    message: "Choose SeaBIOS's bios.bin.")
                FileField(
                    title: "VGA BIOS", path: $vgaBIOS.optional,
                    placeholder: BinaryLocator.vgaBIOS()?.path ?? "Not found",
                    message: "Choose SeaVGABIOS's vgabios.bin.")
            } header: {
                Text("Firmware")
            } footer: {
                Text(
                    "By default the SeaBIOS builds in references/, made with nix build .#seabios --out-link references/seabios. The VGA BIOS is for the standard VGA display; the Radeon brings its own."
                )
            }
        }
        .formStyle(.grouped)
        .frame(width: 560)
        .fixedSize(horizontal: false, vertical: true)
    }
}
