import AppKit
import ManagerCore
import SwiftUI

struct MachineEditor: View {
    @State var machine: Machine
    let isNew: Bool
    let save: (Machine) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var forwards: String
    @State private var diskError: String?

    init(machine: Machine, isNew: Bool, save: @escaping (Machine) -> Void) {
        _machine = State(initialValue: machine)
        _forwards = State(initialValue: machine.config.hostForwards.joined(separator: " "))
        self.isNew = isNew
        self.save = save
    }

    private static let ramChoices = [64, 128, 256, 384, 512, 768, 1024, 1536, 2048, 3072]

    var body: some View {
        VStack(spacing: 0) {
            Form {
                general
                hardware
                drives
                devices
                advanced
            }
            .formStyle(.grouped)
            Divider()
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button(isNew ? "Create" : "Save") {
                    var result = machine
                    result.name = result.name.trimmingCharacters(in: .whitespaces)
                    result.config.hostForwards = forwards.split(whereSeparator: { $0 == " " || $0 == "," }).map(
                        String.init)
                    save(result)
                    dismiss()
                }
                .buttonStyle(.glassProminent)
                .keyboardShortcut(.defaultAction)
                .disabled(machine.name.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            .padding(14)
        }
        .frame(width: 560, height: 680)
        .alert(
            "Cannot make the disk image",
            isPresented: Binding(get: { diskError != nil }, set: { if !$0 { diskError = nil } })
        ) {
            Button("OK") {}
        } message: {
            Text(diskError ?? "")
        }
    }

    private var general: some View {
        Section(isNew ? "New Machine" : "General") {
            TextField("Name", text: $machine.name)
            Picker("System", selection: osBinding) {
                ForEach(OSKind.allCases, id: \.self) { Label($0.label, systemImage: $0.symbol).tag($0) }
            }
            TextField("Notes", text: $machine.notes, axis: .vertical)
                .lineLimit(1...4)
        }
    }

    private var hardware: some View {
        Section {
            Picker("Display", selection: $machine.config.gpu) {
                Text("Standard VGA").tag(GPU.stdvga)
                Text("Radeon 9700").tag(GPU.radeon)
            }
            if machine.config.gpu == .radeon {
                FileField(
                    title: "Video BIOS", path: $machine.config.vgaROM, placeholder: "Required",
                    message: "Choose the Radeon 9700's video BIOS ROM.")
                Picker("3D rendering", selection: $machine.config.renderer) {
                    Text("Metal").tag(Renderer.metal)
                    Text("CPU (reference)").tag(Renderer.cpu)
                }
            }
            Picker("Board", selection: boardBinding) {
                Text("440FX").tag(Board.fx)
                Text("440BX with AGP").tag(Board.bx)
            }
            .disabled(machine.config.gpu == .radeon)
            Picker("Memory", selection: $machine.config.ramMiB) {
                ForEach(ramChoices, id: \.self) { mib in
                    Text(mib >= 1024 ? "\(Double(mib) / 1024, format: .number) GiB" : "\(mib) MiB").tag(mib)
                }
            }
        } header: {
            Text("Hardware")
        } footer: {
            if machine.config.gpu == .radeon {
                Text("The Radeon sits on the 440BX's AGP bus.")
            }
        }
    }

    private var ramChoices: [Int] {
        Self.ramChoices.contains(machine.config.ramMiB)
            ? Self.ramChoices : (Self.ramChoices + [machine.config.ramMiB]).sorted()
    }

    private var drives: some View {
        Section {
            FileField(title: "Primary disk", path: $machine.config.hda, message: "Choose a raw disk image.")
            FileField(title: "Secondary disk", path: $machine.config.hdb, message: "Choose a raw disk image.")
            FileField(
                title: "CD-ROM", path: $machine.config.cdrom, placeholder: "Empty", kinds: FileKind.cdImage,
                message: "Choose a CD image.")
            HStack {
                Spacer()
                Menu("New Blank Disk") {
                    ForEach([2, 4, 8, 16, 32] as [UInt64], id: \.self) { gib in
                        Button("\(gib) GiB…") { makeDisk(gib: gib) }
                    }
                }
                .fixedSize()
            }
        } header: {
            Text("Drives")
        } footer: {
            Text("Starting from a saved state uses the disk saved with it, so the primary disk stays as it was.")
        }
    }

    private var devices: some View {
        Section("Devices") {
            Toggle("Network", isOn: $machine.config.network)
            if machine.config.network {
                TextField("Port forwards", text: $forwards, prompt: Text("tcp::2222-:22"))
                    .help("hostfwd rules, separated by spaces: [tcp|udp]:[HOSTADDR]:HOSTPORT-[GUESTADDR]:GUESTPORT")
            }
            Toggle("Sound", isOn: $machine.config.audio)
            Picker("Battery", selection: batteryKind) {
                Text("Like this Mac").tag(0)
                Text("None").tag(1)
                Text("Fixed level").tag(2)
            }
            if case .fixed(let percent, let ac) = machine.config.battery {
                LabeledContent("Level") {
                    HStack {
                        Slider(
                            value: Binding(
                                get: { Double(percent) },
                                set: { machine.config.battery = .fixed(percent: Int($0), ac: ac) }),
                            in: 0...100, step: 5)
                        Text("\(percent)%").monospacedDigit().frame(width: 44, alignment: .trailing)
                    }
                }
                Toggle(
                    "On AC power",
                    isOn: Binding(get: { ac }, set: { machine.config.battery = .fixed(percent: percent, ac: $0) }))
            }
            Picker("Capture the mouse", selection: $machine.config.captureMouse) {
                Text("With Ctrl+Option+G").tag(CaptureMode.key)
                Text("On a click").tag(CaptureMode.click)
                Text("On a click without a tablet driver").tag(CaptureMode.auto)
            }
        }
    }

    private var advanced: some View {
        Section {
            Toggle("JIT", isOn: $machine.config.jit)
            TextField("Extra arguments", text: $machine.config.extraArgs, prompt: Text("--serial com1.log"))
                .font(.body.monospaced())
            ForEach(machine.config.problems(), id: \.self) { problem in
                Label(problem, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
            }
        } header: {
            Text("Advanced")
        } footer: {
            Text(
                "Without the JIT, pcslop interprets every instruction: slow, but useful when chasing a bug. Extra arguments go to pcslop as they are; relative paths start in the machine's folder."
            )
        }
    }

    private var osBinding: Binding<OSKind> {
        Binding(
            get: { machine.os },
            set: { os in
                if isNew {
                    let template = Machine.template(os)
                    if machine.name == Machine.template(machine.os).name { machine.name = template.name }
                    machine.config.ramMiB = template.config.ramMiB
                    machine.config.captureMouse = template.config.captureMouse
                }
                machine.os = os
            })
    }

    private var boardBinding: Binding<Board> {
        Binding(
            get: { machine.config.gpu == .radeon ? .bx : machine.config.board },
            set: { machine.config.board = $0 })
    }

    private var batteryKind: Binding<Int> {
        Binding(
            get: {
                switch machine.config.battery {
                case .host: 0
                case .none: 1
                case .fixed: 2
                }
            },
            set: { kind in
                switch kind {
                case 0: machine.config.battery = .host
                case 1: machine.config.battery = .none
                default: machine.config.battery = .fixed(percent: 80, ac: false)
                }
            })
    }

    private func makeDisk(gib: UInt64) {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = "\(machine.name.isEmpty ? "disk" : machine.name).img"
        panel.message = "A \(gib) GiB raw disk image. It takes space only as the guest fills it."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        do {
            try DiskImage.createBlank(at: url, mebibytes: gib * 1024)
            if machine.config.hda?.isEmpty ?? true {
                machine.config.hda = url.path
            } else {
                machine.config.hdb = url.path
            }
        } catch {
            diskError = error.localizedDescription
        }
    }
}
