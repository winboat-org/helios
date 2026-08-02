#!/usr/bin/env python3
"""Grab ONE frame off QEMU's VNC surface and write it as a PNG (no deps).

  usage: vnc_shot.py [--host 127.0.0.1] [--port 5900] [--out shot.png]
                     [--exclusive] [--settle 0.5]

QMP `screendump` answers "no surface" whenever the console's scanout kind is
DMABUF (which is always, under egl-vnc and sdl,gl=on), so this is the only way
to see what the guest is actually showing. Deliberately dependency-free -- pure
zlib/struct -- so it works on a box with no numpy/pillow.

⚠ Do NOT send RFB SetPixelFormat: QEMU then stops answering
FramebufferUpdateRequests entirely, silently. Its native format is already
32bpp little-endian B,G,R,X, which is what this decodes.
"""
import argparse
import socket
import struct
import sys
import time
import zlib


def recvall(sock, n):
    buf = bytearray()
    while len(buf) < n:
        c = sock.recv(n - len(buf))
        if not c:
            raise EOFError("server closed")
        buf += c
    return bytes(buf)


class RFB:
    def __init__(self, host, port, shared=1):
        self.s = socket.create_connection((host, port), timeout=10)
        ver = recvall(self.s, 12)
        if not ver.startswith(b"RFB "):
            raise RuntimeError("not an RFB server: %r" % ver)
        self.s.sendall(b"RFB 003.008\n")
        n = recvall(self.s, 1)[0]
        sec = recvall(self.s, n)
        if 1 not in sec:
            raise RuntimeError("server needs auth: %r" % list(sec))
        self.s.sendall(bytes([1]))
        if struct.unpack(">I", recvall(self.s, 4))[0] != 0:
            raise RuntimeError("security handshake failed")
        self.s.sendall(bytes([shared]))
        # ServerInit: 2+2 size, 16 pixel-format, 4 name-length.  Short-reading
        # the pixel format desynchronises the stream and every rect then decodes
        # as garbage.
        hdr = recvall(self.s, 24)
        self.w, self.h = struct.unpack(">HH", hdr[:4])
        nlen = struct.unpack(">I", hdr[20:24])[0]
        self.name = recvall(self.s, nlen).decode("latin1")
        # Raw(0) + DesktopSize(-223): no decoder to get wrong.
        encs = [0, -223]
        self.s.sendall(struct.pack(">BxH", 2, len(encs)) +
                       b"".join(struct.pack(">i", e) for e in encs))
        self.fb = bytearray(self.w * self.h * 4)

    def request(self, incremental=0):
        self.s.sendall(struct.pack(">BBHHHH", 3, incremental, 0, 0,
                                   self.w, self.h))

    def read_update(self):
        while True:
            msg = recvall(self.s, 1)[0]
            if msg == 0:
                break
            elif msg == 1:                        # SetColourMapEntries
                recvall(self.s, 3)
                n = struct.unpack(">H", recvall(self.s, 2)[0:2])[0]
                recvall(self.s, n * 6)
            elif msg == 2:                        # Bell
                pass
            elif msg == 3:                        # ServerCutText
                recvall(self.s, 3)
                n = struct.unpack(">I", recvall(self.s, 4))[0]
                recvall(self.s, n)
            else:
                raise RuntimeError("unexpected server message %d" % msg)
        recvall(self.s, 1)
        nrects = struct.unpack(">H", recvall(self.s, 2))[0]
        for _ in range(nrects):
            x, y, w, h, enc = struct.unpack(">HHHHi", recvall(self.s, 12))
            if enc == -223:                       # DesktopSize pseudo-encoding
                self.w, self.h = w, h
                self.fb = bytearray(self.w * self.h * 4)
                continue
            if enc != 0:
                raise RuntimeError("unexpected encoding %d" % enc)
            data = recvall(self.s, w * h * 4)
            for row in range(h):
                off = ((y + row) * self.w + x) * 4
                self.fb[off:off + w * 4] = data[row * w * 4:(row + 1) * w * 4]


def write_png(path, w, h, fb):
    """B,G,R,X framebuffer -> 8-bit RGB PNG."""
    raw = bytearray()
    for y in range(h):
        raw.append(0)                             # filter: none
        row = fb[y * w * 4:(y + 1) * w * 4]
        raw += bytes(b for i in range(0, len(row), 4)
                     for b in (row[i + 2], row[i + 1], row[i]))

    def chunk(tag, data):
        return (struct.pack(">I", len(data)) + tag + data +
                struct.pack(">I", zlib.crc32(tag + data) & 0xffffffff))

    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)))
        f.write(chunk(b"IDAT", zlib.compress(bytes(raw), 6)))
        f.write(chunk(b"IEND", b""))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=5900)
    ap.add_argument("--out", default="shot.png")
    ap.add_argument("--exclusive", action="store_true")
    ap.add_argument("--settle", type=float, default=0.5,
                    help="seconds of updates to absorb before saving")
    a = ap.parse_args()

    r = RFB(a.host, a.port, shared=0 if a.exclusive else 1)
    sys.stderr.write("connected %dx%d name=%r\n" % (r.w, r.h, r.name))
    r.request(incremental=0)
    r.read_update()
    t_end = time.time() + a.settle
    r.s.settimeout(1.0)
    while time.time() < t_end:
        try:
            r.request(incremental=1)
            r.read_update()
        except socket.timeout:
            break
    write_png(a.out, r.w, r.h, r.fb)
    lit = sum(1 for i in range(0, len(r.fb), 4 * 64)
              if r.fb[i] or r.fb[i + 1] or r.fb[i + 2])
    total = len(r.fb) // (4 * 64)
    sys.stderr.write("wrote %s (%dx%d, %d/%d sampled pixels lit)\n"
                     % (a.out, r.w, r.h, lit, total))


if __name__ == "__main__":
    main()
