#!/usr/bin/env python3
"""Pack square PNG files into a Windows .ico (PNG-compressed entries, Vista and later).

    make-ico.py out.ico a.png b.png ...

Standard library only. Each PNG must be square and at most 256 px; its size is read from
the PNG header.
"""

import struct
import sys

PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"


def png_size(data: bytes, path: str) -> int:
    if data[:8] != PNG_SIGNATURE or data[12:16] != b"IHDR":
        sys.exit(f"{path}: not a PNG")
    width, height = struct.unpack(">II", data[16:24])
    if width != height or not 1 <= width <= 256:
        sys.exit(f"{path}: expected a square image of at most 256 px, got {width}x{height}")
    return width


def main() -> None:
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    out, inputs = sys.argv[1], sys.argv[2:]
    images = []
    for path in inputs:
        with open(path, "rb") as f:
            data = f.read()
        images.append((png_size(data, path), data))
    images.sort(key=lambda image: image[0])

    # ICONDIR: reserved, type 1 (icon), count. Then one 16-byte ICONDIRENTRY per image,
    # then the image data.
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = len(header) + 16 * len(images)
    entries = b""
    for size, data in images:
        # Width and height are one byte each, where 0 means 256.
        entries += struct.pack(
            "<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(data), offset
        )
        offset += len(data)
    with open(out, "wb") as f:
        f.write(header + entries + b"".join(data for _, data in images))


if __name__ == "__main__":
    main()
