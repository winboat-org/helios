#!/usr/bin/env python3
"""Toggle QEMU trace events on a live VM over QMP.

  usage: qmp_trace.py [--sock PATH] on   EVENT [EVENT...]
         qmp_trace.py [--sock PATH] off  EVENT [EVENT...]
         qmp_trace.py [--sock PATH] list [PATTERN]

The trace `log` backend writes to QEMU's stderr, which the launcher tees to
/tmp/helios-qemu-stderr.log with an ISO8601 UTC timestamp per line -- the same
clock `time.time()` gives the VNC probe, so the two streams join directly.

Event names accept globs (`helios_scanout_*`).  Enabling an event that no
module has registered yet is reported, not silently dropped.
"""
import argparse
import json
import socket
import sys

DEFAULT_SOCK = "/tmp/helios-tpm/mon.sock"


class Qmp:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(10)
        self.sock.connect(path)
        self.buf = b""
        self._read()                       # greeting
        self.cmd("qmp_capabilities")

    def _read(self):
        while True:
            nl = self.buf.find(b"\n")
            if nl >= 0:
                line, self.buf = self.buf[:nl], self.buf[nl + 1:]
                if line.strip():
                    msg = json.loads(line)
                    if "event" in msg:     # async event, keep reading
                        continue
                    return msg
            chunk = self.sock.recv(65536)
            if not chunk:
                raise EOFError("QMP closed")
            self.buf += chunk

    def cmd(self, execute, **args):
        req = {"execute": execute}
        if args:
            req["arguments"] = args
        self.sock.sendall((json.dumps(req) + "\n").encode())
        rsp = self._read()
        if "error" in rsp:
            raise RuntimeError("%s: %s" % (execute, rsp["error"]["desc"]))
        return rsp.get("return")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sock", default=DEFAULT_SOCK)
    ap.add_argument("action", choices=["on", "off", "list"])
    ap.add_argument("events", nargs="*")
    a = ap.parse_args()

    q = Qmp(a.sock)
    if a.action == "list":
        pattern = a.events[0] if a.events else "*"
        for ev in q.cmd("trace-event-get-state", name=pattern):
            print("%-40s %s" % (ev["name"], ev["state"]))
        return 0

    if not a.events:
        ap.error("on/off need at least one event name")
    enable = a.action == "on"
    rc = 0
    for name in a.events:
        q.cmd("trace-event-set-state", name=name, enable=enable)
        states = q.cmd("trace-event-get-state", name=name)
        if not states:
            print("NO SUCH EVENT: %s" % name, file=sys.stderr)
            rc = 1
        for ev in states:
            print("%-40s %s" % (ev["name"], ev["state"]))
    return rc


if __name__ == "__main__":
    sys.exit(main())
