#!/usr/bin/env python3
"""Host counterpart to `probe/` -- a plain serial echo, nothing more.

Two things the probe asks for, and nothing else:

* `REQ\\n`        -> exactly one 4096-byte page. This is the per-request
                    round-trip leg, and the host's own service time for it is
                    measured here and printed at exit, because on the badge that
                    time is inside the latency figure and cannot be separated.
* `STREAM <n>\\n`  -> n pages back to back, as fast as the port accepts them, with
                    no waiting in between. This is the sustained-receive leg. It
                    exists because the emulator will read ahead and pipeline
                    rather than stop and wait for each page, so stop-and-wait is
                    the wrong shape to measure the channel with.

                    From fix round 5 the probe issues a *sweep* of these at
                    doubling sizes rather than one 1 MiB push, because a push
                    larger than the badge can absorb kills the probe outright.
                    Nothing changes on this side -- the request is the same and
                    the answer is the same -- but each one is now counted
                    separately below, so the transcript carries an independent
                    check of which bursts were actually served and how big they
                    were.

Everything the probe transmits is counted along the way, so the badge's TX figure
has an independent check.

This is deliberately NOT the Task 4 frame protocol. That protocol is the thing
the emulator will eventually speak; this script exists only to close the loop for
a timing measurement, and a framing layer in the middle would be measured too.

    ./echo-host.py /dev/cu.usbmodem<serial> [| tee probe-transcript.txt]

Stdlib only, raw tty via termios -- no pyserial. Ctrl-C to stop; the byte counts
print on the way out. Report lines go to stdout, everything else to stderr, so
piping stdout to a file captures exactly the transcript -- and the host's own
`host:` turnaround line is a report line, so it goes to stdout with the rest.
"""

import os
import sys
import termios
import time

PAGE = 4096
REQ = b"REQ\n"
STREAM = b"STREAM "
# A **position-dependent** page, not a repeated byte.
#
# This used to be `b"\xa5" * PAGE`, and that single choice hid a real driver bug
# for five hardware rounds. `usb-bao1x` was re-reading one shared 512-byte
# staging buffer for several packets' worth of counts, so deliveries were
# duplicated packets -- and against a page of identical bytes a duplicated
# packet is *byte-for-byte indistinguishable from a correct one*. The probe's
# only content check was `d[0] != FILL || d[last] != FILL`, which such a page
# passes no matter how badly the middle is scrambled. "The probe receives real
# data over this exact mechanism" was therefore never the reassurance it looked
# like, and it pointed several rounds of debugging away from the driver.
#
# The counter below makes every 512-byte packet distinct and every byte's
# position checkable, so duplication, reordering and truncation all show up.
# The low byte still varies fastest so a hexdump reads naturally.
#
# **Any future host-side test data must be position-dependent for the same
# reason.** A constant fill cannot detect a transport that repeats itself.
FILL = bytes((i * 7 + (i >> 8) * 131) & 0xFF for i in range(PAGE))
TX_FILL = 0x5A  # the byte the probe's transmit leg blasts
FILL_RUN = 8  # leading fill bytes needed before a run is treated as payload
# One write syscall carries this much of a stream. Bigger than a page because the
# point of the streaming leg is to keep the port busy, and one page per write puts
# a syscall boundary in the middle of the thing being measured.
BURST = 64 * 1024
# Bytes of unterminated tail kept when the buffer is trimmed. Long enough to rejoin
# a `STREAM <n>\n` split across two reads, which `len(REQ) - 1` was not.
KEEP = 32
# Ceiling on a single `STREAM` request, in pages: 16 MiB.
MAX_STREAM_PAGES = 4096

# Configuring the tty by hand rather than with tty.setraw(). On Python 3.9 --
# which is /usr/bin/python3 on macOS 14 -- setraw() leaves IXANY, IMAXBEL, ONLCR
# and HUPCL set and never touches INLCR, IGNCR or CLOCAL, and tty.cfmakeraw()
# only exists on 3.12+. That matters because termios state persists on the device
# node across opens: an earlier program that left INLCR set turns the probe's
# `REQ\n` into `REQ\r`, this script never answers, and the badge reports a false
# `rt: TIMEOUT` that looks like a hardware finding.
IFLAG_CLEAR = ["IGNBRK", "BRKINT", "IGNPAR", "PARMRK", "INPCK", "ISTRIP",
               "INLCR", "IGNCR", "ICRNL", "IXON", "IXOFF", "IXANY", "IMAXBEL"]
OFLAG_CLEAR = ["OPOST", "ONLCR", "OCRNL", "ONOCR", "ONLRET"]
LFLAG_CLEAR = ["ECHO", "ECHOE", "ECHOK", "ECHONL", "ICANON", "ISIG", "IEXTEN"]
CFLAG_CLEAR = ["PARENB", "CSIZE", "HUPCL"]
CFLAG_SET = ["CS8", "CREAD", "CLOCAL"]


def _mask(names):
    m = 0
    for n in names:
        m |= getattr(termios, n, 0)
    return m


def make_raw(fd):
    a = termios.tcgetattr(fd)
    a[0] &= ~_mask(IFLAG_CLEAR)
    a[1] &= ~_mask(OFLAG_CLEAR)
    a[2] = (a[2] & ~_mask(CFLAG_CLEAR)) | _mask(CFLAG_SET)
    a[3] &= ~_mask(LFLAG_CLEAR)
    a[6][termios.VMIN] = 1
    a[6][termios.VTIME] = 0
    termios.tcsetattr(fd, termios.TCSANOW, a)


def show(line):
    """Print a probe report line.

    The 4 KiB throughput payload is 0x5a bytes with no line terminator, so the
    megabytes of fill arrive as one enormous pseudo-line, and a report line can
    in principle arrive with a run of fill in front of it. Two rules, because
    0x5a *is* ASCII 'Z' and a blanket strip corrupts real text:

    * an all-fill line is the payload, and is dropped;
    * a leading run of fill is stripped only when it is long (>= FILL_RUN). No
      report line starts with eight 'Z's; several could start with one. A short
      run left in place prints as a visible oddity rather than silently eating a
      character.

    A line that is not clean ASCII is printed as an escaped repr rather than
    dropped. This transcript is the only record of numbers that cost a flash; a
    line silently deleted for one bad byte erases a whole measurement and leaves
    no sign that it existed.
    """
    i = 0
    while i < len(line) and line[i] == TX_FILL:
        i += 1
    if i == len(line):
        return  # all fill: this is the transmit payload, not a report
    if i >= FILL_RUN:
        line = line[i:]
    if not line:
        return
    if all(32 <= b < 127 for b in line):
        print(line.decode("ascii"), flush=True)
    else:
        print("[corrupt] " + repr(line), flush=True)


def find_stream(buf):
    """Locate the first well-formed `STREAM <n>\\n`.

    Returns `(start, end, n)`, or `(-1, 0, 0)` when there is none to act on.

    Two things it refuses to do. It will not treat a `STREAM ` that is not
    followed by digits and a newline as a command -- a report line is allowed to
    contain the word, and eating one to the next newline would destroy a
    measurement. And it will not act on a command whose newline has not arrived
    yet: that is `(-1, ...)` too, so the caller waits for the rest of the read
    rather than serving a truncated page count.
    """
    i = 0
    while True:
        i = buf.find(STREAM, i)
        if i < 0:
            return (-1, 0, 0)
        j = buf.find(b"\n", i)
        if j < 0:
            return (-1, 0, 0)  # incomplete; the next read finishes it
        arg = buf[i + len(STREAM):j]
        if arg.isdigit():
            return (i, j + 1, int(arg))
        i += len(STREAM)  # a literal "STREAM " in prose; keep looking


def stream_pages(fd, n):
    """Write n pages back to back, in bursts, with nothing in between.

    The page count is capped. It arrives over the wire from a device that is being
    deliberately pushed to its limits, and a corrupted digit should cost a wrong
    measurement, not this host's memory.
    """
    n = min(n, MAX_STREAM_PAGES)
    burst = FILL * (BURST // PAGE)
    sent = 0
    full, rest = divmod(n, BURST // PAGE)
    for _ in range(full):
        sent += write_all(fd, burst)
    if rest:
        sent += write_all(fd, FILL * rest)
    return sent


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    dev = sys.argv[1]

    fd = os.open(dev, os.O_RDWR | os.O_NOCTTY)
    saved = termios.tcgetattr(fd)
    make_raw(fd)
    rx = tx = served = 0
    streams = []
    turnarounds = []
    buf = b""
    print("echoing %d-byte pages on %s; ctrl-C to stop" % (PAGE, dev), file=sys.stderr)
    try:
        while True:
            chunk = os.read(fd, 65536)
            # Stamped before any parsing, so the turnaround below covers
            # everything this script does with a request and nothing it waits on.
            t_read = time.monotonic()
            if not chunk:
                # EOF: the badge reached terminate_process, or was unplugged, or
                # the node vanished. `continue` here spins a core at 100% and
                # never prints the counters -- which is exactly the moment they
                # are wanted, since the run has just ended.
                print("peer closed the port (EOF)", file=sys.stderr)
                break
            rx += len(chunk)
            buf += chunk
            # Consume whole tokens only, so a `REQ\n`, a `STREAM n\n` or a report
            # line straddling a read boundary is served once and printed once --
            # never twice, and never as a fragment. Whichever token starts
            # earliest is handled first, so the order they arrived in is the order
            # they are acted on.
            while True:
                i_req = buf.find(REQ)
                i_eol = buf.find(b"\r\n")
                i_str, str_end, str_n = find_stream(buf)
                here = [i for i in (i_req, i_eol, i_str) if i >= 0]
                if not here:
                    break
                first = min(here)
                if first == i_str:
                    n = stream_pages(fd, str_n)
                    tx += n
                    streams.append(n)
                    buf = buf[str_end:]
                elif first == i_req:
                    tx += write_all(fd, FILL)
                    served += 1
                    turnarounds.append(time.monotonic() - t_read)
                    buf = buf[i_req + len(REQ):]
                else:
                    show(buf[:i_eol])
                    buf = buf[i_eol + 2:]
            # What is left is unterminated. The throughput payload is megabytes of
            # it with no token in sight, so keep only enough to rejoin a split one.
            if len(buf) > 8192:
                buf = buf[-KEEP:]
    except KeyboardInterrupt:
        pass
    except OSError as e:
        print("read failed: %s" % e, file=sys.stderr)
    finally:
        # Counters first. Restoring termios on a device node that has already
        # gone away raises ENXIO, and losing the independent byte check to a
        # cleanup error -- on the unplug that ends every run -- is the wrong
        # trade.
        #
        # The turnaround line goes to stdout because it is a measurement, not a
        # diagnostic: it is the part of the badge's per-request latency figure
        # that belongs to this host, and the badge cannot see it. Only printed
        # when there is something to report, so a run that served nothing leaves
        # the transcript exactly as it was.
        if served:
            ms = sorted(t * 1000.0 for t in turnarounds)
            print(
                "host: %d requests served, own turnaround min %.2f / median %.2f / "
                "max %.2f ms -- this is inside the badge's rt figure and cannot be "
                "separated there" % (
                    served, ms[0], ms[len(ms) // 2], ms[-1]),
                flush=True,
            )
        # Sizes, not just a count. The probe's sweep stops at the first burst it
        # cannot absorb, and the badge's own report of that burst is written from
        # inside the failure -- so the one independent record of how much was
        # actually pushed at it is this list.
        if streams:
            print(
                "host: %d stream(s) served, KiB each: %s" % (
                    len(streams), ", ".join(str(n // 1024) for n in streams)),
                flush=True,
            )
        print(
            "\nrx %d B, tx %d B, %d pages served, %d streams" % (rx, tx, served, len(streams)),
            file=sys.stderr,
        )
        try:
            termios.tcsetattr(fd, termios.TCSANOW, saved)
        except OSError as e:
            print("could not restore tty settings: %s" % e, file=sys.stderr)
        os.close(fd)


def write_all(fd, data):
    """Write every byte, over a memoryview so a short write does not re-copy.

    `data[n:]` on a megabyte-sized buffer would copy the remainder on every
    partial write, which turns a stream into quadratic memory traffic on the host
    -- inside the window the badge is timing.
    """
    mv = memoryview(data)
    n = 0
    while n < len(mv):
        n += os.write(fd, mv[n:])
    return n


if __name__ == "__main__":
    main()
