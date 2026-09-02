# Hardware transcripts

Captured from the DEF CON 34 badge over USB-serial. Kept because several of these
are the only record that a thing happened at all — the badge has no storage, no
debugger, and every run is a power cycle.

Dates are the capture date, not the commit date.

| file | what it is |
|---|---|
| `2026-09-01-first-linux-boot-console.txt` | **The one that matters.** Guest console output the first time a nixpkgs-built kernel ran on the badge. |
| `2026-09-01-first-linux-boot-transcript.txt` | The same run's badge diagnostics and unframed bytes. Guest console is *not* in here — `serve --console` splits them, which is how we learned the stray device-tree bytes were badge-side and not the guest printing them. |
| `2026-09-01-paced-run-transcript.txt` | The run that proved `--pace-ms` works: 1,313,691 instructions against 13 without it. |
| `2026-09-01-trace-serve-diagnosis.txt` | `trace-serve.py` output showing the host answering `ReadReq` correctly while the badge heard nothing — the measurement that moved the search from our code to the badge's USB driver. |
| `2026-09-01-prefix-usb-driver-diagnosis.txt` | The long run of fingerprints and address dumps that cornered the driver bug. Contains the `00 13 00 00 00 13 …` nop pattern, the `head@base` / `+512` / `+1024` windows whose 1024-byte period identified `usbd-serial`'s two-packet buffer, and the address line proving every archive field was correct while the page content was not. |

| `2026-09-01-throughput-and-input-desync.txt` | Where the interpreter numbers come from. Carries the `rv64 rate:` lines the README quotes — `ips` 146,432 -> 207,680 across a run, with `link` (page I/O) at 19–23 s of a 615 s wall, i.e. **under 4%**: the badge is compute-bound, not transport-bound. The same run ends on the `--input` desync — `reading page 1514: response for page 1513`, with `retries=0 stale=0`, the response stream permanently one behind. |

## The interactive session

`2026-09-02-INTERACTIVE-uname-console.txt` — a human typed a command at the
guest shell over the badge link, and the guest answered.

```
riscv64 Linux
6.12.103
baochip rv64 emu

/nix/store:
6bcwi3dcynnbc2m5d8jq4vp7wblzjvcb
  busybox-static
/bin/sh: can't access tty; job control turned off
~ # uname -r
6.12.103
~ #
```

Keystrokes reach the guest as `ConIn` frames from `rv64-host serve --input`,
cross the same USB-serial link the guest's memory is paged over, and land in the
emulated 8250 UART. The reply comes back the same way.

Run conditions: `--pace-ms 1 --input`, `FRAMES = 1400`. Roughly twenty minutes
from power-on to the prompt, nearly all of it interpreting -- page I/O is under
4% of wall time.

## The shell

`2026-09-01-SHELL-console.txt` / `-transcript.txt` — the run that reached a
prompt. This is the deliverable.

```
riscv64 Linux
6.12.103
baochip rv64 emu

/nix/store:
6bcwi3dcynnbc2m5d8jq4vp7wblzjvcb
  busybox-static
/bin/sh: can't access tty; job control turned off
~ #
```

On the badge's 16x8 display that store entry lands as three rows -- the 32-char
hash split exactly across two lines of sixteen, then the package name -- filling
the screen with nothing spilling. The spec said 18 columns; it was wrong, and at
18 the hash would have split 18+14 and run onto a ninth row that does not exist.

The hash is a real `/nix/store` path for the statically-linked busybox that
nixpkgs built, not a string typed in to look like one.

Run conditions: `serve --pace-ms 1` in both directions, because the badge's USB
driver cannot take back-to-back packets (see the two `usb-bao1x` patches). That
pacing costs about 18 ms per page against a measured 2 ms wire time, which is
why this boot took roughly forty minutes rather than the two it should.

## The first boot

```
[    0.000000] Linux version 6.12.103 (nixbld@localhost)
               (riscv64-unknown-linux-gnu-gcc (GCC) 15.3.0, GNU ld (GNU Binutils) 2.46) #1-NixOS
[    0.000000] Machine model: baochip rv64 emulator
[    0.000000] SBI specification v0.2 detected
[    0.000000] SBI TIME extension detected
[    0.000000] earlycon: sbi0 at I/O port 0x0 (options '')
[    0.000000] printk: legacy bootconsole [sbi0] enabled
[    0.000000] Zone ranges:
[    0.000000]   DMA32    [mem 0x0000000080000000-0x0000000081ffffff]
```

Every instruction behind those lines was fetched a 4 KiB page at a time over
USB-serial from a laptop, decoded by an RV64 interpreter running as a Xous app on
a 32-bit RISC-V microcontroller with 2 MiB of SRAM.

`nixbld@localhost` and `#1-NixOS` are the point: the kernel came out of
`/nix/store`, not from a vendor image.

The run stops mid-line at `node 0: [me` — a memory fault at `0x81fc800c`, under
investigation. That is where the boot got to, not a property of that page: the
emulator is deterministic, so every failure lands at the same address.
