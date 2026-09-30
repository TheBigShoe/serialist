// Rasterize an SVG to a square PNG with AppKit (macOS 13 or later).
//
//   render-svg <in.svg> <out.png> <pixels> [inset]
//
// `inset` is the transparent margin on each side as a fraction of the canvas, so the
// artwork is drawn at (1 - 2 * inset) of the size and centered. Only render.sh uses
// this; nothing here ships in a release.
import AppKit

let args = CommandLine.arguments
guard args.count >= 4, let pixels = Int(args[3]), pixels > 0 else {
    FileHandle.standardError.write(Data("usage: render-svg in.svg out.png pixels [inset]\n".utf8))
    exit(2)
}
let inset = args.count > 4 ? (Double(args[4]) ?? 0) : 0

guard let image = NSImage(contentsOf: URL(fileURLWithPath: args[1])) else {
    FileHandle.standardError.write(Data("cannot load \(args[1])\n".utf8))
    exit(1)
}
guard let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels, bitsPerSample: 8,
    samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
    bytesPerRow: 0, bitsPerPixel: 0)
else { exit(1) }
rep.size = NSSize(width: pixels, height: pixels)

NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
NSGraphicsContext.current?.imageInterpolation = .high
NSColor.clear.set()
NSRect(x: 0, y: 0, width: pixels, height: pixels).fill(using: .copy)
let side = Double(pixels) * (1 - 2 * inset)
let origin = (Double(pixels) - side) / 2
image.draw(
    in: NSRect(x: origin, y: origin, width: side, height: side), from: .zero,
    operation: .sourceOver, fraction: 1)
NSGraphicsContext.restoreGraphicsState()

guard let png = rep.representation(using: .png, properties: [:]) else { exit(1) }
try png.write(to: URL(fileURLWithPath: args[2]))
