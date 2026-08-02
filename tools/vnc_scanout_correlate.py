#!/usr/bin/env python3
"""Defect-0ab A/B report: VNC completeness vs the QEMU bind/flush stream.

  usage: vnc_scanout_correlate.py frames.jsonl qemu.log [label]

`frames.jsonl` comes from `tools/vnc_frame_probe.py`; `qemu.log` is the
launcher's tee (`/tmp/helios-qemu-stderr.log`) with the virtio-gpu trace events
enabled over QMP:

  trace-event-set-state virtio_gpu_cmd_set_scanout_blob / _res_flush / _res_unref

⚠ `set_scanout_blob` lines begin `id 0, res 0x..`, so a `\\D*` between the event
name and `res` silently drops EVERY blob line -- that cost a whole cycle once.

Picks the workload window automatically as the longest run of seconds with
>= MIN_BINDS binds/s, then reports:
  * how often the displayed surface is an UNFINISHED frame (the probe's HUD
    oracle), and
  * whether the flush for a bind was issued AT the bind (submission-ordered --
    defect 0ab) or ~one app frame later (completion-ordered -- the fix).
"""
import bisect
import calendar
import json
import re
import sys
from collections import defaultdict

LINE = re.compile(
    r"^(\d{4})-(\d\d)-(\d\d)T(\d\d):(\d\d):(\d\d)\.(\d{6})Z\s+(virtio_gpu_\w+)\s+(.*)$")
MIN_BINDS = 10
BIND_EDGE_S = 0.002


def load_trace(path):
    evs = []
    for line in open(path, errors="replace"):
        m = LINE.match(line)
        if not m:
            continue
        y, mo, d, h, mi, s, us, name, rest = m.groups()
        t = calendar.timegm((int(y), int(mo), int(d), int(h), int(mi), int(s),
                             0, 0, 0)) + int(us) / 1e6
        mr = re.search(r"res 0x([0-9a-f]+)", rest)
        evs.append((t, name.replace("virtio_gpu_cmd_", ""),
                    int(mr.group(1), 16) if mr else None))
    evs.sort()
    return evs


def main():
    rows = [json.loads(l) for l in open(sys.argv[1])]
    evs = load_trace(sys.argv[2])
    label = sys.argv[3] if len(sys.argv) > 3 else sys.argv[1]
    t0 = rows[0]["t"]
    evs = [e for e in evs if t0 <= e[0] <= rows[-1]["t"]]

    per = defaultdict(int)
    for t, n, r in evs:
        if n == "set_scanout_blob":
            per[int(t - t0)] += 1
    busy = sorted(s for s, c in per.items() if c >= MIN_BINDS)
    runs, cur = [], []
    for s in busy:
        if cur and s != cur[-1] + 1:
            runs.append(cur)
            cur = []
        cur.append(s)
    if cur:
        runs.append(cur)
    if not runs:
        print("%s: no workload window" % label)
        return
    run = max(runs, key=len)
    lo, hi = t0 + run[0] + 2, t0 + run[-1]        # skip the fade-in second

    print("=== %s ===" % label)
    print("workload window: %+.0f..%+.0f s (%.0f s)" % (lo - t0, hi - t0, hi - lo))

    frames = [r for r in rows if lo <= r["t"] <= hi]
    dark = [r for r in frames if r["hud"] < 40]
    print("VNC frames %d at %.1f/s   UNFINISHED (fps bar missing): %d = %.1f%%"
          % (len(frames), len(frames) / (hi - lo), len(dark),
             100 * len(dark) / max(1, len(frames))))
    allblack = [r for r in dark if r["mean"] < 0.05]
    print("   of those, entirely black (mean < 0.05): %d" % len(allblack))

    # bind -> flush structure
    recs = []
    for i, (t, n, r) in enumerate(evs):
        if n != "set_scanout_blob" or not (lo <= t <= hi):
            continue
        be = mk = None
        for t2, n2, r2 in evs[i + 1:]:
            if n2 == "set_scanout_blob":
                break
            if n2 == "res_flush" and r2 == r:
                if be is None and t2 - t < BIND_EDGE_S:
                    be = t2
                elif t2 - t >= BIND_EDGE_S:
                    mk = t2
                    break
        recs.append((t, r, be, mk))
    nbe = sum(1 for x in recs if x[2])
    print("binds %d (%.1f/s)   with a flush WITHIN %.0f ms of the bind: %d = %.0f%%"
          % (len(recs), len(recs) / (hi - lo), BIND_EDGE_S * 1000, nbe,
             100 * nbe / max(1, len(recs))))
    firsts = sorted((min(x for x in (rec[2], rec[3]) if x) - rec[0]) * 1e3
                    for rec in recs if rec[2] or rec[3])
    if firsts:
        print("bind -> FIRST flush of that resource, ms: p10=%.1f p50=%.1f p90=%.1f"
              % (firsts[len(firsts) // 10], firsts[len(firsts) // 2],
                 firsts[9 * len(firsts) // 10]))

    # dark rate for frames sampled in the early-read window vs after it
    ft = [r["t"] for r in rows]
    win = [0, 0]
    aft = [0, 0]
    for idx, (t, r, be, mk) in enumerate(recs):
        if be and mk:
            j = bisect.bisect_right(ft, be)
            if j < len(rows) and rows[j]["t"] <= mk:
                win[0] += 1
                win[1] += rows[j]["hud"] < 40
        last = mk or be
        if last:
            nxt = recs[idx + 1][0] if idx + 1 < len(recs) else last + 0.05
            j = bisect.bisect_right(ft, last)
            if j < len(rows) and rows[j]["t"] <= nxt:
                aft[0] += 1
                aft[1] += rows[j]["hud"] < 40
    if win[0]:
        print("frame sampled BETWEEN the bind-edge flush and the next flush: "
              "%d/%d = %.0f%% unfinished" % (win[1], win[0], 100 * win[1] / win[0]))
    else:
        print("frame sampled BETWEEN the bind-edge flush and the next flush: "
              "n=0 (no bind-edge flushes to sample between)")
    print("frame sampled AFTER the last flush of a bind:                  "
          "%d/%d = %.0f%% unfinished" % (aft[1], aft[0], 100 * aft[1] / max(1, aft[0])))


if __name__ == "__main__":
    main()
