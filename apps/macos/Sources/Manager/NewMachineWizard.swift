import ManagerCore
import SwiftUI

extension Preset {
    var title: String {
        switch self {
        case .windowsXPRadeon: "Windows XP"
        case .ubuntuRadeon: "Ubuntu"
        case .custom: "Custom"
        }
    }

    func blurb(radeon: Bool) -> String {
        switch self {
        case .windowsXPRadeon: radeon ? "Install or run Windows XP, with 3D on the Radeon 9700 Pro." : "Install or run Windows XP."
        case .ubuntuRadeon: radeon ? "Install Ubuntu to a hard disk, with 3D on the Radeon 9700 Pro." : "Install Ubuntu to a hard disk."
        case .custom: "Start from plain defaults and set up every device yourself."
        }
    }

    func hardware(radeon: Bool) -> String {
        machine(name: "", disk: nil, cdrom: nil, radeon: radeon).config.summary
    }
}

private enum WizardStep: Int, CaseIterable {
    case preset, drives, finish

    var title: String {
        switch self {
        case .preset: "Choose a Starting Point"
        case .drives: "Disks"
        case .finish: "Name Your Machine"
        }
    }
}

private enum DiskChoice: Hashable {
    case new, existing, none
}

struct NewMachineWizard: View {
    let created: (Machine) -> Void
    @Environment(Library.self) private var library
    @Environment(\.dismiss) private var dismiss
    @State private var step = WizardStep.preset
    @State private var preset: Preset
    @State private var diskChoice: DiskChoice
    @State private var diskGiB: UInt64
    @State private var existingDisk: String?
    @State private var cdrom: String?
    @AppStorage(Defaults.radeonROM) private var radeonROM = ""
    @State private var name: String
    @State private var error: String?

    init(request: NewMachineRequest, created: @escaping (Machine) -> Void) {
        let preset = request.preset ?? .windowsXPRadeon
        self.created = created
        _preset = State(initialValue: preset)
        _diskChoice = State(initialValue: request.disk == nil ? .new : .existing)
        _diskGiB = State(initialValue: preset.diskGiB)
        _existingDisk = State(initialValue: request.disk)
        _cdrom = State(initialValue: request.cdrom)
        let fromFile = (request.disk ?? request.cdrom).map { URL(fileURLWithPath: $0).deletingPathExtension().lastPathComponent }
        _name = State(initialValue: fromFile ?? preset.title)
        if request.preset != nil { _step = State(initialValue: .drives) }
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            Group {
                switch step {
                case .preset: presets
                case .drives: drives
                case .finish: finish
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .transition(.asymmetric(insertion: .move(edge: .trailing), removal: .move(edge: .leading)).combined(with: .opacity))
            Divider()
            footer
        }
        .frame(width: 660, height: 500)
        .animation(.spring(duration: 0.35), value: step)
        .alert(
            "Cannot make the machine",
            isPresented: Binding(get: { error != nil }, set: { if !$0 { error = nil } })
        ) {
            Button("OK") {}
        } message: {
            Text(error ?? "")
        }
    }

    private var header: some View {
        HStack {
            Text(step.title)
                .font(.title2.weight(.semibold))
            Spacer()
            HStack(spacing: 6) {
                ForEach(WizardStep.allCases, id: \.self) { s in
                    Capsule()
                        .fill(s.rawValue <= step.rawValue ? Color.accentColor : Color.secondary.opacity(0.3))
                        .frame(width: s == step ? 22 : 8, height: 8)
                }
            }
            .accessibilityElement()
            .accessibilityLabel("Step \(step.rawValue + 1) of \(WizardStep.allCases.count)")
        }
        .padding(.horizontal, 22)
        .padding(.vertical, 16)
    }

    private var presets: some View {
        VStack(spacing: 12) {
            ForEach(Preset.allCases) { p in
                PresetCard(preset: p, selected: p == preset, radeon: hasRadeon)
                    .onTapGesture(count: 2) {
                        choose(p)
                        advance()
                    }
                    .onTapGesture { choose(p) }
            }
            Spacer(minLength: 0)
            if !hasRadeon {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Image(systemName: "info.circle")
                    Text("Windows XP and Ubuntu get 3D acceleration once you choose a Radeon 9700 Pro video BIOS in Settings.")
                    Spacer(minLength: 8)
                    SettingsLink { Text("Open Settings…") }
                }
                .font(.callout)
                .foregroundStyle(.secondary)
            }
        }
        .padding(22)
    }

    private var drives: some View {
        Form {
            Section {
                Picker("Hard disk", selection: $diskChoice) {
                    Text("Make a new disk").tag(DiskChoice.new)
                    Text("Use an existing disk image").tag(DiskChoice.existing)
                    Text("No hard disk").tag(DiskChoice.none)
                }
                .pickerStyle(.radioGroup)
                switch diskChoice {
                case .new:
                    LabeledContent("Size") {
                        HStack {
                            Slider(
                                value: Binding(get: { Double(diskGiB) }, set: { diskGiB = UInt64($0.rounded()) }),
                                in: Double(DiskSize.range.lowerBound)...64, step: 1)
                            Text("\(diskGiB) GB").monospacedDigit().frame(width: 52, alignment: .trailing)
                        }
                    }
                case .existing:
                    FileField(title: "Disk image", path: $existingDisk, placeholder: "Required", message: "Choose a raw disk image.", detail: fileSize(existingDisk))
                case .none:
                    EmptyView()
                }
            } footer: {
                if diskChoice == .new {
                    Text("A raw image in the machine's folder. It only takes space as the guest fills it.")
                }
            }
            Section {
                FileField(
                    title: "Install CD", path: $cdrom, placeholder: "None", kinds: FileKind.cdImage,
                    message: "Choose the installation CD image.")
            } footer: {
                Text(preset == .ubuntuRadeon
                     ? "The Ubuntu 8.04 desktop CD installs to the disk from its live session."
                     : "The machine boots from the CD while the disk is blank.")
            }
        }
        .formStyle(.grouped)
        .scrollContentBackground(.hidden)
    }

    private var finish: some View {
        Form {
            Section {
                TextField("Name", text: $name)
            }
            Section("Summary") {
                LabeledContent("Starting point", value: preset.title)
                LabeledContent("Hardware", value: preset.hardware(radeon: hasRadeon))
                LabeledContent("Hard disk", value: diskSummary)
                LabeledContent("CD-ROM", value: fileName(cdrom) ?? "Empty")
            }
        }
        .formStyle(.grouped)
        .scrollContentBackground(.hidden)
    }

    private var hasRadeon: Bool { !radeonROM.isEmpty }

    private var diskSummary: String {
        switch diskChoice {
        case .new: "New, \(diskGiB) GB"
        case .existing: fileName(existingDisk) ?? "Missing"
        case .none: "None"
        }
    }

    private var footer: some View {
        HStack {
            Button("Cancel", role: .cancel) { dismiss() }
                .keyboardShortcut(.cancelAction)
            Spacer()
            if step != .preset {
                Button("Back") { step = WizardStep(rawValue: step.rawValue - 1)! }
            }
            if step == .finish {
                Button("Create", action: create)
                    .buttonStyle(.glassProminent)
                    .keyboardShortcut(.defaultAction)
                    .disabled(name.trimmingCharacters(in: .whitespaces).isEmpty)
            } else {
                Button("Continue", action: advance)
                    .buttonStyle(.glassProminent)
                    .keyboardShortcut(.defaultAction)
                    .disabled(!canAdvance)
            }
        }
        .padding(14)
    }

    private var canAdvance: Bool {
        guard step == .drives else { return true }
        if diskChoice == .existing, existingDisk?.isEmpty ?? true { return false }
        return true
    }

    private func choose(_ p: Preset) {
        if name == preset.title { name = p.title }
        if diskGiB == preset.diskGiB { diskGiB = p.diskGiB }
        preset = p
    }

    private func advance() {
        if let next = WizardStep(rawValue: step.rawValue + 1) { step = next }
    }

    private func create() {
        var machine = preset.machine(
            name: name.trimmingCharacters(in: .whitespaces),
            disk: diskChoice == .existing ? existingDisk : nil,
            cdrom: cdrom, radeon: hasRadeon)
        if diskChoice == .new {
            let folder = library.store.folder(of: machine.id)
            let disk = folder.appendingPathComponent("disk.img")
            do {
                try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
                try DiskImage.createBlank(at: disk, mebibytes: diskGiB * 1024)
            } catch {
                self.error = error.localizedDescription
                return
            }
            machine.config.hda = disk.path
        }
        library.save(machine)
        created(machine)
        dismiss()
    }
}

private struct PresetCard: View {
    let preset: Preset
    let selected: Bool
    let radeon: Bool
    @State private var hovering = false

    var body: some View {
        HStack(spacing: 16) {
            OSIcon(os: preset.os, size: 52)
            VStack(alignment: .leading, spacing: 4) {
                Text(preset.title).font(.title3.weight(.semibold))
                Text(preset.blurb(radeon: radeon)).foregroundStyle(.secondary)
                Text(preset.hardware(radeon: radeon))
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
            Spacer()
            Image(systemName: selected ? "checkmark.circle.fill" : "circle")
                .font(.title2)
                .foregroundStyle(selected ? Color.accentColor : Color.secondary.opacity(0.5))
                .contentTransition(.symbolEffect(.replace))
        }
        .padding(16)
        .contentShape(.rect(cornerRadius: 16))
        .background(
            selected ? Color.accentColor.opacity(0.12) : hovering ? Color.primary.opacity(0.04) : .clear,
            in: .rect(cornerRadius: 16, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .strokeBorder(selected ? Color.accentColor : Color.secondary.opacity(0.25), lineWidth: selected ? 2 : 1)
        }
        .onHover { hovering = $0 }
        .animation(.spring(duration: 0.25), value: selected)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(selected ? [.isButton, .isSelected] : .isButton)
    }
}
