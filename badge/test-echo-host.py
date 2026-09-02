#!/usr/bin/env python3
"""Exercise echo-host.py against a pty standing in for the badge.

This is the regression net for the host end of the probe protocol. Only the badge
end is untested -- there is no way to test it but to flash it -- so everything
that *can* be pinned down here is, and the cases are chosen from the failures that
actually cost a capture: a token split across a read, a token repeated in one
read, a report line eaten by a parser that was too eager, a byte count lost to a
cleanup error, a `Z` in prose mistaken for transmit fill.

    ./test-echo-host.py          # no arguments; exits non-zero on any failure

Stdlib only. Takes about half a minute, most of it deliberate settle time.
"""
import os
import pty
import random
import subprocess
import sys
import time

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "echo-host.py")
PAGE = 4096
REQ = b"REQ\n"


def split_lines(out):
    """Separate the script's own `host:` report lines from the badge's."""
    lines = [l for l in out.decode().splitlines() if l]
    return ([l for l in lines if not l.startswith("host: ")],
            [l for l in lines if l.startswith("host: ")])


def run(case, writes, expect_pages, expect_lines, expect_host=0, settle=0.6,
        drain=1.0):
    """Feed `writes` to the script through a pty and check what comes back.

    `expect_pages` is whole pages echoed, `expect_lines` the badge report lines
    printed on stdout, and `expect_host` the number of `host:` lines the script
    should have added of its own.
    """
    master, slave = pty.openpty()
    p = subprocess.Popen([sys.executable, SCRIPT, os.ttyname(slave)],
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    os.close(slave)
    time.sleep(0.4)
    got = bytearray()
    os.set_blocking(master, False)
    for w in writes:
        # Drain while writing: a pty buffer is small, and a case that asks for
        # more than one page back would otherwise deadlock against the script's
        # own blocking write rather than testing anything.
        i = 0
        while i < len(w):
            try:
                i += os.write(master, w[i:])
            except BlockingIOError:
                try:
                    got += os.read(master, 65536)
                except (BlockingIOError, OSError):
                    time.sleep(0.005)
        time.sleep(0.05)
        try:
            got += os.read(master, 65536)
        except (BlockingIOError, OSError):
            pass
    time.sleep(settle)
    deadline = time.time() + drain
    while time.time() < deadline:
        try:
            d = os.read(master, 65536)
            if not d:
                break
            got += d
        except BlockingIOError:
            time.sleep(0.02)
        except OSError:
            break
    os.close(master)
    try:
        out, err = p.communicate(timeout=8)
    except subprocess.TimeoutExpired:
        p.kill()
        out, err = p.communicate()
        print("  %-40s TIMEOUT (did not exit)" % case)
        return False
    pages = len(got) // PAGE
    lines, hostlines = split_lines(out)
    ok = (pages == expect_pages) and (lines == expect_lines) and (len(hostlines) == expect_host)
    print("  %-40s %s  pages=%d(exp %d) lines=%r host=%d(exp %d)" % (
        case, "OK " if ok else "FAIL", pages, expect_pages, lines,
        len(hostlines), expect_host))
    if not ok:
        print("     stderr:", err.decode().strip().replace("\n", " | "))
        print("     host  :", hostlines)
    return ok


fill = b"\x5a" * 500
results = []

# --- the single-page request path ---
results.append(run("one REQ", [REQ], 1, [], expect_host=1))
results.append(run("REQ split across two writes", [b"RE", b"Q\n"], 1, [], expect_host=1))
results.append(run("three REQs in one write", [REQ * 3], 3, [], expect_host=1))
results.append(run("REQ after 8KiB+ of fill", [b"\x5a" * 20000, REQ], 1, [], expect_host=1))

# --- the streaming path, new in fix round 4 ---
results.append(run("STREAM 4 -> four pages", [b"STREAM 4\n"], 4, [], expect_host=1,
                   drain=2.0))
results.append(run("STREAM split across three writes",
                   [b"STR", b"EAM ", b"4\n"], 4, [], expect_host=1, drain=2.0))
results.append(run("STREAM 48 spans several bursts",
                   [b"STREAM 48\n"], 48, [], expect_host=1, drain=6.0, settle=1.0))
results.append(run("STREAM then REQ, in order",
                   [b"STREAM 2\nREQ\n"], 3, [], expect_host=2, drain=2.0))
# Two host lines here, not one: a served-nothing stream is still a served
# stream, and the summary reports requests and streams on separate lines.
results.append(run("STREAM 0 draws nothing and is consumed",
                   [b"STREAM 0\n", REQ], 1, [], expect_host=2))
# The word in prose must survive: eating a report line to the next newline
# because it happened to contain "STREAM " would destroy a measurement.
results.append(run("report line containing STREAM survives",
                   [b"stream: STREAM was requested\r\n"], 0,
                   ["stream: STREAM was requested"]))
results.append(run("STREAM with a non-numeric argument is prose",
                   [b"STREAM xy\r\n"], 0, ["STREAM xy"]))

# --- report lines ---
results.append(run("report line", [b"xfer: 1 x 2B\r\n"], 0, ["xfer: 1 x 2B"]))
results.append(run("fill then report line",
                   [fill + b"map: 256 KiB touched\r\n"], 0, ["map: 256 KiB touched"]))
results.append(run("literal Z survives both ends",
                   [b"Zmap 2 MiB: okZ\r\n"], 0, ["Zmap 2 MiB: okZ"]))
results.append(run("all-fill pseudo-line is dropped",
                   [b"\x5a" * 600 + b"\r\n"], 0, []))
results.append(run("long fill run before text is stripped",
                   [b"\x5a" * 600 + b"rt: 64/64\r\n"], 0, ["rt: 64/64"]))
results.append(run("corrupt byte is shown, not dropped",
                   [b"rt: has\x00nul\r\n"], 0, ["[corrupt] b'rt: has\\x00nul'"]))
results.append(run("traffic with no request draws no reply", [fill * 4], 0, []))

# --- EOF must not busy-spin, and must still print the counters ---
print("  EOF busy-spin check...")
master, slave = pty.openpty()
p = subprocess.Popen([sys.executable, SCRIPT, os.ttyname(slave)],
                     stdout=subprocess.PIPE, stderr=subprocess.PIPE)
os.close(slave)
time.sleep(0.4)
t_cpu0 = os.times()
os.close(master)
time.sleep(1.0)
try:
    p.wait(timeout=3)
    exited = True
except subprocess.TimeoutExpired:
    p.kill()
    exited = False
out, err = p.communicate()
child = os.times()[2] + os.times()[3] - (t_cpu0[2] + t_cpu0[3])
counters_printed = "pages served" in err.decode()
print("  %-40s %s  exited=%s cpu=%.2fs counters=%s" % (
    "EOF: exits, low CPU, counters printed",
    "OK " if (exited and child < 0.5 and counters_printed) else "FAIL",
    exited, child, counters_printed))
results.append(exited and child < 0.5 and counters_printed)

# --- randomized fuzz over chunk boundaries ---
print("  fuzz (20 rounds)...")
fuzz_ok = True
for rnd in range(20):
    nreq = random.randint(1, 6)
    nstream = random.randint(0, 2)
    stream = b"\x5a" * random.randint(0, 30000)
    for _ in range(nreq):
        stream += b"\x5a" * random.randint(0, 5000) + REQ
    for _ in range(nstream):
        stream += b"STREAM %d\n" % random.randint(1, 3)
    stream += b"probe done\r\n"
    chunks = []
    i = 0
    while i < len(stream):
        n = random.randint(1, 4000)
        chunks.append(stream[i:i + n])
        i += n
    master, slave = pty.openpty()
    p = subprocess.Popen([sys.executable, SCRIPT, os.ttyname(slave)],
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    os.close(slave)
    time.sleep(0.3)
    got = bytearray()
    os.set_blocking(master, False)
    for c in chunks:
        j = 0
        while j < len(c):
            try:
                j += os.write(master, c[j:])
            except BlockingIOError:
                try:
                    got += os.read(master, 65536)
                except (BlockingIOError, OSError):
                    time.sleep(0.005)
    deadline = time.time() + 2.0
    while time.time() < deadline:
        try:
            d = os.read(master, 65536)
            if not d:
                break
            got += d
        except BlockingIOError:
            time.sleep(0.02)
        except OSError:
            break
    os.close(master)
    try:
        out, err = p.communicate(timeout=8)
    except subprocess.TimeoutExpired:
        p.kill()
        out, err = p.communicate()
        fuzz_ok = False
        print("    round %d: TIMEOUT" % rnd)
        continue
    # The pty echoes our own writes back at us, so the reply count comes from the
    # script's own counters on stderr rather than from what we read.
    tail = err.decode().rsplit("rx ", 1)[-1]
    served = int(tail.split("pages served")[0].split(",")[-1].strip())
    streams = int(tail.split("streams")[0].split(",")[-1].strip())
    lines, _ = split_lines(out)
    if served != nreq or streams != nstream or lines != ["probe done"]:
        fuzz_ok = False
        print("    round %d: FAIL served=%d/%d streams=%d/%d lines=%r"
              % (rnd, served, nreq, streams, nstream, lines))
print("  %-40s %s" % ("fuzz", "OK " if fuzz_ok else "FAIL"))
results.append(fuzz_ok)

print("\n%d/%d passed" % (sum(1 for r in results if r), len(results)))
sys.exit(0 if all(results) else 1)
