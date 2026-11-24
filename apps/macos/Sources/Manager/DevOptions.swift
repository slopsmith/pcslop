import AppKit
import ScreenCaptureKit

@MainActor
enum Snapshot {
    static func takeIfAsked() async {
        if let appearance = ManagerApp.argument("--appearance") {
            NSApp.appearance = NSAppearance(named: appearance == "dark" ? .darkAqua : .aqua)
        }
        let window = NSApp.windows.first { $0.isVisible }
        if let size = ManagerApp.argument("--window")?.split(separator: "x").compactMap({ Double($0) }),
            size.count == 2
        {
            window?.setContentSize(NSSize(width: size[0], height: size[1]))
            window?.center()
        }
        guard let path = ManagerApp.argument("--snapshot"), var window else { return }
        try? await Task.sleep(for: .seconds(2))
        if ManagerApp.argument("--open") == "preferences",
            let settings = NSApp.windows.first(where: { $0.isVisible && $0 !== window })
        {
            window = settings
        }
        let url = URL(fileURLWithPath: path)
        do {
            try await capture(window, to: url)
        } catch {
            FileHandle.standardError.write(
                Data("window capture failed (\(error.localizedDescription)); drawing without glass\n".utf8))
            drawWithoutWindowServer(window, to: url)
        }
        await escapeSheets()
        NSApp.terminate(nil)
    }

    // AppKit will not quit with a sheet open, so press Escape in it the way a person would.
    private static func escapeSheets() async {
        for _ in 0..<20 {
            guard let sheet = NSApp.windows.lazy.compactMap(\.attachedSheet).first else { return }
            sheet.makeKey()
            if let escape = NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime,
                windowNumber: sheet.windowNumber, context: nil, characters: "\u{1b}",
                charactersIgnoringModifiers: "\u{1b}", isARepeat: false, keyCode: 53)
            {
                NSApp.postEvent(escape, atStart: false)
            }
            try? await Task.sleep(for: .milliseconds(300))
        }
    }

    private static func drawWithoutWindowServer(_ window: NSWindow, to url: URL) {
        guard let view = window.contentView?.superview,
            let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds)
        else { return }
        view.cacheDisplay(in: view.bounds, to: rep)
        try? rep.representation(using: .png, properties: [:])?.write(to: url)
    }

    private static func capture(_ window: NSWindow, to url: URL) async throws {
        let content = try await SCShareableContent.currentProcess
        guard let target = content.windows.first(where: { $0.windowID == CGWindowID(window.windowNumber) }) else {
            throw CocoaError(.fileNoSuchFile)
        }
        let filter = SCContentFilter(desktopIndependentWindow: target)
        let config = SCStreamConfiguration()
        let scale = window.backingScaleFactor
        config.width = Int(filter.contentRect.width * scale)
        config.height = Int(filter.contentRect.height * scale)
        config.showsCursor = false
        let image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: config)
        let rep = NSBitmapImageRep(cgImage: image)
        guard let png = rep.representation(using: .png, properties: [:]) else {
            throw CocoaError(.fileWriteUnknown)
        }
        try png.write(to: url)
    }
}
