// Draws the Statusline Tray app icon and writes an `.iconset` folder, which
// `iconutil -c icns` turns into AppIcon.icns.
//
//   swift make-icon.swift <output.iconset>
//
// A terminal prompt over a usage meter, on a dark squircle. Drawn in a
// 1024-unit design space (y down, like the SVG it was sketched in) and scaled
// to every size macOS asks for.

import AppKit

func rounded(_ x: Double, _ y: Double, _ w: Double, _ h: Double, _ r: Double) -> CGPath {
    CGPath(roundedRect: CGRect(x: x, y: y, width: w, height: h), cornerWidth: r, cornerHeight: r, transform: nil)
}

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    CGColor(
        red: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255,
        blue: CGFloat(hex & 0xff) / 255, alpha: alpha)
}

func gradient(_ stops: [(CGFloat, UInt32)]) -> CGGradient {
    CGGradient(
        colorsSpace: CGColorSpaceCreateDeviceRGB(), colors: stops.map { color($0.1) } as CFArray,
        locations: stops.map { $0.0 })!
}

func draw(_ ctx: CGContext) {
    // Tile.
    ctx.saveGState()
    ctx.addPath(rounded(100, 100, 824, 824, 185))
    ctx.clip()
    ctx.drawLinearGradient(gradient([(0, 0x2a2f45), (1, 0x12141f)]), start: CGPoint(x: 0, y: 100), end: CGPoint(x: 0, y: 924), options: [])
    ctx.restoreGState()

    // Prompt: a chevron and a cursor.
    ctx.setStrokeColor(color(0xe8eaf6))
    ctx.setLineWidth(64)
    ctx.setLineCap(.round)
    ctx.setLineJoin(.round)
    ctx.move(to: CGPoint(x: 290, y: 320))
    ctx.addLine(to: CGPoint(x: 435, y: 408))
    ctx.addLine(to: CGPoint(x: 290, y: 496))
    ctx.strokePath()
    ctx.setFillColor(color(0xe8eaf6, 0.92))
    ctx.addPath(rounded(490, 440, 240, 64, 32))
    ctx.fillPath()

    // Meter: a dark track with a part-filled, green-to-amber bar.
    ctx.setFillColor(color(0x0b0d16))
    ctx.addPath(rounded(200, 640, 624, 96, 48))
    ctx.fillPath()
    ctx.saveGState()
    ctx.addPath(rounded(216, 656, 360, 64, 32))
    ctx.clip()
    ctx.drawLinearGradient(
        gradient([(0, 0x4ade80), (0.6, 0xfacc15), (1, 0xfb923c)]), start: CGPoint(x: 216, y: 0), end: CGPoint(x: 576, y: 0), options: [])
    ctx.restoreGState()
}

func png(size: Int) -> Data {
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8, samplesPerPixel: 4,
        hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    let ctx = NSGraphicsContext(bitmapImageRep: rep)!.cgContext
    ctx.interpolationQuality = .high
    // Design space is y-down; Core Graphics is y-up.
    ctx.translateBy(x: 0, y: CGFloat(size))
    ctx.scaleBy(x: CGFloat(size) / 1024, y: -CGFloat(size) / 1024)
    draw(ctx)
    return rep.representation(using: .png, properties: [:])!
}

guard CommandLine.arguments.count == 2 else {
    FileHandle.standardError.write("usage: swift make-icon.swift <output.iconset>\n".data(using: .utf8)!)
    exit(2)
}
let out = CommandLine.arguments[1]
try? FileManager.default.createDirectory(atPath: out, withIntermediateDirectories: true)
for base in [16, 32, 128, 256, 512] {
    try! png(size: base).write(to: URL(fileURLWithPath: "\(out)/icon_\(base)x\(base).png"))
    try! png(size: base * 2).write(to: URL(fileURLWithPath: "\(out)/icon_\(base)x\(base)@2x.png"))
}
