import AppKit
import ManagerCore
import SwiftUI
import UniformTypeIdentifiers

enum FileKind {
    static let cdImage: [UTType] = [UTType(filenameExtension: "iso"), UTType(filenameExtension: "cdr")].compactMap {
        $0
    }
    static let any: [UTType] = []
}

@MainActor
enum FilePicker {
    static func open(kinds: [UTType], message: String? = nil, directory: Bool = false) -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseFiles = !directory
        panel.canChooseDirectories = directory
        panel.allowsMultipleSelection = false
        if !kinds.isEmpty { panel.allowedContentTypes = kinds }
        if let message { panel.message = message }
        return panel.runModal() == .OK ? panel.url : nil
    }
}

struct FileField: View {
    let title: String
    @Binding var path: String?
    var placeholder = "None"
    var kinds: [UTType] = FileKind.any
    var message: String?
    var detail: String?

    var body: some View {
        LabeledContent(title) {
            HStack {
                Text(shown ?? placeholder)
                    .foregroundStyle(shown == nil ? .secondary : .primary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(path ?? placeholder)
                if shown != nil, let detail {
                    Text(detail)
                        .foregroundStyle(.secondary)
                        .fixedSize()
                }
                Spacer(minLength: 0)
                Button("Choose…") {
                    if let url = FilePicker.open(kinds: kinds, message: message) { path = url.path }
                }
                if shown != nil {
                    Button("Clear", systemImage: "xmark.circle.fill") { path = nil }
                        .labelStyle(.iconOnly)
                        .buttonStyle(.borderless)
                        .help("Clear")
                }
            }
        }
    }

    private var shown: String? {
        guard let path, !path.isEmpty else { return nil }
        return URL(fileURLWithPath: path).lastPathComponent
    }
}

extension Binding where Value == String {
    var optional: Binding<String?> {
        Binding<String?>(get: { wrappedValue.isEmpty ? nil : wrappedValue }, set: { wrappedValue = $0 ?? "" })
    }
}
