#!/bin/sh
# Re-attach echo-host.py across power cycles.
#
# The badge's CDC node disappears when it powers down and comes back a moment
# after it boots, which kills any reader holding it. The probe then waits five
# seconds and starts talking, so a human racing `ls /dev/cu.*` against that
# window loses more captures than they win.
#
# This polls for the node, launches echo-host.py the moment it appears, and
# loops when it goes away. Everything lands in one transcript with attach and
# detach markers, so a capture spanning several boots stays readable and in
# order. stderr is merged in deliberately: during bring-up the diagnostics
# ("peer closed the port", read errors) matter as much as the report lines.
#
# Usage:  ./reattach.sh [transcript-path]     ctrl-C to stop.
#
# PLATFORM. Polls both /dev/cu.usbmodem* (macOS) and /dev/ttyACM* (Linux), so
# it needs no editing either way. Only the macOS path has been run against
# real hardware; the Linux path is expected to work and is unverified. On
# Linux you also need permission to open the node -- the `dialout` group or
# equivalent, or sudo. On macOS use the cu.* node, never tty.*, which blocks
# on DCD that a CDC-ACM gadget does not assert.

BADGE=$(cd "$(dirname "$0")" && pwd)
LOG=${1:-$BADGE/probe-transcript.txt}

echo "watching for /dev/cu.usbmodem* or /dev/ttyACM* -- transcript: $LOG" >&2
echo "power-cycle the badge whenever you like; this reattaches on its own." >&2

while true; do
    DEV=$(ls /dev/cu.usbmodem* /dev/ttyACM* 2>/dev/null | head -1)
    if [ -n "$DEV" ]; then
        printf '\n=== attached %s at %s ===\n' "$DEV" "$(date +%H:%M:%S)" | tee -a "$LOG"
        # A node can exist a beat before it is openable; echo-host.py exiting
        # immediately is normal in that case and the loop just tries again.
        python3 "$BADGE/echo-host.py" "$DEV" 2>&1 | tee -a "$LOG"
        printf '=== detached at %s ===\n' "$(date +%H:%M:%S)" | tee -a "$LOG"
    fi
    sleep 0.2
done
