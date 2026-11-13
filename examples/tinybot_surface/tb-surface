#!/usr/bin/env python3
"""tb-surface: the agent's side of tinybot's live surface. One call per turn, nothing held open.

  tb-surface set FILE [--surface ID] [--rev N]   show a surface (FILE: a JSON object, - for stdin)
  tb-surface wait [--timeout S]                  print the waiting taps, or block for the next one
  tb-surface clear                               back to the idle line
  tb-surface status                              what is shown, how many taps wait

Each command prints the app's answer as JSON lines on stdout. `wait` prints one `tap` line per
press and nothing else; the app keeps a tap until this command has printed it.

Exit status: 0 done; 1 the app refused (its `err` line is on stdout); 2 `wait` ran out of time
without a tap; 3 the app cannot be reached or did not speak the protocol.

The socket: --socket PATH, else $TINYBOT_SURFACE_SOCK, else $XDG_RUNTIME_DIR/tinybot/surface.sock.
Python standard library only.
"""
import argparse
import json
import os
import socket
import sys
import time

VERSION = 1


def socket_path(arg):
    if arg:
        return arg
    env = os.environ.get("TINYBOT_SURFACE_SOCK")
    if env:
        return env
    runtime = os.environ.get("XDG_RUNTIME_DIR") or "/run/user/%d" % os.getuid()
    return os.path.join(runtime, "tinybot", "surface.sock")


class Unreachable(Exception):
    pass


class TimedOut(Exception):
    pass


class Surface:
    """A connection: the hello is read on connect, then requests and answers are lines."""

    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.buf = b""
        try:
            self.sock.connect(path)
        except OSError as e:
            raise Unreachable("no surface at %s (%s)" % (path, e.strerror or e))
        self.hello = self.read(5.0)
        if self.hello.get("type") != "hello" or self.hello.get("v") != VERSION:
            raise Unreachable("unexpected greeting: %r" % (self.hello,))

    def send(self, obj):
        line = json.dumps(obj, ensure_ascii=False, separators=(",", ":")) + "\n"
        try:
            self.sock.sendall(line.encode("utf-8"))
        except OSError as e:
            raise Unreachable("the surface went away (%s)" % (e.strerror or e))

    def read(self, timeout):
        """The next line as (object, text). timeout None waits for ever."""
        deadline = None if timeout is None else time.monotonic() + timeout
        while b"\n" not in self.buf:
            if deadline is None:
                self.sock.settimeout(None)
            else:
                left = deadline - time.monotonic()
                if left <= 0:
                    raise TimedOut()
                self.sock.settimeout(left)
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout:
                raise TimedOut()
            except OSError as e:
                raise Unreachable("the surface went away (%s)" % (e.strerror or e))
            if not chunk:
                raise Unreachable("the surface closed the connection")
            self.buf += chunk
        raw, self.buf = self.buf.split(b"\n", 1)
        text = raw.decode("utf-8", "replace")
        try:
            obj = json.loads(text)
        except ValueError:
            raise Unreachable("not JSON from the surface: %r" % text)
        obj["_text"] = text
        return obj


def answer(reply):
    """Prints a one-line answer; the exit status says whether it was a refusal."""
    print(reply["_text"])
    return 1 if reply.get("type") == "err" else 0


def cmd_set(conn, args):
    try:
        text = sys.stdin.read() if args.file == "-" else open(args.file, encoding="utf-8").read()
        spec = json.loads(text)
    except (OSError, ValueError) as e:
        sys.stderr.write("tb-surface: %s: %s\n" % (args.file, e))
        return 64
    if not isinstance(spec, dict):
        sys.stderr.write("tb-surface: %s: a surface is one JSON object\n" % args.file)
        return 64
    spec["type"] = "set"
    if args.surface is not None:
        spec["surface"] = args.surface
    if args.rev is not None:
        spec["rev"] = args.rev
    conn.send(spec)
    return answer(conn.read(10.0))


def cmd_wait(conn, args):
    block = args.timeout is None or args.timeout > 0
    conn.send({"type": "taps", "wait": block})
    try:
        reply = conn.read(args.timeout if block else 10.0)
    except TimedOut:
        sys.stderr.write("tb-surface: no tap within %g s\n" % args.timeout)
        return 2
    last = 0
    while reply.get("type") == "tap":
        print(reply["_text"])
        last = max(last, int(reply.get("seq", 0)))
        reply = conn.read(10.0)
    if reply.get("type") == "err":
        return answer(reply)
    if reply.get("type") != "taps":
        raise Unreachable("unexpected answer: %s" % reply["_text"])
    if last == 0:
        return 2
    # only now, with the taps printed, may the app forget them
    sys.stdout.flush()
    conn.send({"type": "done", "seq": last})
    conn.read(10.0)
    return 0


def cmd_clear(conn, args):
    conn.send({"type": "clear"})
    return answer(conn.read(10.0))


def cmd_status(conn, args):
    conn.send({"type": "status"})
    return answer(conn.read(10.0))


def main():
    parser = argparse.ArgumentParser(prog="tb-surface", description="tinybot's live surface, one call per turn")
    parser.add_argument("--socket", help="the surface's socket (default: $TINYBOT_SURFACE_SOCK, then $XDG_RUNTIME_DIR/tinybot/surface.sock)")
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("set", help="show a surface")
    p.add_argument("file", metavar="FILE", help="a JSON object: surface, rev, title, items (- for stdin)")
    p.add_argument("--surface", help="the surface id, over the one in FILE")
    p.add_argument("--rev", type=int, help="the rev, over the one in FILE")
    p.set_defaults(run=cmd_set)
    p = sub.add_parser("wait", help="print the waiting taps, or block for the next one")
    p.add_argument("--timeout", type=float, metavar="S", help="give up after S seconds (0: only what waits now; default: no limit)")
    p.set_defaults(run=cmd_wait)
    p = sub.add_parser("clear", help="back to the idle line")
    p.set_defaults(run=cmd_clear)
    p = sub.add_parser("status", help="what is shown, how many taps wait")
    p.set_defaults(run=cmd_status)
    args = parser.parse_args()
    try:
        conn = Surface(socket_path(args.socket))
        return args.run(conn, args)
    except Unreachable as e:
        sys.stderr.write("tb-surface: %s\n" % e)
        return 3
    except TimedOut:
        sys.stderr.write("tb-surface: the surface did not answer\n")
        return 3


if __name__ == "__main__":
    sys.exit(main())
