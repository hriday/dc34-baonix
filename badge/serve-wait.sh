#!/bin/sh
# Wait for the badge's CDC node and start `rv64-host serve` on it.
#
# The badge starts asking for pages as soon as it boots -- there is no startup
# delay -- and the transport gives up after about two seconds. But the CDC node
# does not exist until the badge has booted. A human cannot win that race by
# hand, which is the same problem `reattach.sh` solves for the probe.
#
# So: poll for the node, start serving the moment it appears, and go back to
# waiting when it goes away. Power-cycle the badge as often as you like.
#
# Each run rebuilds --mem from the guest images, which is what we want: a boot
# starts from a clean guest RAM, never from whatever the last run left behind.
#
# Usage:  ./serve-wait.sh [transcript-path] [rv64-host serve args...]
#         ctrl-C to stop.
#
# PLATFORM. The badge's CDC node is /dev/cu.usbmodem* on macOS and
# /dev/ttyACM* on Linux; both globs are polled below so this script needs no
# editing either way. Only the macOS path has been exercised against real
# hardware -- every hardware run in badge/logs/ was captured on an aarch64
# Mac. The Linux path is expected to work and is unverified. On Linux you
# will also need permission to open the node: be in the `dialout` group (or
# your distribution's equivalent), or run this under sudo.
#
# Use the cu.* node on macOS, never tty.*: tty.* blocks on DCD, which a
# CDC-ACM gadget does not assert.

BADGE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$BADGE/.." && pwd)
LOG=${1:-$BADGE/boot-transcript.txt}
# Guest console output goes to its own file. `$LOG` then contains only the
# badge's own diagnostics and every byte that was NOT a frame -- and the two
# being separable is the point: merged with `2>&1` there is no way to tell
# "the guest printed this" from "this arrived unframed on the wire".
CONSOLE=${CONSOLE:-${LOG%.txt}-console.txt}
shift 2>/dev/null || true
# Anything after the transcript path is passed through to `rv64-host serve`.
# The one that matters today is `--pace-ms N`: write each reply as 512-byte
# chunks N ms apart, so at most one USB packet is in flight. See
# `rv64_host::serve::Pace` -- it tests whether the badge can keep up with
# back-to-back packets, without rebuilding firmware.
#
#   ./serve-wait.sh boot-transcript.txt --pace-ms 1
MEM=${MEM:-/tmp/rv64-guest-mem.img}

# Resolve the guest images through nix so this cannot drift from what the tests
# boot. Override GUEST to point at a different build.
if [ -z "$GUEST" ]; then
    GUEST=$(cd "$ROOT" && nix build .#guest --no-link --print-out-paths) || {
        echo "could not build .#guest -- is nix on PATH?" >&2
        exit 1
    }
fi

HOST="$ROOT/target/release/rv64-host"
[ -x "$HOST" ] || HOST="cargo run --release --manifest-path $ROOT/Cargo.toml -p rv64-host --"

echo "guest:      $GUEST" >&2
echo "transcript: $LOG   (badge diagnostics + unframed bytes)" >&2
echo "console:    $CONSOLE   (guest output only)" >&2
echo "waiting for /dev/cu.usbmodem* or /dev/ttyACM* -- power-cycle the badge whenever you like." >&2

while true; do
    DEV=$(ls /dev/cu.usbmodem* /dev/ttyACM* 2>/dev/null | head -1)
    if [ -n "$DEV" ]; then
        printf '\n=== serving %s at %s ===\n' "$DEV" "$(date +%H:%M:%S)" | tee -a "$LOG"
        # shellcheck disable=SC2086
        $HOST serve \
            --kernel "$GUEST/Image" \
            --dtb    "$GUEST/guest.dtb" \
            --initrd "$GUEST/initramfs.cpio.gz" \
            --mem    "$MEM" \
            --port   "$DEV" \
            --console "$CONSOLE" "$@" 2>&1 | tee -a "$LOG"
        printf '=== detached at %s ===\n' "$(date +%H:%M:%S)" | tee -a "$LOG"
    fi
    sleep 0.2
done
