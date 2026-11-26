#!/usr/bin/env python3
"""Pack PNG files into a Windows .ico (PNG-compressed entries, Vista+).
    python3 scripts/png2ico.py out.ico 16.png 32.png 48.png 256.png"""
import struct, sys

out, pngs = sys.argv[1], sys.argv[2:]
images = []
for p in pngs:
    data = open(p, "rb").read()
    w, h = struct.unpack(">II", data[16:24])  # IHDR
    images.append((w, h, data))
header = struct.pack("<HHH", 0, 1, len(images))
offset = 6 + 16 * len(images)
entries, blobs = b"", b""
for w, h, data in images:
    entries += struct.pack("<BBBBHHII", w % 256, h % 256, 0, 0, 1, 32, len(data), offset + len(blobs))
    blobs += data
open(out, "wb").write(header + entries + blobs)
