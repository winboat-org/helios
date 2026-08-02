#!/usr/bin/env python3
"""Sample what the guest is actually DISPLAYING, off the QEMU VNC surface.

This is the host-side half of the defect-0ab instrument (the guest-side half is
`kmd_render/src/ddi/scanout_trace.rs`). It connects to QEMU's RFB server, pulls
incremental framebuffer updates as fast as the server will produce them (~30/s,
QEMU's refresh floor), and records for EVERY update:

  * `t`      -- CLOCK_REALTIME at receipt. The SAME CLOCK as the QEMU trace log,
                whose `log` backend prefixes every `virtio_gpu_cmd_*` line with a
                UTC timestamp, so a displayed frame can be attributed to a
                specific RESOURCE_FLUSH. That correlation is what named 0ab;
                seven rounds of inference did not.
  * `hud`    -- mean brightness of a rectangle that is bright in every COMPLETED
                application frame (3DMark's fps bar). This is the oracle: it
                separates "the app rendered a dark scene" from "we displayed a
                frame the app had not finished", which whole-frame brightness
                cannot do.
  * `mean` / `dark` / `p95` / the update's rect list.

Unfinished frames and their neighbours are written out as PNGs so the artifact
can be LOOKED AT. Two traps paid for in the 58th session:

  * Do NOT send SetPixelFormat. QEMU's native format is already 32bpp
    B,G,R,X -- and this sampler once hung forever with no data after sending one.
  * Writing PNGs inside the sample loop throttled it to 3/s and destroyed the
    temporal resolution the whole measurement depends on. They are deferred to
    the end of the run.

Requires numpy + pillow (a venv is fine; nothing here is guest-side).

  python3 tools/vnc_frame_probe.py --seconds 170 --out cap --exclusive \\
      --hudthresh 40 --dark 0.0

`--exclusive` disconnects any other viewer: QEMU's default share policy refuses
a SHARED client while an exclusive one is connected, and most viewers are
exclusive.
"""
import argparse
import json
import os
import socket
import struct
import sys
import time

import numpy as np
from PIL import Image


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
        self.shared = shared
        self.s = socket.create_connection((host, port))
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        ver = recvall(self.s, 12)
        if not ver.startswith(b"RFB "):
            raise RuntimeError("not RFB: %r" % ver)
        self.s.sendall(b"RFB 003.008\n")
        n = recvall(self.s, 1)[0]
        if n == 0:
            reason_len = struct.unpack(">I", recvall(self.s, 4))[0]
            raise RuntimeError(recvall(self.s, reason_len).decode())
        types = recvall(self.s, n)
        if 1 not in types:
            raise RuntimeError("no None auth; offered %r" % (list(types),))
        self.s.sendall(bytes([1]))
        res = struct.unpack(">I", recvall(self.s, 4))[0]
        if res != 0:
            raise RuntimeError("auth failed %d" % res)
        self.s.sendall(bytes([self.shared]))  # ClientInit
        hdr = recvall(self.s, 24)
        self.w, self.h = struct.unpack(">HH", hdr[:4])
        namelen = struct.unpack(">I", hdr[20:24])[0]
        self.name = recvall(self.s, namelen).decode("latin1")
        # Keep the server's native pixel format (QEMU: 32bpp, R<<16 G<<8 B,
        # i.e. byte order B,G,R,X).  Sending SetPixelFormat makes QEMU stop
        # answering FramebufferUpdateRequests entirely, so do not send one.
        self.pf = hdr[4:20]
        assert self.pf[:4] == bytes([32, 24, 0, 1]) and self.pf[10:13] == bytes([16, 8, 0]), \
            "unexpected server pixel format %s" % self.pf.hex()
        # SetEncodings: Raw(0), DesktopSize(-223)
        encs = [0, -223]
        self.s.sendall(struct.pack(">BxH", 2, len(encs)) +
                       b"".join(struct.pack(">i", e) for e in encs))
        self.fb = np.zeros((self.h, self.w, 4), np.uint8)

    def request(self, incremental=1):
        self.s.sendall(struct.pack(">BBHHHH", 3, 1 if incremental else 0,
                                   0, 0, self.w, self.h))

    def read_update(self):
        """Return (rects, resized) after applying one FramebufferUpdate."""
        while True:
            mt = recvall(self.s, 1)[0]
            if mt == 0:
                break
            elif mt == 2:      # Bell
                continue
            elif mt == 3:      # ServerCutText
                hdr = recvall(self.s, 7)
                ln = struct.unpack(">I", hdr[3:7])[0]
                recvall(self.s, ln)
                continue
            else:
                raise RuntimeError("unexpected server msg %d" % mt)
        nrect = struct.unpack(">xH", recvall(self.s, 3))[0]
        rects = []
        resized = False
        for _ in range(nrect):
            x, y, w, h, enc = struct.unpack(">HHHHi", recvall(self.s, 12))
            if enc == -223:                       # DesktopSize
                self.w, self.h = w, h
                self.fb = np.zeros((self.h, self.w, 4), np.uint8)
                resized = True
                continue
            if enc != 0:
                raise RuntimeError("unexpected encoding %d" % enc)
            data = recvall(self.s, w * h * 4)
            if w and h:
                self.fb[y:y + h, x:x + w] = np.frombuffer(
                    data, np.uint8).reshape(h, w, 4)
            rects.append((x, y, w, h))
        return rects, resized


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=5900)
    ap.add_argument("--seconds", type=float, default=60.0)
    ap.add_argument("--out", default="cap")
    ap.add_argument("--hudthresh", type=float, default=-1.0,
                    help="hud mean below this => incomplete frame")
    ap.add_argument("--dark", type=float, default=8.0,
                    help="frame mean below this => 'dark'")
    ap.add_argument("--neighbours", type=int, default=2)
    ap.add_argument("--max-save", type=int, default=60)
    ap.add_argument("--hud", default="410,695,870,735",
                    help="x0,y0,x1,y1 of a region that is bright in EVERY "
                         "completed app frame (default: 3DMark's fps bar at "
                         "1280x800)")
    ap.add_argument("--exclusive", action="store_true",
                    help="RFB shared=0 (kicks other viewers)")
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    hx0, hy0, hx1, hy1 = (int(v) for v in args.hud.split(","))
    r = RFB(args.host, args.port, shared=0 if args.exclusive else 1)
    r.s.settimeout(2.0)
    sys.stderr.write("connected %dx%d name=%r\n" % (r.w, r.h, r.name))

    log = open(os.path.join(args.out, "frames.jsonl"), "w")
    r.request(incremental=0)

    t_end = time.time() + args.seconds
    idx = 0
    stalls = 0
    to_save = []          # (idx, tag, frame) written AFTER the loop
    ring = []             # last few frames, for "pre" context
    pending_after = 0
    while time.time() < t_end:
        try:
            rects, resized = r.read_update()
        except socket.timeout:
            # QEMU backs its VNC refresh timer off to 3 s when nothing is
            # changing; re-arm with a forced update rather than give up.
            stalls += 1
            r.request(incremental=0)
            continue
        t = time.time()
        r.request(incremental=1)
        f = r.fb
        sub = f[::4, ::4, :3]
        mx = sub.max(axis=2)
        mean = float(sub.mean())
        rec = {"i": idx, "t": round(t, 6), "n": len(rects),
               "area": sum(w * h for _, _, w, h in rects),
               "mean": round(mean, 3),
               "dark": round(float((mx < 12).mean()), 4),
               "p95": float(np.percentile(mx, 95)), "w": r.w, "h": r.h,
               "hud": round(float(f[hy0:hy1, hx0:hx1, :3].mean()), 2)}
        if resized:
            rec["resize"] = True
        if len(rects) <= 4:
            rec["r"] = rects
        log.write(json.dumps(rec) + "\n")

        is_dark = rec["hud"] < args.hudthresh or mean < args.dark
        if is_dark and len(to_save) < args.max_save:
            for j, arr in ring:
                to_save.append((j, "pre", arr))
            ring = []
            to_save.append((idx, "DARK", f.copy()))
            pending_after = args.neighbours
        elif pending_after and len(to_save) < args.max_save:
            to_save.append((idx, "post", f.copy()))
            pending_after -= 1
        else:
            ring.append((idx, f.copy()))
            if len(ring) > args.neighbours:
                ring.pop(0)
        idx += 1

    log.close()
    sys.stderr.write("captured %d frames (%d stalls); writing %d pngs\n"
                     % (idx, stalls, len(to_save)))
    seen = set()
    for j, tag, arr in to_save:
        if j in seen:
            continue
        seen.add(j)
        Image.fromarray(arr[:, :, [2, 1, 0]]).save(
            os.path.join(args.out, "f%06d_%s.png" % (j, tag)))


if __name__ == "__main__":
    main()
