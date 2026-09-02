#!/usr/bin/env python3
"""Instrumented stand-in for echo-host.py, for diagnosing a stalled round-trip leg.

echo-host.py consumes `REQ\\n` silently, so when the probe blocks there is no way
to tell "the host never saw the request" from "the host answered and the badge
threw the bytes away". This prints every inbound byte and announces each reply,
which separates those two cases in one run.

It also answers a bare `REQ\\r` and a bare `REQ`, so that if line-ending
translation is the fault we find out by seeing the round trip complete -- the
answer arrives labelled with which form matched.

Usage:  ./echo-debug.py /dev/cu.usbmodemXXXX
"""

import os
import select
import sys
import termios
import time

PAGE = 4096
PATTERN = bytes(range(256)) * (PAGE // 256)


def make_raw(fd):
    a = termios.tcgetattr(fd)
    a[0] = 0                                  # iflag: no CR/NL translation, no flow control
    a[1] = 0                                  # oflag: no output processing
    a[3] = 0                                  # lflag: no echo, no canonical mode
    a[2] |= termios.CLOCAL | termios.CREAD
    a[6] = list(a[6])
    a[6][termios.VMIN] = 0
    a[6][termios.VTIME] = 0
    termios.tcsetattr(fd, termios.TCSANOW, a)


def write_all(fd, data):
    sent = 0
    while sent < len(data):
        try:
            sent += os.write(fd, data[sent:sent + 512])
        except BlockingIOError:
            select.select([], [fd], [], 0.5)
    return sent


def main():
    dev = sys.argv[1]
    fd = os.open(dev, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    make_raw(fd)
    print("debug echo on %s -- printing every inbound byte" % dev, flush=True)

    buf = b""
    served = 0
    try:
        while True:
            r, _, _ = select.select([fd], [], [], 0.5)
            if not r:
                continue
            try:
                chunk = os.read(fd, 4096)
            except BlockingIOError:
                continue
            if not chunk:
                print("[eof] peer closed", flush=True)
                break

            # Show exactly what arrived, so a REQ that is not the REQ we expect
            # is visible as itself rather than as silence.
            print("[rx %4d] %s" % (len(chunk), repr(chunk[:200])), flush=True)
            buf += chunk

            # Answer any of the three plausible request forms, and say which.
            for form, name in ((b"REQ\n", "REQ\\n"), (b"REQ\r", "REQ\\r"), (b"REQ", "REQ")):
                i = buf.find(form)
                if i >= 0:
                    n = write_all(fd, PATTERN)
                    served += 1
                    print("[tx] matched %s -> wrote %d bytes (reply #%d)"
                          % (name, n, served), flush=True)
                    buf = buf[i + len(form):]
                    break
            else:
                if len(buf) > 8192:
                    buf = buf[-16:]
    except KeyboardInterrupt:
        pass
    finally:
        print("[done] served %d replies" % served, flush=True)
        os.close(fd)


if __name__ == "__main__":
    main()
