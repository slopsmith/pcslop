// Draws the app icon into an .iconset folder: xcrun swift make-icon.swift OUT.iconset
import AppKit

let out = URL(fileURLWithPath: CommandLine.arguments.dropFirst().first ?? "AppIcon.iconset")
try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)

func draw(_ px: Int) -> Data? {
    let s = CGFloat(px)
    guard
        let rep = NSBitmapImageRep(
            bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
            samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
            bytesPerRow: 0, bitsPerPixel: 0)
    else { return nil }
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

    let inset = s * 0.1
    let body = NSRect(x: inset, y: inset, width: s - 2 * inset, height: s - 2 * inset)
    let w = body.width
    let shape = NSBezierPath(roundedRect: body, xRadius: w * 0.225, yRadius: w * 0.225)
    NSGradient(
        starting: NSColor(calibratedRed: 0.16, green: 0.72, blue: 0.68, alpha: 1),
        ending: NSColor(calibratedRed: 0.20, green: 0.22, blue: 0.62, alpha: 1))?
        .draw(in: shape, angle: -60)

    let screen = NSRect(x: body.minX + w * 0.16, y: body.minY + w * 0.30, width: w * 0.68, height: w * 0.50)
    let bezel = NSBezierPath(roundedRect: screen, xRadius: w * 0.06, yRadius: w * 0.06)
    NSColor.white.withAlphaComponent(0.92).setFill()
    bezel.fill()
    let glass = NSBezierPath(
        roundedRect: screen.insetBy(dx: w * 0.035, dy: w * 0.035), xRadius: w * 0.035, yRadius: w * 0.035)
    NSColor(calibratedRed: 0.08, green: 0.10, blue: 0.22, alpha: 1).setFill()
    glass.fill()

    let stand = NSBezierPath()
    stand.move(to: NSPoint(x: body.midX - w * 0.07, y: screen.minY))
    stand.line(to: NSPoint(x: body.midX + w * 0.07, y: screen.minY))
    stand.line(to: NSPoint(x: body.midX + w * 0.12, y: body.minY + w * 0.17))
    stand.line(to: NSPoint(x: body.midX - w * 0.12, y: body.minY + w * 0.17))
    stand.close()
    NSColor.white.withAlphaComponent(0.92).setFill()
    stand.fill()

    let text = NSAttributedString(
        string: "C:\\>_",
        attributes: [
            .font: NSFont.monospacedSystemFont(ofSize: w * 0.13, weight: .bold),
            .foregroundColor: NSColor(calibratedRed: 0.45, green: 1.0, blue: 0.75, alpha: 1),
        ])
    let size = text.size()
    text.draw(at: NSPoint(x: screen.midX - size.width / 2, y: screen.midY - size.height / 2))

    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])
}

for points in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let name = scale == 1 ? "icon_\(points)x\(points).png" : "icon_\(points)x\(points)@2x.png"
        guard let png = draw(points * scale) else { fatalError("cannot draw \(name)") }
        try png.write(to: out.appendingPathComponent(name))
    }
}
