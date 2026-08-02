#!/usr/bin/env python3
"""Defect 0ab-B oracle: what did the host actually put on screen, per flush?

  usage: scanout_oracle_report.py /tmp/helios-qemu-stderr.log [label]

Reads the two QEMU-side trace events added for 0ab-B (ui/egl-headless.c):

  helios_scanout_bind  res R bound_ino I size S backing WxH stride ST offset OF
                       read_ino J reuse N path P
  helios_scanout_read  res R bound_ino I read_ino J xy .. wh .. sampled N
                       nonzero K max M csum C seq Q

`bound_ino` is the DMA-BUF the guest has bound at that moment; `read_ino` is the
DMA-BUF the active readback actually imported.  One line is emitted per FLUSH,
so this resolves every displayed frame -- the VNC sampler could not (30/s vs
~142 flushes/s).

Answers, in order:
  1. are the guest's scan-out resources distinct buffers, or aliased?
  2. when the host reads black, did it read the BOUND buffer?
  3. is a black read new content, or a re-read of what it already showed?
"""
import calendar
import re
import sys
from collections import Counter, defaultdict

TS = r"^(\d{4})-(\d\d)-(\d\d)T(\d\d):(\d\d):(\d\d)\.(\d{6})Z\s+"
READ = re.compile(
    TS + r"helios_scanout_read res (\d+) bound_ino (\d+) read_ino (\d+) "
    r"xy 0x([0-9a-f]+) wh 0x([0-9a-f]+) sampled (\d+) nonzero (\d+) "
    r"max (\d+) csum 0x([0-9a-f]+) seq (\d+)")
BIND = re.compile(
    TS + r"helios_scanout_bind res (\d+) bound_ino (\d+) size (\d+) "
    r"backing (\d+)x(\d+) stride (\d+) offset (\d+) read_ino (\d+) "
    r"reuse (\d+) path (\S+)")
LAYOUT = re.compile(
    TS + r"helios_scanout_blob_layout scanout (\d+) res (\d+) fd (-?\d+) "
    r"blob_size (\d+) guest_offset (\d+) fb_offset (\d+) stride (\d+) "
    r"(\d+)x(\d+) modifier 0x([0-9a-f]+)")

# A frame is BLACK when no sampled pixel has any colour at all: the post-clear,
# pre-first-draw state.  Anything above that is the app's own content, however
# dark, so it is not counted here (that distinction cost a cycle once).
BLACK_MAX = 0


def stamp(m, i=1):
    y, mo, d, h, mi, s, us = (int(x) for x in m.groups()[i - 1:i + 6])
    return calendar.timegm((y, mo, d, h, mi, s, 0, 0, 0)) + us / 1e6


def load(path):
    reads, binds, layouts = [], [], []
    for line in open(path, errors="replace"):
        m = READ.match(line)
        if m:
            g = m.groups()[7:]
            reads.append(dict(
                t=stamp(m), res=int(g[0]), bound=int(g[1]), read=int(g[2]),
                xy=int(g[3], 16), wh=int(g[4], 16), sampled=int(g[5]),
                nonzero=int(g[6]), max=int(g[7]), csum=int(g[8], 16),
                seq=int(g[9])))
            continue
        m = BIND.match(line)
        if m:
            g = m.groups()[7:]
            binds.append(dict(
                t=stamp(m), res=int(g[0]), bound=int(g[1]), size=int(g[2]),
                w=int(g[3]), h=int(g[4]), stride=int(g[5]), offset=int(g[6]),
                read=int(g[7]), reuse=int(g[8]), path=g[9]))
            continue
        m = LAYOUT.match(line)
        if m:
            g = m.groups()[7:]
            layouts.append(dict(
                t=stamp(m), scanout=int(g[0]), res=int(g[1]), fd=int(g[2]),
                blob=int(g[3]), guest_offset=int(g[4]), fb_offset=int(g[5]),
                stride=int(g[6]), w=int(g[7]), h=int(g[8]),
                modifier=int(g[9], 16)))
    return reads, binds, layouts


def workload_window(reads, min_rate=40):
    """Longest run of consecutive seconds flushing at a fullscreen rate."""
    per = Counter(int(r["t"]) for r in reads)
    secs = sorted(s for s, n in per.items() if n >= min_rate)
    best = run = []
    for s in secs:
        if run and s == run[-1] + 1:
            run.append(s)
        else:
            run = [s]
        if len(run) > len(best):
            best = list(run)
    if not best:
        return reads[0]["t"], reads[-1]["t"]
    return float(best[0]), float(best[-1] + 1)


def pct(n, d):
    return "%5.1f%%" % (100.0 * n / d) if d else "    -"


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/helios-qemu-stderr.log"
    label = sys.argv[2] if len(sys.argv) > 2 else path
    reads, binds, layouts = load(path)
    if not reads:
        print("no helios_scanout_read lines in %s -- was the trace enabled?"
              % path)
        return 1

    t0, t1 = workload_window(reads)
    win_r = [r for r in reads if t0 <= r["t"] <= t1]
    win_b = [b for b in binds if t0 <= b["t"] <= t1]
    print("=== %s ===" % label)
    print("window %.1f s   flushes %d (%.1f/s)   binds %d (%.1f/s)"
          % (t1 - t0, len(win_r), len(win_r) / max(t1 - t0, 1e-9),
             len(win_b), len(win_b) / max(t1 - t0, 1e-9)))
    print("(whole log: %d flushes, %d binds)" % (len(reads), len(binds)))

    # 1. buffer inventory -------------------------------------------------
    print("\n-- buffers (are the rotating resources distinct memory?) --")
    by_res = defaultdict(lambda: dict(inos=Counter(), sizes=Counter(),
                                      paths=Counter(), binds=0, shape=""))
    for b in win_b:
        e = by_res[b["res"]]
        e["inos"][b["bound"]] += 1
        e["sizes"][b["size"]] += 1
        e["paths"][b["path"]] += 1
        e["binds"] += 1
        e["shape"] = "%dx%d stride %d offset %d" % (b["w"], b["h"], b["stride"],
                                                    b["offset"])
    flushes_by_res = Counter(r["res"] for r in win_r)
    for res in sorted(by_res):
        e = by_res[res]
        print("  res %-6d binds %-5d flushes %-5d ino %s size %s %s [%s]"
              % (res, e["binds"], flushes_by_res.get(res, 0),
                 ",".join(str(i) for i in e["inos"]),
                 ",".join(str(s) for s in e["sizes"]), e["shape"],
                 ",".join(e["paths"])))
    # The guest's own view of each buffer.  QEMU builds every QemuDmaBuf at
    # offset 0, so a nonzero guest_offset means the host reads the wrong bytes.
    win_l = [l for l in layouts if t0 <= l["t"] <= t1]
    if win_l:
        seen = Counter((l["res"], l["fd"], l["blob"], l["guest_offset"],
                        l["fb_offset"], l["stride"], l["w"], l["h"])
                       for l in win_l)
        print("\n-- SET_SCANOUT_BLOB as the guest sent it --")
        for (res, fd, blob, goff, foff, stride, w, h), n in \
                sorted(seen.items()):
            print("  res %-6d fd %-4d blob %-10d guest_offset %-8d "
                  "fb_offset %-8d stride %-6d %dx%d  x%d"
                  % (res, fd, blob, goff, foff, stride, w, h, n))
        bad = sum(n for k, n in seen.items() if k[3] or k[4])
        print("  binds with a NONZERO offset (dropped by the QemuDmaBuf): "
              "%d %s" % (bad, pct(bad, len(win_l))))

    ino_owners = defaultdict(set)
    for res, e in by_res.items():
        for ino in e["inos"]:
            ino_owners[ino].add(res)
    aliased = {i: rs for i, rs in ino_owners.items() if len(rs) > 1}
    print("  ALIASED: %s" % (aliased if aliased else
                             "no -- every resource has its own DMA-BUF"))

    # 2. did the host read the buffer the guest bound? ---------------------
    wrong = [r for r in win_r if r["read"] and r["read"] != r["bound"]]
    print("\n-- read identity --")
    print("  flushes whose read_ino != bound_ino: %d / %d %s"
          % (len(wrong), len(win_r), pct(len(wrong), len(win_r))))
    if wrong:
        print("  by resource: %s"
              % Counter((r["res"], r["bound"], r["read"]) for r in wrong)
              .most_common(6))

    # 3. content verdict ---------------------------------------------------
    black = [r for r in win_r if r["max"] <= BLACK_MAX]
    print("\n-- content --")
    print("  BLACK flushes (no lit pixel): %d / %d %s"
          % (len(black), len(win_r), pct(len(black), len(win_r))))
    for res in sorted(flushes_by_res):
        n = flushes_by_res[res]
        k = sum(1 for r in black if r["res"] == res)
        print("    res %-6d %5d flushes, %5d black %s" % (res, n, k, pct(k, n)))
    if black:
        bw = sum(1 for r in black if r["read"] and r["read"] != r["bound"])
        print("  of the black flushes, %d read a buffer OTHER than the bound "
              "one %s" % (bw, pct(bw, len(black))))

    # 4. new content or a re-read? -----------------------------------------
    dup = same_res_dup = 0
    prev = None
    for r in win_r:
        if prev is not None and r["csum"] == prev["csum"]:
            dup += 1
            if r["res"] == prev["res"]:
                same_res_dup += 1
        prev = r
    print("  consecutive flushes with IDENTICAL content: %d %s (same res %d)"
          % (dup, pct(dup, len(win_r)), same_res_dup))

    # 5. shape of the artifact --------------------------------------------
    runs, cur = Counter(), 0
    for r in win_r:
        if r["max"] <= BLACK_MAX:
            cur += 1
        elif cur:
            runs[cur] += 1
            cur = 0
    if cur:
        runs[cur] += 1
    print("  black run lengths: %s"
          % (", ".join("%d x%d" % (k, v) for k, v in sorted(runs.items()))
             or "none"))

    # 6. position relative to the bind that armed it -----------------------
    if win_b:
        bt = [b["t"] for b in win_b]
        import bisect

        def since_bind(r):
            i = bisect.bisect_right(bt, r["t"]) - 1
            return None if i < 0 else r["t"] - bt[i]

        def first_after_bind(r):
            i = bisect.bisect_right(bt, r["t"]) - 1
            return i >= 0 and win_b[i]["res"] == r["res"]

        buckets = [(0, 0.001), (0.001, 0.003), (0.003, 0.006), (0.006, 0.012),
                   (0.012, 1e9)]
        print("\n-- black rate vs age of the current binding --")
        for lo, hi in buckets:
            sel = [r for r in win_r
                   if (a := since_bind(r)) is not None and lo <= a < hi]
            k = sum(1 for r in sel if r["max"] <= BLACK_MAX)
            print("  %6.0f-%-6.0f ms  %5d flushes  %5d black %s"
                  % (lo * 1e3, hi * 1e3 if hi < 1e9 else 999, len(sel), k,
                     pct(k, len(sel))))
        firsts = [r for r in win_r if first_after_bind(r)]
        k = sum(1 for r in firsts if r["max"] <= BLACK_MAX)
        print("  first flush after a bind of the SAME res: %d, %d black %s"
              % (len(firsts), k, pct(k, len(firsts))))

    # 7. partial flushes ---------------------------------------------------
    shapes = Counter((r["wh"] >> 16, r["wh"] & 0xffff) for r in win_r)
    print("\n-- flush rects: %s"
          % ", ".join("%dx%d x%d" % (w, h, n) for (w, h), n in
                      shapes.most_common(4)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
