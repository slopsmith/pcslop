import AppKit
import ManagerCore
import SwiftUI

enum SettingsPage: String, CaseIterable, Identifiable {
    case information, system, display, network, sound, input, power, drives, advanced

    var id: String { rawValue }

    var title: String {
        switch self {
        case .information: "Information"
        case .system: "System"
        case .display: "Display"
        case .network: "Network"
        case .sound: "Sound"
        case .input: "Input"
        case .power: "Power"
        case .drives: "Disks and CD"
        case .advanced: "Advanced"
        }
    }

    var symbol: String {
        switch self {
        case .information: "info.circle"
        case .system: "cpu"
        case .display: "display"
        case .network: "network"
        case .sound: "speaker.wave.2"
        case .input: "computermouse"
        case .power: "battery.75percent"
        case .drives: "internaldrive"
        case .advanced: "wrench.and.screwdriver"
        }
    }
}

struct MachineSettings: View {
    @State var machine: Machine
    let save: (Machine) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var page: SettingsPage? = ManagerApp.argument("--page").flatMap(SettingsPage.init) ?? .information
    @State private var forwards: String
    @AppStorage(Defaults.radeonROM) private var radeonROM = ""

    init(machine: Machine, save: @escaping (Machine) -> Void) {
        _machine = State(initialValue: machine)
        _forwards = State(initialValue: machine.config.hostForwards.joined(separator: " "))
        self.save = save
    }

    var body: some View {
        HStack(spacing: 0) {
            sidebar
                .frame(width: 200)
            Divider()
            VStack(spacing: 0) {
                Form {
                    pageContent
                }
                .formStyle(.grouped)
                .scrollContentBackground(.hidden)
                Divider()
                footer
            }
        }
        .frame(width: 740, height: 500)
    }

    private var sidebar: some View {
        List(selection: $page) {
            Section {
                row(.information)
                row(.system)
            }
            Section("Devices") {
                row(.display)
                row(.network)
                row(.sound)
                row(.input)
                row(.power)
            }
            Section("Drives") {
                row(.drives)
            }
            Section {
                row(.advanced)
            }
        }
        .listStyle(.sidebar)
    }

    private func row(_ page: SettingsPage) -> some View {
        Label(page.title, systemImage: page.symbol)
            .tag(page)
    }

    private var footer: some View {
        HStack {
            let problems = machine.config.problems(radeonROM: radeonROM.isEmpty ? nil : radeonROM)
            if !problems.isEmpty {
                Label(problems.count == 1 ? problems[0] : "\(problems.count) problems", systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(problems.joined(separator: "\n"))
            }
            Spacer()
            Button("Cancel", role: .cancel) { dismiss() }
                .keyboardShortcut(.cancelAction)
            Button("Save") {
                var result = machine
                result.name = result.name.trimmingCharacters(in: .whitespaces)
                result.config.hostForwards = forwards.split(whereSeparator: { $0 == " " || $0 == "," }).map(String.init)
                save(result)
                dismiss()
            }
            .buttonStyle(.glassProminent)
            .keyboardShortcut(.defaultAction)
            .disabled(machine.name.trimmingCharacters(in: .whitespaces).isEmpty)
        }
        .padding(14)
    }

    @ViewBuilder private var pageContent: some View {
        switch page ?? .information {
        case .information: information
        case .system: system
        case .display: display
        case .network: network
        case .sound: sound
        case .input: input
        case .power: power
        case .drives: drives
        case .advanced: advanced
        }
    }

    private var information: some View {
        Section {
            TextField("Name", text: $machine.name)
            LabeledContent("System") {
                OSPicker(selection: $machine.os)
            }
            TextField("Notes", text: $machine.notes, prompt: Text("Anything worth remembering"), axis: .vertical)
                .lineLimit(5...10)
        } footer: {
            HStack(spacing: 10) {
                OSIcon(os: machine.os, size: 32)
                Text(machine.config.summary).foregroundStyle(.secondary)
            }
            .padding(.top, 6)
        }
    }

    private var system: some View {
        Group {
            Section {
                Picker("Board", selection: boardBinding) {
                    Text("Intel 440FX").tag(Board.fx)
                    Text("Intel 440BX with AGP").tag(Board.bx)
                }
                .pickerStyle(.radioGroup)
                .disabled(machine.config.gpu == .radeon)
            } footer: {
                if machine.config.gpu == .radeon {
                    Text("The Radeon 9700 sits on the 440BX's AGP bus, so the board follows the display.")
                }
            }
            Section {
                MemoryField(mib: $machine.config.ramMiB)
            } footer: {
                Text("Windows XP runs well with 512 MB. More than 3 GB does not fit below the PCI hole.")
            }
            Section {
                Toggle("Run guest code with the JIT", isOn: $machine.config.jit)
            } footer: {
                Text("Without the JIT, pcslop interprets every instruction: much slower, but useful when chasing a bug.")
            }
        }
    }

    private var display: some View {
        Group {
            Section {
                Picker("Graphics card", selection: $machine.config.gpu) {
                    Text("Standard VGA").tag(GPU.stdvga)
                    Text("ATI Radeon 9700, with 3D").tag(GPU.radeon)
                        .disabled(radeonROM.isEmpty && machine.config.gpu != .radeon)
                }
                .pickerStyle(.radioGroup)
            } footer: {
                if radeonROM.isEmpty {
                    VStack(alignment: .leading, spacing: 6) {
                        Text("The Radeon 9700 needs a copy of the real card's video BIOS. Choose one in Settings to use it.")
                        SettingsLink { Text("Open Settings…") }
                    }
                }
            }
            if machine.config.gpu == .radeon {
                Section {
                    Picker("3D rendering", selection: $machine.config.renderer) {
                        Text("Metal").tag(Renderer.metal)
                        Text("CPU").tag(Renderer.cpu)
                    }
                    .pickerStyle(.segmented)
                } footer: {
                    Text("Metal draws on this Mac's GPU. The CPU renderer is much slower and only useful for comparing against Metal.")
                }
            }
        }
    }

    private var network: some View {
        Section {
            Toggle("Network card", isOn: $machine.config.network)
            if machine.config.network {
                LabeledContent("Port forwards") {
                    TextField("Port forwards", text: $forwards, prompt: Text("tcp::2222-:22"))
                        .labelsHidden()
                        .font(.body.monospaced())
                }
            }
        } footer: {
            Text("An RTL8139 on a user-mode network: the guest reaches the internet through this Mac. Port forwards are separated by spaces, as [tcp|udp]:[HOSTADDR]:HOSTPORT-[GUESTADDR]:GUESTPORT.")
        }
    }

    private var sound: some View {
        Section {
            Toggle("AC'97 sound card", isOn: $machine.config.audio)
        } footer: {
            Text("Plays through this Mac's default output.")
        }
    }

    private var input: some View {
        Section {
            Picker("Capture the mouse", selection: $machine.config.captureMouse) {
                Text("Only with Control-Option-G").tag(CaptureMode.key)
                Text("On a click in the window").tag(CaptureMode.click)
                Text("On a click, until the guest's tablet driver loads").tag(CaptureMode.auto)
            }
            .pickerStyle(.radioGroup)
        } footer: {
            Text("With a USB tablet driver, Windows follows the Mac's pointer without capturing it.")
        }
    }

    private var power: some View {
        Section {
            Picker("Battery", selection: batteryKind) {
                Text("Mirror this Mac's battery").tag(0)
                Text("No battery").tag(1)
                Text("Fixed level").tag(2)
            }
            .pickerStyle(.radioGroup)
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
        }
    }

    private var drives: some View {
        Group {
            Section("Hard disks") {
                DiskField(title: "Primary", path: $machine.config.hda, machineName: machine.name)
                DiskField(title: "Secondary", path: $machine.config.hdb, machineName: machine.name)
            }
            Section {
                FileField(
                    title: "CD-ROM", path: $machine.config.cdrom, placeholder: "Empty", kinds: FileKind.cdImage,
                    message: "Choose a CD image.")
            } footer: {
                Text("Starting from a saved state uses the disk saved with it, so these disks stay as they were.")
            }
        }
    }

    private var advanced: some View {
        Section {
            LabeledContent("Extra arguments") {
                TextField("Extra arguments", text: $machine.config.extraArgs, prompt: Text("--serial com1.log"))
                    .labelsHidden()
                    .font(.body.monospaced())
            }
            ForEach(machine.config.problems(radeonROM: radeonROM.isEmpty ? nil : radeonROM), id: \.self) { problem in
                Label(problem, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
            }
        } footer: {
            Text("Extra arguments go to pcslop as they are. Relative paths start in the machine's folder.")
        }
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
}

struct MemoryField: View {
    @Binding var mib: Int

    var body: some View {
        LabeledContent("Memory") {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                VStack(spacing: 2) {
                    Slider(
                        value: Binding(
                            get: { Double(MemorySize.nearestStep(to: mib)) },
                            set: { mib = MemorySize.steps[Int($0.rounded())] }),
                        in: 0...Double(MemorySize.steps.count - 1), step: 1
                    )
                    HStack {
                        Text(MachineConfig.memory(MemorySize.range.lowerBound))
                        Spacer()
                        Text(MachineConfig.memory(MemorySize.range.upperBound))
                    }
                    .font(.caption)
                    .foregroundStyle(.tertiary)
                }
                .frame(minWidth: 200)
                TextField(
                    "MB", value: Binding(get: { mib }, set: { mib = MemorySize.clamped($0) }), format: .number
                )
                .labelsHidden()
                .multilineTextAlignment(.trailing)
                .monospacedDigit()
                .frame(width: 64)
                Text("MB").foregroundStyle(.secondary)
            }
        }
    }
}

struct DiskField: View {
    let title: String
    @Binding var path: String?
    let machineName: String
    @State private var error: String?

    var body: some View {
        FileField(title: title, path: $path, message: "Choose a raw disk image.", detail: fileSize(path))
            .contextMenu {
                Menu("New Blank Disk") { sizes }
            }
        if path?.isEmpty ?? true {
            LabeledContent("") {
                Menu("New Blank Disk…") { sizes }
                    .fixedSize()
            }
            .alert(
                "Cannot make the disk image",
                isPresented: Binding(get: { error != nil }, set: { if !$0 { error = nil } })
            ) {
                Button("OK") {}
            } message: {
                Text(error ?? "")
            }
        }
    }

    @ViewBuilder private var sizes: some View {
        ForEach([2, 4, 8, 16, 32, 64] as [UInt64], id: \.self) { gib in
            Button("\(gib) GB") { makeDisk(gib: gib) }
        }
    }

    private func makeDisk(gib: UInt64) {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = "\(machineName.isEmpty ? "disk" : machineName).img"
        panel.message = "A \(gib) GB raw disk image. It takes space only as the guest fills it."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        do {
            try DiskImage.createBlank(at: url, mebibytes: gib * 1024)
            path = url.path
        } catch {
            self.error = error.localizedDescription
        }
    }
}

struct OSPicker: View {
    @Binding var selection: OSKind

    var body: some View {
        LazyVGrid(columns: Array(repeating: GridItem(.fixed(78), spacing: 6), count: 5), spacing: 6) {
            ForEach(OSKind.allCases, id: \.self) { os in
                Button {
                    selection = os
                } label: {
                    VStack(spacing: 5) {
                        OSIcon(os: os, size: 34)
                        Text(os.label)
                            .font(.caption)
                            .lineLimit(1)
                            .minimumScaleFactor(0.8)
                    }
                    .frame(width: 74, height: 64)
                    .background(
                        selection == os ? Color.accentColor.opacity(0.18) : .clear,
                        in: .rect(cornerRadius: 10, style: .continuous)
                    )
                    .overlay {
                        RoundedRectangle(cornerRadius: 10, style: .continuous)
                            .strokeBorder(selection == os ? Color.accentColor : .clear, lineWidth: 1.5)
                    }
                    .contentShape(.rect(cornerRadius: 10))
                }
                .buttonStyle(.plain)
                .accessibilityAddTraits(selection == os ? .isSelected : [])
            }
        }
    }
}
