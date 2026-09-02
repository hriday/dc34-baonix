# riscv64 Linux on a DEF CON 34 badge

An RV64 interpreter, written in `no_std` Rust, running as a [Xous](https://github.com/betrusted-io/xous-core)
application on the DEF CON 34 badge — a Baochip-1x, which is a **32-bit** RISC-V
microcontroller with 2 MiB of SRAM. It boots a nixpkgs-built riscv64 Linux to an
interactive shell.

TL;DR: Take the 3 files from the prebuilt section, and flash your badge. When 
you restart the badge, run the serve-wait.sh so you can see on your screen 
what's going on on the badge, and once you get a shell, you can interact with it.

This is a bragging-rights project. It is slow, it is useless, and the rest of
this document tries to be precise about exactly how slow and exactly how useless.

Shoutouts to 'Bunny' Huang and his team for such a great badge, DT for a cool 
conference and to Rob, Dan and the rest of the NixOS village at Defcon.

---

## The setup

**1. It is a 32-bit machine running a 64-bit operating system.** There is no
virtualisation and no hardware acceleration; there cannot be. Every single RV64
instruction the guest executes is fetched, decoded and executed in software by an
interpreter running on an RV32 core. Every 64-bit add costs the host at least two
operations. Variable shifts, `DIV` and `REM` cost considerably more than two.

**2. The guest has 32 MiB of RAM. The badge has 2 MiB of SRAM, of which about
308 KiB is free** once Xous, its kernel and eleven system services have taken
theirs. The guest's memory is **sixteen times the machine's entire physical
memory**, and about **106 times what is actually available**.

**3. So the guest's RAM is not on the badge at all.** It is a file on a laptop.
It arrives over a USB-serial cable, one 4 KiB page at a time, on demand, for the
entire boot — about **2,952 page operations** from power-on to a shell prompt.
Unplug the cable and the guest stops at its next page fault. There is no storage
on the badge — no SD slot, nothing — so there is nowhere else 32 MiB could live.

**4. And the page cache does not fit either.** It is 1,024 frames — **4 MiB** —
against **308 KiB** of free SRAM, so most of the cache itself lives in the badge's
8 MiB of encrypted off-chip PSRAM swap. Which means a *cache hit* can be a swap
fault, and a Xous swap fault costs about **4 ms**. A 4 KiB page fetched from the
laptop over the USB cable costs about **2 ms**.

> Fetching a page of guest memory from a laptop, over a cable, across a framed
> serial protocol, is **faster** than reading it out of the badge's own memory.

That inversion is the strangest true thing in this repository, and it is not a
rhetorical flourish — both numbers were measured on hardware by `badge/probe`.

**5. A boot takes tens of minutes.** Booting this kernel to a shell is
**173,500,000 guest instructions** (exact — the laptop dry run asserts it). The
badge interprets them at **146,000–207,000 instructions per second**, settling
around 190–207 K once the caches warm. One real report line, mid-run, from
`badge/logs/2026-09-01-throughput-and-input-desync.txt`:

```
rv64 rate: insn=117626295 wall=615452ms link=23521ms svc=3258ms cpu=588673ms ips=199816
```

117.6 million instructions in 615 seconds. Of that 615 s, **23.5 s — 3.8% — was
the cable.** The rest was the interpreter. You will be waiting roughly twenty
minutes for a prompt, and it is not the wire's fault.

---

## The artifact

`badge/logs/2026-09-02-INTERACTIVE-uname-console.txt`, tail — a human typing at
the guest shell, over the badge link, and the guest answering:

```
[   16.358601] Freeing unused kernel image (initmem) memory: 204K
[   16.359864] Kernel memory protection not selected by kernel config.
[   16.361207] Run /init as init process

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

And the head of the same file:

```
[    0.000000] Linux version 6.12.103 (nixbld@localhost)
               (riscv64-unknown-linux-gnu-gcc (GCC) 15.3.0, GNU ld (GNU Binutils) 2.46)
               #1-NixOS Sun Aug  9 18:23:28 UTC 2026
```

`nixbld@localhost` and `#1-NixOS` are the point. The kernel is
`pkgsCross.riscv64` out of nixpkgs-unstable, built by the flake in this
repository; busybox is a statically linked riscv64 build from the same nixpkgs.
`6bcwi3dcynnbc2m5d8jq4vp7wblzjvcb` is a real `/nix/store` hash, read back at
runtime by `ls`-ing `/nix/store` inside the guest — not a string typed in to look
like one. The `~ #` is a busybox `ash` prompt; `rv64-host serve --input` forwards
keystrokes from the laptop's terminal to it as `ConIn` frames.

Every instruction behind those lines was fetched a 4 KiB page at a time over
USB-serial from a laptop and interpreted on a 32-bit microcontroller.

The badge's OLED is 128×128 pixels, which is a 16×8 character grid. The guest's
`/init` is written to that width, so the eight lines above fill the screen
exactly:

```
┌────────────────┐
│riscv64 Linux   │
│6.12.103        │
│baochip rv64 emu│
│                │
│/nix/store:     │
│6bcwi3dcynnbc2m5│
│d8jq4vp7wblzjvcb│
│  busybox-static│
└────────────────┘
```

The 32-character hash lands on exactly two full rows.

`badge/logs/README.md` says what each transcript in that directory shows and why
it was kept. [`WRITEUP.md`](WRITEUP.md) tells the story this document
deliberately does not: why the guest had to be 64-bit, the four defects found in
shipped badge firmware, and how each was cornered.

---

## Run it without a badge

**Start here.** You do not need the hardware, you do not need to flash anything,
and nothing about this is irreversible. The emulator core is portable `no_std`
Rust and is the *same code* on the laptop and on the badge — the only
platform-specific piece is one trait, `MemBacking`, which is a file here and a
USB link there. So the laptop runner boots the same guest, the same way, in about
**five seconds** instead of twenty minutes.

You need [Nix with flakes](https://nixos.org/download/) and a Rust toolchain.
Works on Linux and macOS; see [Platform notes](#platform-notes-linux-and-macos)
for what differs. **On macOS, building the guest kernel needs a Linux builder** —
that is the one thing that can stop you on step one, and it is explained there.

```sh
git clone https://github.com/hriday/dc34-baonix
cd dc34-baonix

nix develop            # builds the guest, exports GUEST_KERNEL/GUEST_DTB/GUEST_INITRAMFS

cargo run --release -p rv64-host -- \
  --kernel "$GUEST_KERNEL" \
  --dtb    "$GUEST_DTB" \
  --initrd "$GUEST_INITRAMFS" \
  --mem    /tmp/guest.img
```

That boots riscv64 Linux to a busybox shell on your terminal, and prints the
page-cache and MMU-walk counters to stderr when it exits. `--mem` is the 32 MiB
backing file for guest RAM; it is opened with `truncate(true)`, so every run
starts from a zeroed image and it is not a resumable snapshot.

If you would rather not enter the dev shell:

```sh
nix build .#guest      # -> ./result/{Image,guest.dtb,initramfs.cpio.gz}
cargo run --release -p rv64-host -- \
  --kernel result/Image --dtb result/guest.dtb \
  --initrd result/initramfs.cpio.gz --mem /tmp/guest.img
```

Useful flags: `--frames <n>` sets the resident page-cache size (default 256; the
badge uses 1,024), and `--max-insns <n>` is the safety valve against a guest that
never terminates. `rv64-host --help` lists them all.

**Use `--release`.** Booting Linux is 173.5 million emulated instructions: about
five seconds optimized, minutes not.

### What happens, and what you can type

The kernel messages scroll past, `/init` prints the banner, and you land at a
busybox prompt:

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

**And then nothing, because this mode has no keyboard.** `rv64-host`'s standalone
runner deliberately has no stdin plumbing — the guest's UART has nothing feeding
it, so the shell blocks on a read that never completes and the emulator spins
there. That is not a crash; it is a shell waiting for input that cannot arrive.
Typing at the guest is a *badge* feature, because it is `serve --input` that
carries keystrokes as `ConIn` frames — see [Using it](#using-it) below.

Two consequences worth knowing before you start it:

- **Give it `--max-insns`** if you want it to stop on its own, because a guest
  sitting at a prompt never issues an SBI shutdown. `--max-insns 400000000`
  comfortably clears the 173.5 M a boot needs and then ends the run.
- **The counters print when the run ends** — on an SBI shutdown or on
  `--max-insns` — not on `Ctrl-C`, which kills the process before it can report:

  ```
  instructions executed: 173500000
  page cache (256 frames): hits=… misses=… evictions=… writebacks=… declined=…
  mmu walks: 2169838
  ```

If you want to see the *badge's* run loop drive the same guest to a prompt on
your own machine — the real loop, the real protocol, over a socket instead of
USB — that is `badge/app/tests/dry_run.rs`, run by `./check.sh`. It is a test
rather than an interactive tool: it asserts the guest reaches a shell and does
not offer you a keyboard.

### Running the whole test suite

```sh
./check.sh             # from the repository root, inside `nix develop`
```

One command, and it is the one to run before any flash. It exists because
`cargo test --workspace` does **not** cover this project — see
[step 3](#3-the-badge-app) below.

The interesting test is `badge/app/tests/dry_run.rs`: it boots the real guest to
a shell **through the badge's real run loop**, over a socket instead of USB. That
is what makes a laptop a meaningful test of the badge port, rather than a
different program that happens to work.

---

## What it is not

- **Not fast.** 146,000–207,000 guest instructions per second, measured on the
  badge. A boot is 173,500,000 instructions, so a boot takes tens of minutes.
- **Not useful.** It cannot do anything a laptop cannot do better, and the laptop
  is right there, plugged in, holding its RAM.
- **Not a general-purpose computer.** Unplug the cable and the guest stops on its
  next page fault. There is no storage on the badge for a 32 MiB guest and there
  is no plan for one.
- **Not reliable.** The transmit path can still wedge inside `serial_send`; see
  [The firmware patches](#the-firmware-patches). Runs fail. The transcripts in
  `badge/logs/` include the ones that did.
- **Not a supported thing.** Installing it destroys secrets on your badge,
  permanently. See [Flashing](#flashing-the-badge).

---

## How it works

```
  laptop                                    DEF CON 34 badge
  ──────                                    ────────────────
  rv64-host serve                           rv64-badge (a Xous app)
    guest RAM: a 32 MiB file       USB-CDC    ├── Cpu       RV64IMAC, M/S/U
    kernel + dtb + initramfs   ◄───────────►  ├── Mmu       Sv39
    keyboard  ──► ConIn                       ├── Sbi/Clint/Uart
    guest console ◄── ConOut                  ├── PageCache 1024 frames (4 MiB)
                                              └── OLED      16×8 character grid
```

The wire protocol (`crates/rv64-proto`) is six frame kinds — `ReadReq`,
`ReadResp`, `WriteReq`, `WriteAck`, `ConOut`, `ConIn` — each with a length, a
type byte and a bitwise CRC-32 (no table; this runs on a microcontroller). The
badge is the client: it asks for pages, writes dirty ones back, mirrors guest
console output to the laptop, and receives keystrokes.

The emulator core (`crates/rv64`) is portable `no_std` and is the *same code* on
the laptop and on the badge. The only platform-specific piece is one trait,
`MemBacking`: a file on the laptop, a USB link on the badge.

| directory | what it is |
|---|---|
| `crates/rv64` | the emulator core: CPU, Sv39 MMU, SBI, CLINT, 8250 UART, page cache |
| `crates/rv64-host` | laptop side — a standalone runner, and `serve`, the page server |
| `crates/rv64-proto` | the wire protocol, shared by both ends |
| `crates/rv64-difftest` | differential harness against Spike |
| `nix/guest` | the guest: kernel config, device tree, initramfs, `/init` |
| `badge/app` | the Xous app — the payload that runs on the badge |
| `badge/probe` | a separate Xous app that measures the badge (throughput, latency, free memory) |
| `badge/*.patch` | firmware repairs against `xous-core` |
| `badge/logs` | hardware transcripts, including the one above |
| `badge/README.md` | the long version of everything below, with file-and-line citations |
| `docs/xous-api-notes.md` | verbatim `xous-core` extracts, cited by file and line |
| `prebuilt/` | the three `.uf2` images that produced those transcripts, with their shas |
| `WRITEUP.md` | the story: why 64-bit, four firmware defects, how they were found |

---

## The hardware

The Baochip-1x on the DEF CON 34 badge (`board-baosec` + `oem-baosec-lite` in
xous-core terms). Every constraint here shapes the design, so the table has a
third column.

| | | why it matters |
|---|---|---|
| CPU | RV32-IMAC with MMU, 350 MHz | **32-bit host, 64-bit guest → interpretation, not virtualization.** The 350 MHz figure is from the project's design doc and was *not* independently verified; the numbers under [Speed](#speed) are measured, and are the ones to trust. |
| SRAM | 2 MiB on-die | ~308 KiB free after Xous, its kernel and eleven services. The page cache is 4 MiB, so most of it lives in swap. |
| RRAM | 4 MiB on-die | Reserved for the kernel. `APP_RRAM_LEN == 0` on this board, so **there is no app region**: shipping code means replacing the whole swap image. |
| Off-chip memory | 8 MiB PSRAM | Xous swap, encrypted, ~4 ms per fault. Not addressable by us directly. |
| Display | 128×128 mono OLED | 16 columns × 8 rows of the mono font. That is the whole guest console. |
| Host link | USB-C: CDC serial + UF2 mass storage | Firmware load *and* the guest's entire memory bus. |
| Storage | none — no SD slot | **This is why the guest's RAM lives on the laptop.** There is nowhere else to put 32 MiB. |

Three consequences worth stating outright:

**No storage for the guest is why RAM is remote.** 8 MiB of swap cannot hold a
32 MiB guest, and there is no filesystem to put an image in. The only device on
the badge big enough to back guest RAM is the USB cable.

**A 32-bit host running a 64-bit guest is why this is an interpreter.** There is
no way to run RV64 instructions on an RV32 core.

**nixpkgs is why the guest is riscv64 in the first place.** nixpkgs has no
`riscv32-linux` platform — riscv32 exists there only as a bare-metal cross
target. `riscv64-linux` is supported. Since the entire point of the project is
that nixpkgs built the image, the guest architecture was forced to 64-bit, and
with it everything above. [`WRITEUP.md`](WRITEUP.md) follows that one fact
through to the rest of the design.

---

## Build it yourself

Four separate build systems have to line up, and none of them knows about the
others: nix for the guest, the ordinary Rust toolchain for the host and emulator,
a hand-installed Xous sysroot for the badge app, and `xous-core`'s own `xtask`
for the firmware image.

**If you only want to see it run, stop after step 1** — that is the
[no-badge](#run-it-without-a-badge) path above, and it is complete on its own.
Steps 2–5 are only needed to put it on hardware.

Three things here cost days. Each is called out where it bites:

- the Xous sysroot must match your `rustc` **exactly**, and `rustup update`
  destroys it silently (step 2);
- `xous-app-uf2` **does not compile** at the pinned xous-core revision (step 5);
- `--git-rev` and `--git-describe` are **mandatory**, and a wrong `--git-rev`
  silently produces an image the loader rejects (step 5).

### 1. The guest, and the host tools

```sh
nix develop            # sets GUEST_KERNEL, GUEST_DTB, GUEST_INITRAMFS
nix build .#guest      # kernel Image, guest.dtb, initramfs.cpio.gz
cargo build --release -p rv64-host
```

`nix develop` builds the guest as a side effect of entering the shell, so the
first entry on macOS needs an `aarch64-linux` builder — see
[Platform notes](#platform-notes-linux-and-macos). On Linux it builds natively
and there is nothing to configure.
The dev shell also sets `RV64_REQUIRE_SUITES=1`, which turns "this suite skipped
because an image was missing" into a failure rather than a green run that tested
nothing.

At this point you can boot the guest with no badge at all — see
[Run it without a badge](#run-it-without-a-badge). That reaches the same shell in
about five seconds. Everything after this point is the part that makes it twenty
minutes and puts it on a badge.

### 2. The Xous toolchain — the first trap

`riscv32imac-unknown-xous-elf` is not a rustup target. It needs a prebuilt
sysroot from
[`betrusted-io/rust`](https://github.com/betrusted-io/rust/releases), unzipped
into your stable sysroot, and **its version must match your `rustc` exactly**.
These artifacts were built with 1.97.1:

```sh
rustc --version                                  # -> 1.97.1
curl -L -o xous-tc.zip \
  https://github.com/betrusted-io/rust/releases/download/1.97.1.1/riscv32imac-unknown-xous_1.97.1.zip
unzip -o xous-tc.zip -d "$(rustc --print sysroot)"
cat "$(rustc --print sysroot)/lib/rustlib/riscv32imac-unknown-xous-elf/RUST_VERSION"
```

Three things to know.

- The release **tag** is `<rustc-version>.N`, where `N` is a rebuild counter you
  cannot derive from the rustc version. Look it up on the releases page; the zip
  inside the release is named without it.
- **`rustup update` destroys this sysroot silently.** The files were unzipped
  into the `stable` channel's sysroot, `rustup` has no idea the target exists,
  and the next build fails with `can't find crate for 'std'` — which reads like a
  broken `Cargo.toml` and is not. The recovery is the same `curl`/`unzip` with
  the new version's tag.
- **Check the `RUST_VERSION` stamp before every build.** That last `cat` is the
  check. `check.sh` skips the hardware type-check *loudly* rather than silently
  when the sysroot is missing, because a silent skip there is the one that costs
  a hardware cycle.

### 3. The badge app

```sh
./check.sh    # from the repository root, inside `nix develop`
```

That is the one command that runs everything, and it exists because
`cargo test --workspace` does **not** cover this project. `badge/app` is a
standalone workspace on purpose — so Cargo does not drag xous-core's
target-specific dependencies into the root lock file — which means its two
integration tests, the dry run and the OLED boot test, are never even *compiled*
from the root. Those two are the entire argument that the badge port works.

`check.sh` runs the workspace tests, clippy, the badge app's tests including the
dry run, and `cargo check --target riscv32imac-unknown-xous-elf`, which is the
only thing standing between a typo in a syscall and a flash.

Then build the payload:

```sh
(cd badge/app && cargo build --release --target riscv32imac-unknown-xous-elf)
```

### 4. The firmware image

```sh
BADGE=$PWD/badge
XC=/path/to/xous-core          # NOT under /tmp -- see below
git clone https://github.com/betrusted-io/xous-core "$XC"
git -C "$XC" checkout 9844906ddc1214438d0d942d2db2922846ae4722

# Order matters for the first two: serialrx's context includes serialflush.
patch -d "$XC" -p1 < "$BADGE/usb-bao1x-serialflush-repair.patch"
patch -d "$XC" -p1 < "$BADGE/usb-bao1x-serialrx-repair.patch"
patch -d "$XC" -p1 < "$BADGE/usb-bao1x-drop-in-completion-reset.patch"
patch -d "$XC" -p1 < "$BADGE/xous-log-usb-mirror-nonblocking.patch"
patch -d "$XC" -p1 < "$BADGE/xous-app-uf2-repair.patch"     # needed for step 5

# Do NOT apply bao1x-hal-usb-in-completion.patch. It is the writeback regression.

APP="$BADGE/app/target/riscv32imac-unknown-xous-elf/release/rv64-badge"
(cd "$XC" && cargo xtask baosec-lite "$APP~swap")

mkdir -p "$BADGE/app/out"
cp "$XC"/target/riscv32imac-unknown-xous-elf/release/{loader,xous,swap}.uf2 \
   "$BADGE/app/out/"
```

`$XC` must be at exactly that revision. It supplies the kernel, the loader, all
eleven services and the signing keys, and — the part that bites — **the swap
image's encryption nonce is derived from its HEAD commit**. A mismatched
checkout produces a `swap.uf2` the badge's loader will not decrypt, and the
symptom is a badge that does nothing at all.

**Do not put the checkout or the installed tool under `/tmp`.** macOS sweeps
`/tmp` on an idle timer and took most of a xous-core checkout with it between two
hardware runs.

### 5. App-only updates, and the remaining traps

Once developer mode has been tripped (see below), rebuilding only `swap.uf2`
takes seconds instead of rebuilding the kernel and eleven services:

```sh
cargo install --locked --path "$XC/tools" --bin xous-app-uf2 \
  --root "$PWD/target/xous-tools"
(cd badge/app/out && ../../../target/xous-tools/bin/xous-app-uf2 --swap \
  --git-rev      9844906ddc1214438d0d942d2db2922846ae4722 \
  --git-describe v0.10.2-beta1-153-g9844906dd \
  --elf ../target/riscv32imac-unknown-xous-elf/release/rv64-badge)
```

**`xous-app-uf2` does not compile at the pinned revision.** It is a workspace
member nothing in `cargo xtask` builds, and it bit-rotted when post-quantum
signing was threaded through `SwapWriter::encrypt_to` without updating this
caller: a dead binding referencing two identifiers that do not exist in the file,
a missing argument, and an unconstrained type parameter.
`xous-app-uf2-repair.patch` fixes all three — apply it in step 4, before
`cargo install`. (The published `xous-tools 0.1.2` on crates.io *does* build, but
predates PQ signing and emits an image 3,840 bytes short of a valid one, which
fails closed on any device with the `REQUIRE_PQ` counter set. Do not use it.)

**`--git-rev` and `--git-describe` are mandatory here.** Both default to running
`git` in the *current directory*, which is **this** repository, not xous-core.
This repository has no tags, so the tool dies with `SemVer::from_git: no major
version`; and `--git-rev` is the swap encryption nonce, so a wrong value silently
produces an image the loader rejects. Pass both literally, every time.

You can check what a built image was packed against — the nonce at offset 36 is
the low 16 hex digits of the commit:

```sh
xxd -s 36 -l 8 -p badge/app/out/swap.uf2      # -> 2db2922846ae4722
```

which is the tail of `9844906ddc1214438d0d942d2db2922846ae4722`.

---

## Flashing the badge

Prebuilt images are in [`prebuilt/`](prebuilt/), with their sha256 sums and the
commits they were built from. **They are dev-key-signed, and flashing the loader
trips developer mode.** Read the rest of this section before using them.

### Read this first: the cost is irreversible

**Flashing a dev-key-signed loader trips developer mode.** The badge's
provisioned secrets are *erased* — FIDO2 credentials, vault contents, the
attested-boot property, badge-to-badge light-pattern exchange — and a one-way
counter is incremented. Reflashing stock firmware restores the software and
**does not restore the secrets or decrement the counter**. There is no undo.

Two details that make this exact rather than scary:

- `cargo xtask baosec-lite` signs the loader and kernel with `devkey/dev.key`.
  That dev-signed *loader* is what does it: boot1 validates it, calls
  `erase_secrets()`, increments the counter, and only then does the loader accept
  a dev-signed swap image. Nothing about flashing the swap partition alone can
  put a badge into developer mode.
- It therefore fails **closed**. A swap-only flash on a factory badge halts inside
  the loader *before* erasing anything: no secrets lost, no counter incremented,
  recoverable by copying stock `swap.uf2` back. The cost of that particular
  mistake is a flash cycle, not a badge.

The counter stops incrementing at 15, so the irreversible cost is paid exactly
once. It is still paid.

Save a copy of your badge's stock `loader.uf2`, `xous.uf2` and `swap.uf2` before
you start. Nothing here redistributes them.

### Procedure

Nothing in this repository flashes anything. Copying UF2s onto a badge is a
deliberate human act.

1. Confirm the three files exist in `badge/app/out/` (or `prebuilt/`) and that
   you know which build they came from.
2. Hold **PROG** (the button closest to the USB connector) while plugging the
   badge in. It enumerates as a mass-storage volume labelled **`BAOCHIP`** —
   `/Volumes/BAOCHIP` on macOS, wherever your automounter puts it on Linux.
3. **First flash: copy all three** — `loader.uf2`, `xous.uf2`, `swap.uf2`. A
   dev-signed `swap.uf2` alone does not boot on a factory badge.
4. `sync`, then cleanly unmount the volume — `diskutil unmount` on macOS,
   `udisksctl unmount` or `umount` on Linux. See
   [Platform notes](#platform-notes-linux-and-macos).
5. Press **PROG** again to run.
6. **Start the page server before you press PROG**, not after — the badge asks
   for its first page immediately and gives up after two seconds. See
   [Using it](#using-it).

**Every flash after that: copy `swap.uf2` alone.** The loader and kernel on the
badge are unchanged, and copying the stale copies sitting beside it adds nothing
but two more chances to copy the wrong file. A change to `usb-bao1x` or any other
service is *not* an app-only update — that lives in `xous.uf2` and needs the full
three-file rebuild.

**Backing out** is the same steps 2–5 with your own stock images.

---

## Using it

The badge is the client. It asks the laptop for its first page **as soon as it
boots**, there is no startup delay, and the transport gives up after about two
seconds. So the page server has to be running before the badge is. A human
cannot win that race by hand, which is why `serve-wait.sh` exists: it polls for
the CDC node and starts serving the instant it appears.

### Start the server, then the badge

```sh
./badge/serve-wait.sh badge/logs/run.txt --pace-ms 1 --input
```

Leave that running and power-cycle the badge as often as you like; it
re-attaches on its own. The three arguments that matter:

- **`--input`** is what makes typing possible at all. Without it the guest boots
  and you watch; nothing you type reaches it. It is a flag rather than an
  `isatty` check on purpose — `serve-wait.sh` is meant to be left running
  unattended, and auto-raw-mode in a backgrounded job means SIGTTIN, a stopped
  server and a stolen keyboard on exactly the runs where nobody is watching.
- **`--pace-ms 1`** is not optional today. It is the host-side half of the
  workaround for the badge's single-buffer bulk IN endpoint; see
  [The firmware patches](#the-firmware-patches).
- **The transcript path** (`badge/logs/run.txt` here) gets the badge's own
  diagnostics and every byte that was *not* a frame. Guest console output goes
  to a separate `-console.txt` beside it, via `--console`. Keeping them apart is
  deliberate: merged, there is no way to tell "the guest printed this" from
  "this arrived unframed on the wire", and that question has decided two
  investigations.

With `--input`, guest console output is teed — it goes to the `--console` file
*and* to your terminal, so you have a live view to type against and a durable
transcript afterwards.

Driving `rv64-host serve` yourself, without the wrapper, looks like this
(substitute your own device path — see [Platform notes](#platform-notes-linux-and-macos)):

```sh
cargo run --release -p rv64-host -- serve \
  --kernel "$GUEST_KERNEL" --dtb "$GUEST_DTB" --initrd "$GUEST_INITRAMFS" \
  --mem /tmp/rv64-guest-mem.img \
  --port /dev/cu.usbmodem1234       # macOS;  /dev/ttyACM0 on Linux
  --console badge/logs/run-console.txt --pace-ms 1 --input
```

It prints `raw mode set` when it has the port configured, and refuses to serve
if it cannot — a CDC node opened in canonical mode corrupts the first page that
crosses it.

### The control keys

This is the part that is not guessable, and you will need it.

| key | what it does |
|---|---|
| `Ctrl-C` | **Ends `serve`** and restores your terminal. It is *not* forwarded to the guest. |
| `Ctrl-] <key>` | Sends `<key>` through to the guest verbatim. |
| `Ctrl-] Ctrl-C` | Therefore: **interrupts whatever is running inside Linux**, without killing the server. |

That distinction matters more here than on a normal terminal. A boot is twenty
minutes; a stray `Ctrl-C` that ended the server rather than the guest's command
would cost all of it. So `serve --input` handles `Ctrl-C` itself, in band, and
`Ctrl-]` is the escape that reaches the guest.

Everything else you type is forwarded byte for byte: no echo, no line buffering,
no line discipline. Your terminal is restored when `serve` exits.

A non-tty stdin — a pipe or a file — is accepted and forwarded with no termios
call, which is how you script a session. Drive `rv64-host serve` directly for
that rather than the wrapper, so nothing else is competing for stdin:

```sh
printf 'uname -a\n' | cargo run --release -p rv64-host -- serve \
  --kernel "$GUEST_KERNEL" --dtb "$GUEST_DTB" --initrd "$GUEST_INITRAMFS" \
  --mem /tmp/rv64-guest-mem.img --port /dev/cu.usbmodem1234 \
  --console badge/logs/run-console.txt --pace-ms 1 --input
```

### What to run at the prompt

The guest is a statically linked busybox from nixpkgs. The applets symlinked
into `/bin` are deliberately few — `sh`, `mount`, `ls`, `cat`, `tr`, `uname`,
`cut`, `basename` — but the full busybox binary is there, so anything else it
was built with is reachable as `busybox <applet>`.

```sh
uname -a                  # the whole banner: version, build host, #1-NixOS
uname -r                  # 6.12.103
ls /nix/store             # the real store path the initramfs was built from
cat /proc/cpuinfo         # what the guest thinks it is running on
cat /proc/meminfo         # 32 MiB, most of which lives on your laptop
busybox free              # anything not in /bin, if this busybox was built with it
```

`sh: <name>: not found` for something busybox normally has means it was not
symlinked into `/bin`, not that it is absent — try `busybox <name>`. The applet
list is in `nix/guest/initramfs.nix` and is short on purpose: the archive is
assembled on the build host, where `busybox --install` cannot run.

**Set your expectations.** Every command forks busybox, which faults in pages,
each of which is a request over the serial cable and possibly a swap fault on
the badge. A command takes **seconds**, not milliseconds. A keystroke takes a
second or two to echo. That is normal and it is not a hang — the interpreter is
running at about 200,000 instructions per second and it has work to do. If you
want to know whether it is alive, watch the `rv64 rate:` lines in the transcript
or the spinner in the bottom-right cell of the badge's screen, which ticks only
into a blank cell so it can never eat a character of a store path.

### When you are done

**The badge parks at the prompt.** It does not exit, reboot or blank the screen,
so the display stays up for as long as you like — which is the point, since
photographing it is most of what this project is for.

**Pulling the cable is a stop, not a suspend.** The guest's RAM is on the laptop.
Unplug it, or `Ctrl-C` the server, and the guest continues exactly until its next
page fault and then stops there permanently. There is no resume: `--mem` is
rebuilt from the guest images on every `serve` invocation, by design, so a boot
always starts from clean guest RAM rather than from whatever the last run left
behind.

---

## Platform notes: Linux and macOS

**Everything on hardware was done on an aarch64 Mac.** Building is expected to
work identically on Linux and the substitutions below are given in good faith,
but **the flashing path and the serial paths have never been run on Linux.** They
are marked accordingly. Nothing in the pure-software path
([Run it without a badge](#run-it-without-a-badge), `./check.sh`) is
macOS-specific.

### Building

| | macOS | Linux |
|---|---|---|
| Nix | flakes must be enabled (`experimental-features = nix-command flakes`) | same |
| Guest kernel | **needs a Linux builder** — see below | builds natively, nothing to set up |
| Rust workspace | native | native |
| Xous sysroot | one archive, host-independent — see below | same archive |
| `cargo xtask baosec-lite` | verified | expected to work; unverified |

**The guest kernel and a Linux builder.** A Linux kernel cannot be built on
Darwin — kbuild bootstraps its own host tools (`fixdep`, `conf`, …) and the tree
assumes a Linux host. `flake.nix` therefore pins the kernel's *build* platform:
if you are already on Linux it uses your own system and there is nothing to
configure; if you are on Darwin it asks for `<your-cpu>-linux`, which on an
Apple-silicon Mac means `aarch64-linux`. Nix will not conjure that, so a Mac
needs either a remote builder or a local Linux VM registered as one — see
[the nix manual on distributed builds](https://nix.dev/manual/nix/stable/advanced-topics/distributed-builds).
The initramfs needs it too.

This is not incidental cost that could be optimised away: it is what makes the
boot test unable to silently skip for a missing image.

**Checksums.**

```sh
shasum -a 256 prebuilt/*.uf2      # macOS
sha256sum     prebuilt/*.uf2      # Linux (coreutils)
```

### The Xous sysroot, on either host

The `riscv32imac-unknown-xous-elf` target is not a rustup target; it is a zip
from [`betrusted-io/rust`](https://github.com/betrusted-io/rust/releases)
unpacked into your toolchain's sysroot. Two things a reader will otherwise get
wrong:

**The sysroot path is host-specific; the way to find it is not.** It is
`~/.rustup/toolchains/1.97.1-aarch64-apple-darwin/` on an Apple-silicon Mac and
`~/.rustup/toolchains/1.97.1-x86_64-unknown-linux-gnu/` on a typical Linux box.
Do not type either. `rustc --print sysroot` prints the right one on any host, and
every command in this README uses it.

**There is only one archive, and that is correct.** It contains
`lib/rustlib/riscv32imac-unknown-xous-elf/` — *target* libraries for a riscv32
machine, not host binaries — so the same download serves any host. You will not
find a per-platform variant on the releases page because there isn't one. If you
want to confirm before unpacking into your sysroot, `unzip -l xous-tc.zip` shows
the layout.

**Pin your rustc, and know the failure signature.** The sysroot must match
`rustc --version` *exactly*, and unzipping into the `stable` channel's sysroot
means the next `rustup update` silently deletes it — `rustup` does not know the
target exists. The build then fails with `can't find crate for 'std'`, which
reads like a broken `Cargo.toml` and is not. Installing the version as its own
pinned toolchain avoids the whole class:

```sh
rustup toolchain install 1.97.1
rustup default 1.97.1                    # or `rustup override set 1.97.1` in this repo
rustc --version                          # -> rustc 1.97.1
# ... unzip into "$(rustc --print sysroot)" as in step 2 ...
cat "$(rustc --print sysroot)/lib/rustlib/riscv32imac-unknown-xous-elf/RUST_VERSION"
```

`check.sh` skips the hardware type-check *loudly* rather than silently when the
sysroot is missing, so a wiped sysroot shows up before a flash rather than after.

### Serial devices — *verified on macOS only*

| | macOS | Linux (expected) |
|---|---|---|
| CDC node | `/dev/cu.usbmodem*` | `/dev/ttyACM*` |
| Trap | use `cu.*`, **never `tty.*`** — `tty.*` blocks on DCD, which a CDC-ACM gadget never asserts | be in the `dialout` group (or your distribution's equivalent), or use `sudo`; the scripts set `CLOCAL` explicitly |

`serve-wait.sh` and `reattach.sh` poll **both** globs, so they need no editing on
either platform. `echo-host.py` and `rv64-host serve --port` take the path as an
argument. `badge/echo-host.py` carries its own note that its Linux path is
untested against real hardware; that remains true.

### The UF2 volume — *verified on macOS only*

Hold **PROG**, plug in, and the badge enumerates as a mass-storage volume
labelled `BAOCHIP`.

| | macOS | Linux (expected) |
|---|---|---|
| Mount point | `/Volumes/BAOCHIP` | wherever the automounter puts it — `/run/media/$USER/BAOCHIP` or `/media/$USER/BAOCHIP` — or mount it by hand |
| Copy | `cp swap.uf2 /Volumes/BAOCHIP/` | `cp swap.uf2 /run/media/$USER/BAOCHIP/` |
| Flush | `sync` | `sync` |
| Unmount | `diskutil unmount /Volumes/BAOCHIP` | `udisksctl unmount -b /dev/sdX1`, or `umount` |

`diskutil` is macOS-only. Unmount cleanly before pressing PROG again: a UF2
bootloader that receives a partial file writes a partial image.

---

## The firmware patches

Four defects in shipped badge firmware, found by chasing them across twenty-three
hardware runs. All apply against `xous-core@9844906` with `patch -p1`, all carry
their rationale as comments in the source they touch, and all are written to be
reported upstream. If you work on this hardware, these are probably the most
reusable thing here. [`WRITEUP.md`](WRITEUP.md) tells the story of how each was
found.

**`usb-bao1x-serialflush-repair.patch` — a panic, and a wedge.** The `SerialFlush`
handler does `buf.d.copy_from_slice(serial_buf.drain(..n).as_slice())` where
`buf.d` is the client's freshly constructed `Vec::new()` — length 0.
`copy_from_slice` panics unless the lengths match, so **the flush can only deliver
when there is nothing to deliver, and panics whenever there is.** The IRQ arrival
path two hundred lines above gets it right with `extend_from_slice`; the flush
path was never fixed. One word. The same handler also `continue`s past the
listener release when the device is not `Configured`, so a disconnect or a
charge-only port wedges a blocked client permanently — which is precisely the case
the watchdog exists for.

**`usb-bao1x-serialrx-repair.patch` — a shared staging buffer, and only a count
crossing.** The USB interrupt handler received each CDC packet into one shared
512-byte `serial_rx` buffer and handed the consumer a *byte count* rather than the
bytes. A second packet arriving before the consumer ran overwrote the first. This
one took five hardware rounds to corner, because every layer above it was
provably correct: the address instrumentation showed rkyv's archive offsets,
lengths and resolved pointers all matching their host-computed predictions
exactly, while the page itself held RISC-V `nop` padding. What identified it was
periodicity — the payload repeated with a **1024-byte period**, `base` and
`base+1024` byte-identical, `+512` different. Wire data does not repeat like that;
two 512-byte blocks cycling through a 3,840-byte payload is stale re-reads of a
two-packet buffer. The patch copies each packet at IRQ time.

**`xous-log-usb-mirror-nonblocking.patch` — the log server deadlocking the system
it reports on.** `usb_send_str` mirrors each log line to `usb-bao1x` with
`Buffer::send`, under a comment saying *"this API doesn't block."* It does: the
kernel answers `ServerQueueFull` for a blocking `SendMessage` by parking the
caller and retrying. Only `try_send_message` returns the error. That closes a
cycle with a service that logs: `usb-bao1x` calls `log::error!` (a blocking lend
to the log server) → the log server mirrors the line back to `usb-bao1x` → whose
queue is full, which is *exactly the condition its own log line was reporting* →
the log server parks forever. Every process that logs then blocks behind it. No
fault, no frames, no output at all. Observed as two `serial rx ring overflowed`
lines followed by five minutes of silence. A blocking call from the log server
into a service that logs is a deadlock in whichever direction it is written.

**`bao1x-hal-usb-in-completion.patch` — written, tried, and reverted.** The
underlying bug is real: `CorigineWrapper::write` copies every bulk IN packet into
a **single** 512-byte hardware buffer and enqueues a transfer at that address with
no check that the previous one completed — nine packets handed over back to back
are nine writes into one buffer. The patch makes the endpoint return `WouldBlock`
while a transfer is in flight, which is exactly the answer the layers above were
written for.

It is not applied, because **the refusal is invisible from above**.
`SerialPort::write` returns bytes *buffered*, and tolerates its own `flush`
failing with `WouldBlock` — so a refused packet still comes back as `Ok(512)`. The
refused bytes then wait for a completion interrupt (on a driver whose own comment
says interrupts go missing) or a 5 ms watchdog, neither of which is on the send's
critical path. A stranded *tail* carries the frame's CRC, so the host's decoder
holds an incomplete frame silently — neither accepted nor reported as discarded.
The bisect is one variable: the boot that reached a shell had no patch and a 1 ms
transmit pace, and wrote memory back successfully; every run with the patch had
**zero** successful writebacks. The correct fix is to make the refusal
*observable* — have `usb-bao1x` report `flush`'s result alongside `total_sent` —
rather than to hide it inside `bao1x-hal`. `usb-bao1x-drop-in-completion-reset.patch`
removes the one stanza the serialrx patch carries on its behalf, so builds without
it still compile.

**This is the part that still does not work.** With that patch reverted and a 1 ms
app-side transmit pace, writebacks succeed — but the transmit path can still wedge
inside `serial_send`, and when it does the run ends there. It is not fixed. It is
avoided.

Two more upstream bugs were found that need no patch here, because both have
app-side workarounds: `xous_swapper::Swapper::garbage_collect_pages()` returns a
constant 0 to every caller on every kernel (it destructures the reply's first
field, which is the message id, instead of `arg1`); and the kernel's
`ServerQueueFull` retry path does not undo the message lend before retrying, so
crossing the receive queue depth kills the *receiving process* with a substituted
`Internal error` rather than reporting a queue-full condition. The second is a
real kernel defect and the reason the receive window is measured rather than
assumed.

---

## Speed

The run loop measures itself. Three disjoint spans — `wall` (the whole run),
`link` (every millisecond blocked inside a page exchange, accumulated inside the
transport because a page fault happens inside `Cpu::step` where the loop cannot
see it), and `svc` (the console pump, mirror, heartbeat and repaint between
slices) — with `cpu = wall − link − svc` and `ips = insn / cpu`. It reports at
slice 16 and then every 256 slices, so a run that dies before reaching a shell
still produces the number.

| | measured |
|---|---|
| Guest instructions/second, **on the badge** | **146,000 – 207,000** (190–207 K in steady state) |
| Guest instructions/second, laptop dry run (calibration) | 38,351,016 |
| Instructions in one boot to a shell | 173,500,000 (exact; asserted by the dry run) |
| MMU page-table walks in one boot | 2,169,838 (exact) |
| Page operations in one boot | ~2,952 |
| Wall time for a boot | tens of minutes (~20 for the interactive run) |
| Share of wall time spent on page I/O | **under 4%** (23.5 s of 615 s) |
| USB transmit, badge → host | 5,797 KiB/s (8 MiB of 4 KiB writes in 1,413 ms) |
| 4 KiB page round trip | min 2 / mean 2.0 / max 3 ms |
| Largest receive burst the badge absorbs | 32 KiB, at 35 KiB/s sustained |
| Xous swap fault | ~4 ms |

The headline is two of those rows together: **page I/O is under 4% of wall time,
so this is compute-bound, not I/O-bound.** The wire is not the problem. The
interpreter is. An earlier explanation of the forty-minute boot blamed the
host-side pacing; the instrument closed that account and showed the pacing was
about five minutes of forty.

Note the honest caveat the instrument itself cannot resolve: a page-cache *hit* on
a frame the Xous swapper has paged out costs a swap fault, and that time lands in
`cpu` where nothing here can see it. The cache is 1,024 frames — 4 MiB — against
~308 KiB of free SRAM, so most frames are swap-backed and only locality keeps that
from dominating. This is the inversion from
[The setup, stated plainly](#the-setup-stated-plainly): the badge's own memory is
slower than the cable.

**What it can do:** boot to a shell, run busybox, echo what you type, list
`/nix/store`, run `uname -a`. Interactive in the sense that a keystroke produces a
character in a couple of seconds.

**What it cannot do:** anything time-sensitive (the guest's own clock is
emulated and wall time is meaningless), anything needing throughput, anything
after you unplug the cable.

### The path to going faster

Profiled, not guessed, and listed here as future work rather than a promise. The
biggest single win available is a **decoded-instruction cache keyed by physical
PC**, estimated at **1.8–2.2×**: it removes fetch's page-cache traffic (~31% of
runtime), compressed-instruction expansion (5.7%), the fetch half of `translate`
(~7%), and the operand extraction and three-way opcode match (~5%). The risk that
cannot be measured on a laptop is memory: 4,096 entries at 12–16 bytes is 64 KiB
against 308 KiB of free SRAM, and if it pushes the page cache further into swap a
4 ms swap fault eats the entire win.

The profile also overturned a prior: there is already a 32-entry TLB, this guest's
0.5% miss rate makes the walks 1.9% of memory traffic, and doubling it would buy
about 1%. **The MMU is not slow because it walks. It is slow because it is called
twice per instruction.**

---

## A note on method

The project's specification said the badge's display was 18 columns wide, derived
from 128/7 — seven being the mono font's glyph width. It was wrong. Seven is the
width of a glyph's *ink*; the *advance* is `wide + kern`, `DEFAULT_KERN` is 1, and
128/8 is 16.

It was not settled by re-reading the derivation, and not settled on hardware
either. It was settled by taking xous-core's actual typesetter and blitter,
running them on the laptop into a 128×128 bit buffer, and looking at where the
pixels landed: 18 characters demonstrably wrap, and eight rows of 16 land on eight
clean 15-pixel bands. Then the guest's `/init` was corrected to 16 columns, and a
host test now asserts the grid of characters the display would show — store path
included — so a regression fails on a laptop rather than on a photograph.

At 18 the hash would have split 18+14 and run onto a ninth row that does not
exist.

That is the shape of most of the work here. The badge has no debugger, no storage,
and every run is a power cycle, so almost everything was settled by running the
real code somewhere it could be observed, or by reading the source to the point of
citing a line number — and the things that genuinely could only be answered by
hardware were measured deliberately, once, by a purpose-built probe
(`badge/probe`). The transcripts in `badge/logs/` are kept because for several of
them they are the only record that a thing happened at all.

---

## Further reading

- [`WRITEUP.md`](WRITEUP.md) — the story: why the guest had to be riscv64, the
  four firmware defects and how each was cornered, the measurements, and two
  mistakes worth reading.
- [`badge/README.md`](badge/README.md) — the long version: every syscall, every
  trap, every patch, with file-and-line citations into xous-core.
- [`badge/logs/README.md`](badge/logs/README.md) — what each hardware transcript
  shows and why it was kept.
- [`docs/xous-api-notes.md`](docs/xous-api-notes.md) — verbatim quotes of the
  xous APIs this depends on, cited by file and line, including the questions that
  could *not* be answered from the source. Read it before changing a syscall.
- [`prebuilt/README.md`](prebuilt/README.md) — the flashable images, their sha256
  sums, and exactly what went into them.

## License

MIT, for the work that is ours — see [`LICENSE`](LICENSE).

What is **not** ours, and is called out in that file:

- `badge/*.patch` are unified diffs against `betrusted-io/xous-core`. They carry
  that project's source as context and modified lines, so they are derivative
  works of xous-core and carry its terms. They are written to be reported
  upstream.
- `prebuilt/loader.uf2` and `prebuilt/xous.uf2` are compiled xous-core, with four
  of those patches applied. `prebuilt/swap.uf2` is our application in xous-core's
  swap-image container.
- `docs/xous-api-notes.md` quotes xous-core source verbatim, cited by file and
  line.
- `nix/` builds a Linux kernel and busybox from nixpkgs — GPL-2.0 artifacts,
  built by you, not distributed here.

Nothing in this repository redistributes the DEF CON 34 badge's stock firmware.

## Credits

Built on [`betrusted-io/xous-core`](https://github.com/betrusted-io/xous-core)
(the Xous microkernel and the badge's firmware) and
[`betrusted-io/rust`](https://github.com/betrusted-io/rust) (the
`riscv32imac-unknown-xous-elf` toolchain). The guest is nixpkgs-unstable's
`pkgsCross.riscv64`. The badge is Baochip-1x, from DEF CON 34.
