// Compose a review sheet of icon candidates with AppKit (macOS 13 or later).
//
//   contact-sheet <out.png> <prefix>[=<label>] ...
//
// For every prefix it reads <prefix>-{256,64,32,16}.png, as written by
// render-svg.swift (for example png/pulse-s=Pulse S reads png/pulse-s-256.png), and lays them out in rows on a dark and a light panel side by
// side: 256 px, then 64, 32 and 16 px at 1:1, then the 32 px render blown up 4x
// with nearest-neighbour sampling so individual pixels can be judged.
import AppKit

let args = CommandLine.arguments
guard args.count >= 3 else {
    FileHandle.standardError.write(Data("usage: contact-sheet out.png prefix[=label] ...\n".utf8))
    exit(2)
}
let entries: [(name: String, label: String)] = args[2...].map { arg in
    let parts = arg.split(separator: "=", maxSplits: 1).map(String.init)
    return (parts[0], parts.count > 1 ? parts[1] : parts[0])
}

func load(_ name: String, _ size: Int) -> NSBitmapImageRep {
    let url = URL(fileURLWithPath: "\(name)-\(size).png")
    guard let data = try? Data(contentsOf: url), let rep = NSBitmapImageRep(data: data) else {
        FileHandle.standardError.write(Data("cannot load \(url.path)\n".utf8))
        exit(1)
    }
    return rep
}

let pad = 40.0, rowHeight = 300.0, header = 70.0
let panelWidth = 820.0
let width = Int(panelWidth * 2), height = Int(header + Double(entries.count) * rowHeight + pad / 2)

guard let sheet = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height, bitsPerSample: 8,
    samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
    bytesPerRow: 0, bitsPerPixel: 0)
else { exit(1) }
sheet.size = NSSize(width: width, height: height)

NSGraphicsContext.saveGraphicsState()
let context = NSGraphicsContext(bitmapImageRep: sheet)!
NSGraphicsContext.current = context
// Draw in a top-left origin to keep the layout arithmetic readable.
context.cgContext.translateBy(x: 0, y: CGFloat(height))
context.cgContext.scaleBy(x: 1, y: -1)

func text(_ s: String, _ x: Double, _ y: Double, size: CGFloat, weight: NSFont.Weight, color: NSColor) {
    let attrs: [NSAttributedString.Key: Any] = [
        .font: NSFont.systemFont(ofSize: size, weight: weight), .foregroundColor: color,
    ]
    NSGraphicsContext.saveGraphicsState()
    let t = NSAffineTransform()
    t.translateX(by: x, yBy: y + Double(size))
    t.scaleX(by: 1, yBy: -1)
    t.concat()
    NSString(string: s).draw(at: .zero, withAttributes: attrs)
    NSGraphicsContext.restoreGraphicsState()
}

func image(_ rep: NSBitmapImageRep, _ x: Double, _ y: Double, scale: Double = 1) {
    let w = Double(rep.pixelsWide) * scale, h = Double(rep.pixelsHigh) * scale
    NSGraphicsContext.saveGraphicsState()
    let t = NSAffineTransform()
    t.translateX(by: x, yBy: y + h)
    t.scaleX(by: 1, yBy: -1)
    t.concat()
    context.imageInterpolation = scale == 1 ? .high : .none
    rep.draw(
        in: NSRect(x: 0, y: 0, width: w, height: h), from: .zero, operation: .sourceOver,
        fraction: 1, respectFlipped: false,
        hints: [.interpolation: NSNumber(value: (scale == 1 ? NSImageInterpolation.high : .none).rawValue)])
    NSGraphicsContext.restoreGraphicsState()
}

let panels: [(bg: NSColor, fg: NSColor, dim: NSColor, title: String)] = [
    (NSColor(srgbRed: 0.11, green: 0.12, blue: 0.14, alpha: 1), .white,
     NSColor(white: 0.62, alpha: 1), "Dark"),
    (NSColor(srgbRed: 0.93, green: 0.94, blue: 0.95, alpha: 1), NSColor(white: 0.08, alpha: 1),
     NSColor(white: 0.40, alpha: 1), "Light"),
]

for (p, panel) in panels.enumerated() {
    let ox = Double(p) * panelWidth
    panel.bg.set()
    NSRect(x: ox, y: 0, width: panelWidth, height: Double(height)).fill()
    text("Serialist icon candidates · \(panel.title) · macOS inset, rendered by render-svg.swift",
         ox + pad, 24, size: 18, weight: .medium, color: panel.dim)

    for (i, entry) in entries.enumerated() {
        let y = header + Double(i) * rowHeight
        image(load(entry.name, 256), ox + pad, y)
        let tx = ox + pad + 256 + 36
        text(entry.label, tx, y + 18, size: 26, weight: .semibold, color: panel.fg)

        // 64, 32 and 16 px at 1:1, bottom-aligned, with captions.
        let base = y + 150.0
        var x = tx
        for size in [64, 32, 16] {
            image(load(entry.name, size), x, base - Double(size))
            text("\(size)", x, base + 8, size: 14, weight: .regular, color: panel.dim)
            x += Double(size) + 28
        }
        // 32 px at 4x, nearest neighbour.
        let zx = x + 20
        image(load(entry.name, 32), zx, y + 86, scale: 4)
        text("32 px ×4", zx, y + 86 + 128 + 8, size: 14, weight: .regular, color: panel.dim)
    }
}
NSGraphicsContext.restoreGraphicsState()

guard let png = sheet.representation(using: .png, properties: [:]) else { exit(1) }
try png.write(to: URL(fileURLWithPath: args[1]))
