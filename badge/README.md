# Badge payloads

Out-of-tree Xous code for the DEF CON 34 badge (`board-baosec` + `oem-baosec-lite`
+ `bao1x`, target `riscv32imac-unknown-xous-elf`), built against
`betrusted-io/xous-core@9844906ddc1214438d0d942d2db2922846ae4722` (branch `dev`).

The API facts these crates rely on are quoted verbatim, with file and line, in
`../docs/xous-api-notes.md`. Read that before changing a syscall here.

> **A first flash must be a full matched set: `loader.uf2` + `xous.uf2` +
> `swap.uf2`.** A dev-key-signed `swap.uf2` on its own does **not** boot on a
> factory badge and does **not** trip developer mode — the loader dies instead.
> See *Which tool for which moment*, below; the reasoning is the single most
> important thing in this file.
>
> **Flashing a dev-signed loader trips developer mode.** The badge's provisioned
> secrets are erased and a one-way counter is incremented; reflashing stock
> firmware does not undo it. Nothing in this directory flashes anything. Copying
> UF2s onto the badge is a deliberate human act.

## Which tool for which moment

There are two ways to get our code onto the badge, and they are for different
moments. Getting this wrong costs a flash cycle.

**First flash — `cargo xtask baosec-lite`, all three UF2s.** The badge leaves the
factory with the `DEVELOPER_MODE` one-way counter at 0. `loader/src/platform/
bao1x/swap.rs:282-306` checks, immediately after a *successful* signature check on
a fresh swap image, whether that image was signed with the developer key; if it
was and `DEVELOPER_MODE == 0` it prints `LOADER.SWAPDIE` to a debug UART nobody
has wired and calls `die_no_std()`. Its own comment explains why it cannot do
anything else: *"we can't erase keys in the loader, because the keys have already
been locked out at this point."* Two more gates enforce the same rule
(`loader/src/phase1.rs:726-735` and `:757-765`), and the keystore service enforces
it again at runtime (`services/keystore/src/platform/baosec/store.rs:665`, `:715`,
`:813`).

`DEVELOPER_MODE` is incremented in exactly one function — `erase_secrets()`,
`libs/bao1x-hal/src/sigcheck.rs:791` — and every caller of that is on a boot0/boot1
signature-validation path (`bao1x-boot/boot0/src/main.rs:375`,
`bao1x-boot/boot1/src/secboot.rs:92`). **Nothing about flashing the swap partition
can put the badge into developer mode.** Only a dev-key-signed `loader.uf2` can,
because boot1 validating that loader is what calls `erase_secrets()`. Both happen
in one boot: boot1 erases and increments, jumps to the loader, and the loader then
finds `DEVELOPER_MODE != 0` and accepts our swap image.

This fails *closed*, which is worth knowing: a swap-only flash would have halted
inside the loader **before** erasing anything. No secrets lost, no counter
incremented, recoverable by copying stock `swap.uf2` back. The cost of that
mistake is a cycle, not a badge.

*(`DEVELOPER_MODE` stops incrementing at 15, `sigcheck.rs:791`, so repeated
developer-mode boots do not exhaust it. The irreversible cost is paid once.)*

**Every flash after that — `xous-app-uf2 --swap`.** Once the counter is non-zero,
the loader accepts a dev-signed swap image on its own, and `xous-app-uf2` rebuilds
just `swap.uf2` from one ELF in seconds instead of rebuilding the kernel and eleven
services. That is where this project will live for the rest of its life, so the
repair patch that makes that tool compile is kept, documented, and still worth
having — it is just not the tool for the first flash. Note that it can only carry
*app* changes: a change to `usb-bao1x` or any other service is in `xous.uf2` and
needs a full rebuild.

## What is here

| file | what it is |
|---|---|
| `app/` | the emulator payload, an out-of-tree Xous app — see below. `src/run.rs` is the run loop, `src/main.rs` the badge's platform leaf, `tests/dry_run.rs` the laptop's |
| `probe/` | the memory and throughput probe, an out-of-tree Xous app |
| `echo-host.py` | host counterpart: answers `REQ` with one page and `STREAM n` with n pages back to back |
| `test-echo-host.py` | pty regression suite for `echo-host.py` — 20 cases, the only part of the protocol that can be tested without a badge |
| `reattach.sh` | polls for the CDC node and re-attaches `echo-host.py` across power cycles |
| `serve-wait.sh` | the same trick for the emulator: polls for the CDC node and starts `rv64-host serve` the moment it appears. The badge asks for its first page as soon as it boots and the transport gives up after ~2 s, so a human cannot win that race by hand |
| `boot-transcript.txt` | what the last `serve-wait.sh` run captured. **Overwritten every run** — copy it aside under a dated name when a run is worth keeping, the way `probe-transcript.txt` was |
| `usb-bao1x-serialflush-repair.patch` | fixes two live bugs in the stock USB server; **required** |
| `usb-bao1x-serialrx-repair.patch` | copies each received packet at IRQ time instead of leaving it in a shared buffer; **required** for any workload that receives more than one packet of *varying* data |
| `bao1x-hal-usb-in-completion.patch` | makes a bulk IN endpoint return `WouldBlock` while a transfer is still in flight, instead of overwriting the one hardware buffer. **DO NOT APPLY — this is the writeback regression; see below.** |
| `xous-log-usb-mirror-nonblocking.patch` | makes the log server's USB console mirror `try_send` instead of `send`; **required** whenever the mirror is hooked, because `send` blocks the log server on a full USB queue and deadlocks the pair |
| `usb-bao1x-drop-in-completion-reset.patch` | removes the one stanza `usb-bao1x-serialrx-repair.patch` carries on the in-completion patch's behalf; **required** in any build that omits it, which is every build now |
| `xous-app-uf2-repair.patch` | makes `xous-app-uf2` compile; needed only for app-only updates |
| `probe/out/*.uf2` | build products, gitignored |

## `app/` — the emulator payload

A standalone workspace, for the same reason `probe/` is one: it builds for
`riscv32imac-unknown-xous-elf` against a custom sysroot and must not be pulled
into the host workspace at `../Cargo.toml`. Cargo resolves target-specific
dependencies for every platform when it writes a lock file, so a member entry
there would make `cargo test` at the repo root fetch xous-core.

Everything that can be tested on a laptop is tested on a laptop. The badge-only
code — `usbhost::UsbTransport`, the `usb_bao1x` syscalls, `oled::GfxScreen` —
is `#[cfg(target_os = "xous")]` and its dependencies are gated to match, so:

```bash
# everything, in one command -- workspace tests, badge/app tests (the dry run
# included), clippy, and the hardware type-check. `cargo test --workspace` at
# the repo root does NOT cover badge/app: it is a standalone workspace, so its
# tests are never even compiled from there.
./check.sh          # from baochip/, inside `nix develop`

cd badge/app

# the transport logic, the framing, the send loop, the accumulate-across-
# deliveries loop, the error paths, the display grid, the run loop and the
# MemBacking conformance suite -- no badge, no xous sysroot, no network
cargo test

# THE DRY RUN: boot the real guest to a shell through the badge's own run
# loop, on the laptop, against the real `rv64_host::serve` over a socket.
# Needs `nix develop` for GUEST_KERNEL/GUEST_DTB/GUEST_INITRAMFS.
cargo test --release --test dry_run -- --nocapture

# type-checks the hardware path against the real usb-bao1x and Gfx APIs.
# Needs the xous sysroot from ### Toolchain, below.
cargo check --target riscv32imac-unknown-xous-elf
```

The last command is not optional before a hardware cycle. It is the only
thing standing between a typo in a syscall and a flash.

### The dry run — `app/tests/dry_run.rs`

**Run this before every flash.** It boots the real nixpkgs guest to a busybox
prompt in ~20 seconds through `run::run`, `UsbHost`, `rv64_proto`,
`rv64::PageCache` and `OledSink` — the badge's own code, unmodified — with
exactly two things swapped:

| badge | dry run |
|---|---|
| `usbhost::UsbTransport` (`usb_bao1x` syscalls) | a TCP loopback transport, same reader-thread shape, same 3840-byte delivery cap, same `send_all` |
| `oled::GfxScreen` (`Gfx` syscalls) | `oled::FakeScreen`, which records the frames |

The host side is not a stand-in either: it is `rv64_host::serve::serve` — the
function `rv64-host serve` itself calls — on a thread, over an image laid down
by `rv64_host::load_boot_images`.

Two things a socket cannot model, and both have now bitten:

**Xous service startup.** There are no services on a laptop to be un-started, so
nothing here could have caught the ticktimer race that killed the first hardware
run. See `app/src/startup.rs`.

**The tty layer**, which is where the laptop's blocker lived: `serve --port` is a USB-CDC node, and a freshly opened
tty is in canonical mode, which mangles the first 4 KiB page that crosses it.
`crates/rv64-host/tests/rawtty.rs` covers that over a real pty; see
`crates/rv64-host/src/rawtty.rs` for why it is not an `stty` in a README.

**Reading the badge's own diagnostics.** `serve` writes two streams: guest
console output (`ConOut` frames) to **stdout**, and everything arriving on
`--port` that is not a frame to **stderr, verbatim**. The second is the log
server's USB panic mirror, which shares the CDC endpoint with the protocol —
before this, the decoder scanned past `PANIC in PID n:` looking for SYNC and
dropped it, so the first hardware failure produced no diagnosis until `serve`
was killed and a plain reader attached. Run it as

```sh
rv64-host serve --kernel … --dtb … --initrd … --mem … --port /dev/cu.usbmodem… \
    >guest.log 2>badge.log
```

and `badge.log` is the transcript.

It asserts the prompt, a genuine `/nix/store` path laid out on the 16×8 grid,
and that every frame is exactly `ROWS`×`COLS`. Beyond that it makes three
claims that are worth knowing about, because each one is a number someone will
read off a hardware transcript:

- **Instruction count.** It boots the same guest a second time through
  `rv64_host::run_until` and requires the instruction count and the MMU-walk
  count to match **exactly** — the plan's Step 5 comparison, made on a laptop
  first, so that a mismatch on hardware means the port diverged rather than
  that the two loops never agreed. Current guest: **173,500,000** instructions,
  2,169,838 walks.
- **Page traffic.** The badge's cache counters are the boot alone; the
  reference's are the boot *plus* the ~909-page image load, which the host does
  on the badge. The test measures the load phase separately and requires
  `badge + load - reference` to land inside `0..=FRAMES` — the pages left
  resident when the load ended. Currently 17. So a badge page-traffic estimate
  can be built on these numbers rather than on an unexplained 7% gap.
- **`retries` means one thing.** A loopback socket drops nothing, so the test
  requires `retries == 0` and prints the wait distribution when it fails.
  Measured: 0 re-sends in 15,558 requests per boot, worst wait 10–23 ms
  unloaded and 71 ms with eight boots running at once — against a 250 ms
  re-send deadline. That bounds the "host stalled" term to zero on an unloaded
  host, which is what lets a non-zero `retries` in a *hardware* transcript be
  read as log-mirror interleaving rather than as either-or.

**Typed input.** It injects a `ConIn` frame and requires the guest to print a
line the shell *computed* (`echo BADGE-INPUT-$((6*7))`), terminated. Both halves
are deliberate: a marker that appears in the typed text is satisfied by the
tty's own echo, and an unterminated marker is satisfied by a slice boundary
landing mid-word — either would let a console pipeline that eats bytes pass.

What it does not cover: timing (a loopback socket is ~50 µs, the badge's CDC
link is 2 ms), memory pressure, the park-before-send race (a socket buffers;
`usb-bao1x` does not — `usbhost`'s own tests model that), and frame corruption
from mirrored log text sharing the transmit endpoint.

### Startup order — the clock, then the screen, then everything else

Three hardware runs were lost here, one per run, and all three were the same
shape: **a dependency that is invisible because it lives inside somebody else's
library.** The order is `ticktimer → names → gfx + paint → log → usb → mirror`,
and the full reasoning — with the run that paid for each constraint — is the
comment block at the top of `main()` in `app/src/main.rs`. That block is worth
more than any of the individual fixes, because the next person to reorder the
file will otherwise reintroduce one of them.

**Run 1 — connect before registered.** `Ticktimer::new().expect("no ticktimer")`
inside `UsbTransport::new`. This app is a swap-resident `IniS` and starts before
the ticktimer registers; `probe/` never hit it only because it opens with a
five-second sleep, nominally "let a console attach", accidentally "let every
service register".

**Run 2 — report before a channel exists.** The fix for run 1 waited for five
dependencies *before* building the display, so when one did not resolve the
badge sat on the loader's bao graphic with nothing on screen and nothing on the
wire. There was also a literal `return` on the path where the missing dependency
*was* the screen.

**Run 3 — std time before ticktimer.** Painting first, as prescribed, put the
name-server and graphics clients ahead of the ticktimer — and they are ordinary
`std` code. `std::thread::sleep` and `Instant::now()` connect to
`ticktimer-server` internally and panic if it is absent, so the failure moved
into std's own `xous.rs`, a file nobody here wrote.

**The resolution** is that the ticktimer is uniquely cheap to wait for — a
well-known SID needing no name server, probed with a bare `try_connect` syscall
— so it goes first and nothing after it can hit that class. The screen is still
as early as it can be; it simply cannot be earlier than the clock that `std`
needs in order to get there.

**The standing rules, in order of how much they cost to learn:**

1. **The screen is as early as it can be, and everything after it writes its
   stage line before the call that could hang.** A badge stuck showing `usb...`
   is a badge whose USB server never came up. `XousNames::new()` and
   `OledSink::new()` are both blocking and unbounded, deliberately -- that is
   the pair that reached the screen on real hardware, and a bounded probe in
   front of them is a second code path that never has.
2. **Nothing returns.** Every failure path parks, because a process that exits
   takes its screen with it and becomes indistinguishable from one that never
   started. What each halt says is `startup::Halt`, above the `cfg`, and a test
   asserts that no variant can halt without saying something on some channel.
3. **Probes must predict the real call.** Run 2's probe used
   `xns.request_connection(name)` where the real client uses
   `request_connection_blocking` — `Opcode::Lookup` against `Opcode::BlockingConnect`
   (`api/xous-api-names/src/lib.rs:127` vs `:148`), two different name-server code
   paths that can disagree. `Lookup` also calls `xous::create_server_id()` on every
   miss (`services/xous-names/src/main.rs:489`), so polling it draws from the TRNG
   once per attempt, while `BlockingConnect` parks the request in
   `waiting_connections` and answers it the moment the server registers. Named
   servers are therefore no longer probed at all: the blocking call *is* the wait,
   and the stage line above it is the report.

Well-known SIDs are still polled with `xous::try_connect`, and that is a
different case: it is a bare kernel syscall that touches no other server and has
no side effects, and it is the only way to bound such a wait — `xous::connect`
retries inside the kernel forever (`kernel/src/syscall.rs:990-999`).

One more ordering trap, found by reading the source rather than by burning a
cycle: the log server's `TryHookUsbMirror` handler opens with a **blocking**
`xous::connect(b"xous-name-server")` (`services/xous-log/src/main.rs:254`), so
hooking the mirror before the name server exists wedges the log server inside
its own message loop — and every process that logs blocks behind it. The hook
now happens after both names and USB are up, where its only remaining lookup is
a `TryConnect`.

### `app/src/startup.rs` — waiting for services instead of assuming them

The first hardware run painted the OLED and then died on
`Ticktimer::new().expect("no ticktimer")`. As a swap-resident `IniS` this app
starts early enough to reach a well-known-SID connect before the server behind
it has registered. `probe/` never hit this only because it opens with a
five-second sleep, nominally "let a console attach" — we inherited its code and
not its accident.

With the screen already live, the log server and the ticktimer are waited for
with a bound and reported per dependency (`log ok`, `tt ok 214ms`,
`tt MISSING`). Two things worth knowing:

- **The clock is one of the dependencies.** `std::thread::sleep` on this target
  is a blocking call to the ticktimer server, so it cannot be used to wait *for*
  the ticktimer. `Clock::None` is a variant rather than a special case: before
  the ticktimer the bound is an attempt budget with a `yield_slice` backoff,
  after it a 30 s deadline with a sleeping one.
- **No laptop test can reach any of this**, because there are no Xous services
  on a laptop to be un-started. That is the honest boundary of the dry run. The
  policy — the waiting, the bounds, the halt messages — is still plain Rust with
  unit tests in `startup.rs`; only "is it up yet?" is below the `cfg`.

### `app/src/run.rs` — the run loop, and `app/src/main.rs` — the platform leaf

`run.rs` holds all of Task 8's policy: the slice length, the frame count, the
DTB derivation, the `check_interrupts`-before-`step` ordering, what happens on a
link fault, and what goes on the screen when the run ends. `main.rs` holds only
syscalls. That is the same split `oled.rs` makes and for the same reason — every
hardware-only defect in Task 6 was a policy decision written below a `cfg`.

Two things there are worth knowing before changing either file:

- **`Link` must be taken before `PageCache::new`.** `PageCache` owns its
  backing privately and exposes no accessor, so once the cache has the
  `UsbHost` there is no way back to the console bytes, the fault channel or the
  non-blocking pump. `run::assemble` is the one place that does it, so there is
  one ordering rather than two. (The plan's Task 8 text proposed
  `bus.backing_mut().take_console()`, which cannot compile.)
- **The DTB address is derived, not sent.** `rv64-host serve` computes it during
  its load phase and the wire protocol has no field for it, so `run::dtb_address`
  recovers it from the kernel's own boot header — `image_size` at offset 16,
  the same input `rv64_host::boot_layout` uses — and then *checks* the result by
  requiring the FDT magic at the address it derived. A drift between the two
  copies of that rule is reported on the screen at boot rather than presenting
  as a hang with no output.

**This flash is a three-file set.** `usb-bao1x`, `bao1x-hal` and `xous-log` all
live in `xous.uf2`, so none of the four repairs is an app-only update. Build the
image with the three surviving patches applied and the pinned revision forced,
so the swap nonce still matches the app recipe below:

```sh
cd "$XC"
patch -p1 < "$BADGE/usb-bao1x-serialflush-repair.patch"
patch -p1 < "$BADGE/usb-bao1x-serialrx-repair.patch"   # after the flush one: its context includes it
# bao1x-hal-usb-in-completion.patch is deliberately NOT applied: it is the
# writeback regression. See "The four patches" below and task-8-report §34/§35.
# The serialrx patch carries one stanza that belongs to it -- the ep_in_busy
# reset -- so drop that too, or usb-bao1x will not compile.
patch -p1 < "$BADGE/usb-bao1x-drop-in-completion-reset.patch"
patch -p1 < "$BADGE/xous-log-usb-mirror-nonblocking.patch"
cargo xtask baosec-lite \
  --git-rev      9844906ddc1214438d0d942d2db2922846ae4722 \
  --git-describe v0.10.2-beta1-153-g9844906dd
```

`--git-rev` is not optional here. The swap nonce is derived from
`git rev-parse HEAD` of whatever tree built the image, so a checkout at a
different commit — including a scratch copy with a synthetic commit — produces
an image whose nonce the app's `swap.uf2` does not match. Forcing the pinned rev
keeps the app recipe below correct. (`target/xous-tools` documents the same
lesson about `/tmp`.)

Then flash `loader.uf2` and `xous.uf2` from that build together with the *app's*
`swap.uf2` — not the 24 KB `swap.uf2` the xtask emits, which is the empty swap
image for a build with no apps in it.

### Why the host still paces, and what would retire that

`rv64-host serve --pace-ms 1` is still required, and the obvious firmware fix for
it — re-poll inside the IRQ's receive drain loop — is a **guaranteed no-op on
this bus**. Written down because it has already been proposed once and would
cost an image rebuild to disprove a second time:

- `SerialPort::read` calls `read_packet` → `Endpoint::read` →
  `CorigineWrapper::read`, so `read()` is the call that touches hardware: it
  copies out of the app buffer *and* re-arms the OUT transfer
  (`driver.rs`, `fn read`).
- `CorigineWrapper::poll` is `self.core().event_inner.take()` — a cached event
  set by `handle_event_inner`, no hardware access at all (`driver.rs:2841`). A
  second poll in one interrupt returns `None`, `UsbDevice::poll` returns false,
  and nothing happens.
- The OUT endpoint carries **one** TRB at a time into one 512-byte buffer,
  re-armed only from inside `read()`. One completion event is one packet, and
  `composite_handler`'s event-ring loop already polls once per event.

So the receive side is structurally one packet per interrupt, and the fix that
retires `--pace-ms` is a **deeper OUT ring**: distinct app buffers (today
`get_app_buf_ptr` hands back the same address every call in both directions) and
an `app_ptr` that is a queue rather than one `Option`. That is a real driver
change with its own design, and it should not be attempted blind.

**A deeper `SerialRxRing` is not that ring, and does not retire pacing.** Asked
directly on 2026-09-01 when `SERIAL_RX_SLOTS` went 16 → 64, because the two are
easy to confuse and only one of them is on the hardware's side of the boundary:

- `SerialRxRing` sits **above** `SerialPort::read`. It buffers packets the
  endpoint has *already handed over*, one at a time, and it exists so a packet
  is not overwritten before the main loop collects it.
- `app_ptr[pei]` is a single `Option<AppPtr>` per endpoint
  (`driver.rs`, `fn read` takes it with `.take()`, `pending_ep` reads it), and
  `read()` re-arms exactly one transfer into an address `get_app_buf_ptr`
  recomputes to the same value every call. That `Option` is the depth that
  decides whether the hardware can accept back-to-back packets, and it is one.

Deepening the ring therefore raises how much *burst* the badge survives without
dropping packets — which is what it was raised for — and changes nothing about
how fast the endpoint can take them. `--pace-ms 1` stays until `app_ptr` becomes
a queue.

**`app/` requires `usb-bao1x-serialflush-repair.patch`**, for the same reason
`probe/` does and then one more. `UsbTransport` runs a flush watchdog, because
`serial_wait_binary()` blocks forever with no sender and a flush on a period is
the only thing that bounds a blocked read. Stock, the flush handler's binary
branch does `copy_from_slice` into the client's empty `Vec` and panics whenever
there is anything to deliver.

**`app/` can have the USB panic mirror on the same port -- hook it directly.**
(An earlier revision of this file said the opposite. It was wrong.)
`TryHookUsbMirror` is log server opcode 4; it stores a CID in the *log server's*
own state (`services/xous-log/src/main.rs:250-288`) and never touches
`usb-bao1x`'s `serial_listen_mode`, so the transport's `SerialHookBinary`
re-parks cannot disturb it. What must be avoided is
`serial_console_input_injection()`, which reaches `SerialHookConsole` and sets
`serial_listen_mode = ConsoleListener` -- a mode in which arriving page bytes are
injected as keystrokes and then cleared. Hook the log server directly the way
`probe/`'s `try_hook_panic_mirror` does, and check the answer. The probe proved
both work at once on hardware: `mirror: HOOKED`, binary listeners parked for the
whole session, and the swapper's `INFO:xous_swapper: Free pages after GC: 77`
still arriving over CDC after the round-trip leg.

The one real interaction is that mirrored text shares the CDC *transmit* stream
with protocol frames -- so what it corrupts is requests, not responses -- and a
4109-byte request always takes at least two `serial_send` calls, so a mirrored
line can split one. The host's decoder resyncs and drops it rather than
mis-reading it, and then has no request to answer. The transport retries a
timed-out exchange for exactly this reason, so a split frame costs a re-send
rather than a guest fault; the count is on `Link::retries()`. This is not
something log discipline can fix -- the mirror carries every process's output,
and the noisiest is the swapper, which logs under the memory pressure the
emulator generates continuously. Full reasoning in the module docs of
`app/src/usbhost.rs`.

**The decoder reports what it throws away, on the badge too.** `Mux` there is
built with `capturing_noise_capped(NOISE_SAMPLE)` — 64 bytes of sample and an
*exact* count of everything discarded — and both land in `Fault::describe()`,
which reaches the wire through the log mirror. `rv64_proto`'s own docs say the
badge must not carry noise capture, and that was right at 64 KiB and wrong at 64
bytes: the fourth hardware run received four complete replies
(16,436 bytes = 4 × 4109 exactly), preserved their order, had a healthy reader
on every counter, and decoded no frame — and every remaining explanation is told
apart by what the discarded bytes *are*:

| first bytes of the hex | what it means |
|---|---|
| `c1 b0 02 04 10 …` | the frame arrived intact and was rejected on its CRC |
| a shifted or interleaved copy | something upstream of the decoder is mangling the stream |
| `c1 b0 01 04 00 …` (13 bytes) | the CDC endpoint is looping our own request back |
| nothing discarded at all | the bytes never reached the decoder despite being counted |

Note the last two are distinguishable *because* a well-formed frame of an
unwanted type is **held**, never discarded — a loopback shows no discarded bytes
at all while a CRC rejection shows thousands.

**`app/`'s release profile is `opt-level = 3`, and the `"s"` experiment that
briefly replaced it is over.** For seven hardware runs `"s"` was the last
uncontrolled variable between a mechanism that returned real wire data on this
badge and one that returned its own `.text`, and the mechanism that would have
made it matter is real: `xous_ipc::Buffer::to_original` calls
`rkyv::access_unchecked`, which raw-casts
`bytes.as_ptr().add(len - size_of::<T>())`, and rkyv's own alignment assertion on
that pointer is `#[cfg(debug_assertions)]` -- compiled out in release. A
misaligned root there is UB, and UB whose symptom tracks the optimisation level
is exactly what "correct length, payload from somewhere else" looks like.

**The experiment came back negative** (task-8 report §20): the payload bytes were
byte-identical across the two binaries, at the same offset, with different
`.text` sizes and layouts, so the fault did not track codegen. The defect was in
the badge's USB driver and is fixed. Re-measured 2026-09-01 with the current
tree, `3` is still both faster and smaller:

| | `"s"` | `3` |
|---|---|---|
| dry run, boot to a shell (two runs each) | 5.5 / 5.6 s | **4.7 / 4.8 s** |
| `riscv32imac-unknown-xous-elf` release ELF | 1,129,544 B | **1,061,180 B** |

There is therefore no swap-image or icache argument on the other side of it,
which was the only reason `"s"` would have been right for a payload this size.
`app/Cargo.toml` carries the numbers and the autopsy at the profile itself.

**The run loop reports its own throughput, one line at a time.** The badge is
compute-bound rather than I/O-bound -- a boot is ~2,952 page operations against
173.5 M guest instructions -- so the number that matters is instructions per
second of *interpreting*, with the link taken out. `run::run` measures three
disjoint spans and logs

```
rv64 rate: insn=173500000 wall=4886ms link=361ms svc=1ms cpu=4524ms ips=38351016
```

through `log::info!`, which on the badge rides the log server's USB mirror to
the transcript's **stderr** half (`serve` splits the two streams; see above). The
figures above are the laptop dry run, for calibration.

* `link` is accumulated inside `usbhost::LinkInner::exchange` -- a page fault
  happens inside `Cpu::step` and the run loop cannot see one from outside.
* `svc` is the between-slice block: the console pump, the `ConOut` mirror, the
  heartbeat and the repaint. On the badge a heartbeat tick is a whole-screen
  `draw_textview` over IPC, so this is not negligible there and it is neither
  interpreting nor waiting for the host.
* `cpu = wall - link - svc`, and `ips = insn / cpu`.

The first report is at slice 16 (1.6 M instructions, seconds even on the badge)
and then every 256 slices (25.6 M instructions), so a run that dies before
reaching a shell still produces the number -- and so that mirrored text, which
shares the transmit endpoint with page requests, splits a request about seven
times per boot rather than continuously.

**One thing that line cannot separate on the badge:** a page-cache *hit* on a
frame the Xous swapper has paged out costs a swap fault (~4 ms, measured in
`probe-transcript.txt`), and that time lands in `cpu`. At `FRAMES = 1024` the
cache is 4 MiB against ~308 KiB of free SRAM, so most frames are swap-backed and
only locality keeps this from dominating. If a hardware `ips` comes back far
below the laptop-scaled expectation, re-run at `FRAMES = 64` -- an app-only
rebuild -- before concluding anything about the interpreter.

**`wait_binary` reports the actual pointers, once, on the first delivery.**
`capture_addresses` records the lent page's base and length, the offset the
*server* returned, `size_of`/`align_of` of the archived root, the root address
`access_unchecked` computes and **its alignment** (the check rkyv itself makes
only under `debug_assertions`), the `ArchivedVec`'s relative offset and the
address it resolves to, the length beside it, and one known address each from
this process's text, heap and stack so the resolved pointer can be placed
without a memory map. It goes on the wire at construction and again in every
fault.

It captures the **first non-empty** delivery. The first attempt caught
`used=8, veclen=0` — a correct, empty archive, which is routine with the flush
watchdog running and says nothing about the data path. That negative is still
worth having: `used=8` is exactly what rkyv produces for a zero-length payload
(verified against rkyv 0.8.18 on the host), so the boundary is provably correct
for the empty case.

The report prints the **expected** `used` beside the observed one, computed as
`round_up(veclen, 4) + 8` — rkyv lays the payload down first and the root last.
That formula returns 8, 12, 24, 280, 392 and 3848 for payloads of 0, 1, 13, 269,
384 and 3840, checked against rkyv itself rather than derived by eye. So the
badge compares them for us, and a mismatch is the bug named on the device.

It also dumps the first 16 bytes **at `base`** and the 8 bytes at the root
position, which separates the last fork: rkyv writes the payload first, so on a
correct archive the frame starts at `base`. `c1 b0 02 04 10 …` there means the
server wrote our frame and only the root offset is wrong; nops there mean the
page itself holds the wrong data and the fault is upstream of the archive.

It exists because the payload bytes turned out to be **byte-identical across two
binaries with different `.text` sizes and layouts** — which rules out reading our
own code, and with it every content-based theory. `ArchivedVec` is a relative
pointer plus a length; the length is always exactly right, so the pointer is
what is wrong, and an address names the region outright.

The archived size is printed rather than assumed for a specific reason: rkyv's
`pointer_width_*` features are additive and cargo unifies them per dependency
graph, and this app and the flashed `xous.uf2` are resolved in *different*
workspaces. If they disagreed about whether `ArchivedUsize` is 4 or 8 bytes,
`size_of::<ArchivedVec<u8>>()` would differ and `access_unchecked` would look
for the root in the wrong place. `arch=size8` means both ends are 32-bit.

### `usb-bao1x-serialrx-repair.patch` — the receive bug, and the fix

**Confirmed on hardware.** The address instrumentation showed every archive
number matching its host-computed prediction exactly (`used=3848`,
`root=base+3840`, `reloff=-3840`, `resolved=base`, `veclen=3840`) while the page
at `base` held RISC-V `nop` padding — so rkyv, `xous-ipc`, the kernel's offset
return and our whole client stack were correct, and the *page* was already wrong.
The byte windows then showed the payload repeating with a **1024-byte period**:
`base` and `base+1024` byte-identical, `+512` different. Wire data does not
repeat like that. Two 512-byte blocks cycling through a 3840-byte payload is
stale re-reads of a two-packet buffer.

**The defect.** Every CDC packet is read into one shared 512-byte buffer, and
the IRQ posts a scalar carrying *only the byte count*:

```
hw.rs:333    if let Ok(count) = serial.read(&mut usb.serial_rx) {
hw.rs:335        try_send_message(conn, new_scalar(IrqSerialRx, count, ..)).ok();
main.rs:595  serial_buf.extend_from_slice(&cu.serial_rx[..valid_bytes]);
```

`claim_interrupt` hands the IRQ the same `Box<Bao1xUsb>` the main loop holds
(`hw.rs:137-140`), so those are the same bytes. No queue, **no copy at IRQ
time**: a packet arriving before the main loop drains the previous notification
overwrites data already accounted for, and the older count is then satisfied
from the newer bytes. `try_send_message(..).ok()` makes it worse — a full server
queue discards the notification *after* the overwrite.

The 1024-byte period is the `usbd-serial` store: `hw.rs:77` builds the port with
`rx_buf = [0u8; SERIAL_MAX_PACKET_SIZE * 2]`, and `SerialPort::read` pulls one
packet into that **two-packet (1024-byte) software ring** and then drains up to
`data.len()` out of it. Not hardware double-buffering; a two-deep ring, which is
exactly the period observed.

**The residual, and the cheap test for it before another image rebuild.**
With the copy in place the *first* 512 bytes of every delivery are correct and
the tail is still stale -- byte-identical across runs, 1,060,122 bytes and
`not_sync=258` both times. Draining `SerialPort::read` in a loop changed nothing,
and reading `usb-device` says why it could not have: `UsbDevice::poll` opens with
`let pr = self.bus.poll();`, and **`poll()` is what moves data out of the
hardware endpoint**. `SerialPort::read` only drains the class's `rx_buf`, which a
previous `poll()` filled. So looping over `read()` alone can only yield what one
poll produced -- at most the 1024-byte two-packet buffer -- never nine packets.

Before rebuilding firmware again, `rv64-host serve --pace-ms N` tests the
mechanism from the host: write each reply as 512-byte chunks N ms apart so at
most one packet is in flight. If the reply then arrives intact the badge cannot
keep up with back-to-back packets, and the firmware fix has a specification. If
it still arrives stale the overrun theory is wrong and the rebuild would have
been wasted. Off by default -- at 1 ms a page costs ~9 ms against a 2 ms round
trip.

```sh
./serve-wait.sh boot-transcript.txt --pace-ms 1     # then tighten toward 0
```

**Two streams, and keep them apart.** `serve` writes guest console output
(`ConOut` frames) to stdout or to `--console <path>`, and **every byte that was
not a frame** to stderr. `serve-wait.sh` now uses `--console`, because merging
them with `2>&1` makes "did the guest print this, or did it arrive unframed?"
unanswerable — and that question has now decided two investigations. If a
transcript shows something surprising, the first thing to establish is which
file it is in.

**And a caution that follows from the fill-byte lesson:** the probe's round-trip
leg received 4096-byte replies and reported success, but `echo-host.py` filled
pages with one repeated byte, so duplicated packets were undetectable. "The
probe received large replies fine" is *not* evidence that back-to-back packets
ever worked on this hardware. They may never have.

### The same defect in the other direction — the badge's transmit side

The receive fix got the guest kernel booting, and the boot then stopped at
1,313,691 instructions with `misses=258 evictions=1 writebacks=0`, a memory
fault at `0x81fc800c`, and **device-tree content in the transcript** — unframed,
badge→host, and absent from the guest console. The badge has no device tree of
its own; the only device-tree bytes it has ever held are ones it received.

Those bytes are not a leaked buffer. They are **the badge's first `WriteReq`**.
The dry run measures it directly (`Diag::writes`, printed in the link
diagnostics): the first writeback a boot performs is

```
writebacks: 3921 WriteReq frames; first ten (requests before it, page):
  [(258, 1275), (261, 1200), (263, 1201), ...]
```

— page **1275**, which is the DTB's own page (`0x804fb000`), sent after exactly
**258** requests. That matches the hardware counters to the digit, and
`PageCache::resident` says why: it reads the incoming page *first* and evicts
*after*, so a failed writeback leaves `writebacks` **and** `evictions` where
they were and surfaces as a fault at the address of the load that missed. The
badge was never failing to read page 8136. It was failing to write page 1275
back, inside the same call.

**Why that write fails.** `CorigineWrapper::write` (`libs/bao1x-hal/src/usb/
driver.rs:2567`) copies every IN packet into a *single* 512-byte hardware
buffer — `get_app_buf_ptr` computes `new_index = enq + mps` and resets `enq` to
0 whenever `new_index + mps > CRG_UDC_APP_BUF_LEN`, and with
`mps == CRG_UDC_APP_BUF_LEN == 512` that is **every call** — and then enqueues a
transfer pointing at it with no check that the previous transfer has completed.
`usbd-serial`'s `flush` emits one packet per `SerialPort::write`, and
`SerialSendDataBlocking` calls `SerialPort::write` once per 512-byte chunk in a
tight loop. A 4109-byte frame handed over in one `serial_send` is nine packets
queued back-to-back into one buffer, each overwriting the last. It is the exact
mirror of the receive defect, and it is why `--pace-ms` was needed on the host.

Everything this link had ever successfully sent fits in **one** packet: a
13-byte `ReadReq`, and console lines short enough not to split. The first
transmit that does not is the first writeback.

**The test was app-only, and it worked**: `usbhost::send_paced` is the
transmit-side twin of `serve::Pace` — hand the USB server one `TX_PACKET` (512)
at a time with a gap between calls, so at most one packet is in flight. The
first boot with it set `writebacks` non-zero and got past the fault the badge had
died on for five rounds. Read the result off `writebacks` in the report line:
that is still how this mechanism reports itself.

**A firmware fix was tried, and the millisecond came back.**
`bao1x-hal-usb-in-completion.patch` makes `write` return `WouldBlock` while a
transfer is in flight, which is the answer `usbd-serial` was always written for —
`flush` keeps the bytes and `endpoint_in_complete` sends the next packet when the
transfer event lands. On that basis `TX_PACE_MS` was set to 0, with the standing
instruction that a run showing `writebacks=0` again meant putting it back to 1.
**A run did, and it is back to 1.** The transcript named the clobbered buffer
directly — `4 full 512-byte blocks, REPEATED: 2=3`, one packet on the wire three
times — so the completion check is not carrying the pipeline on its own. The
millisecond is what makes writebacks work, it is app-only, and it costs ~8 ms per
writeback and nothing at all on any other frame, because the gap runs *between*
calls and every other frame this link sends is one call.

**And the patch does not stay.** The run after the millisecond came back still
had zero writebacks, this time with the frame reaching the host *neither
accepted nor discarded* — the signature of a truncated frame the decoder holds
in silence. The bisect is one variable: pace 1 **without** the patch reached a
shell and wrote the host image repeatedly; pace 1 **with** it does not.
`bao1x-hal-usb-in-completion.patch` is the regression, its refusal is laundered
into `Ok(512)` by `SerialPort::write` so nothing above can see it, and the
recommendation is to build without it. See the boxed note under "The four
patches", and task-8-report §34.

Going back to 0 needs a hardware run whose `writebacks` climb at 0 — not an
argument from the driver source, which has now been wrong about this twice.

`serve` now also names what it threw away: a discarded run of bytes that is
frame-shaped gets one line in front of the verbatim dump giving its type, its
page and — for a page-sized payload — whether any two 512-byte blocks are
byte-identical. That last check is the one that identified the receive defect,
pointed at the transmit direction:

```
[rv64-host: discarded 4109 bytes; frame-shaped at offset 0: WriteReq,
 declared len 4100, page 1275; 8 full 512-byte blocks, REPEATED: 4=5]
```

**The second half, found after the first fix ran.** With the copy in place the
*first* 512 bytes of every delivery were correct and the tail was still stale.
`SerialPort::read` moves **at most one packet** out of the hardware per call
(usbd-serial `serial_port.rs:120-147`), and one interrupt can cover several
arrivals -- a 4109-byte reply is nine packets and they do not get nine separate
interrupts. Taking one packet and returning left the rest in the endpoint until
something happened to poke it, so a delivery carried its first packet and then
whatever the listener was served next. The IRQ now **drains** the endpoint,
bounded by the ring's capacity so an interrupt cannot run unboundedly.

**The fix.** `usb-bao1x-serialrx-repair.patch` copies the packet at IRQ time
into `SerialRxRing`, so the bytes and their length travel together and nothing
can overwrite a packet that has been accounted for. Sixteen slots (8 KiB): the
largest single response is a 4109-byte page frame = 9 packets, the client's
exchange is strictly synchronous so at most one is in flight, and a power of two
makes the index arithmetic a mask in an interrupt handler. It is deliberately
not deeper — the real bound upstream is the USB server's 128-slot message queue,
and a ring far deeper than the queue feeding it converts a fast failure into a
slow one. Overflows are counted, never silent: the IRQ bumps
`SerialRxRing::overflows` and the main loop logs the change. A notification lost
to a full queue now costs latency rather than data, because the next one drains
both and the flush watchdog bounds the case where there is no next packet.

**Why `probe/` never caught this** — the part worth remembering.
`echo-host.py` answered with `b"\xa5" * PAGE`, and against a page of identical
bytes **a duplicated packet is byte-for-byte indistinguishable from a correct
one**. The probe's only content check was `d[0] != FILL || d[last] != FILL`,
which such a page passes no matter how scrambled the middle is. So "the probe
receives real data over this exact mechanism" was never the reassurance it
looked like, and it pointed five rounds of debugging away from the driver.

Both are fixed: `echo-host.py`'s page is now position-dependent (every 512-byte
block distinct), and the probe checks *every* byte against
`fill_byte(stream_offset + k)`. A check that cannot fail is not cheaper than one
that can, it is worthless. **Any future host-side test data must be
position-dependent for the same reason.**

### The `serial_rx` staging buffer — how it was found

`usb-bao1x` reads every CDC packet into **one shared 512-byte buffer** and tells
the main loop only how many bytes arrived:

```
hw.rs:333   if let Ok(count) = serial.read(&mut usb.serial_rx) {
hw.rs:335       try_send_message(conn, new_scalar(IrqSerialRx, count, ...)).ok();
main.rs:595 serial_buf.extend_from_slice(&cu.serial_rx[..valid_bytes]);
```

`claim_interrupt` hands the IRQ the same `Box<Bao1xUsb>` the main loop holds
(`hw.rs:137-140`), so `usb.serial_rx` and `cu.serial_rx` are the same 512 bytes.
There is no queue and **no copy at IRQ time**. If a second packet lands before
the main loop drains the first message, the older count is satisfied from the
newer bytes — and `try_send_message(...).ok()` drops the message outright if the
server queue is full, after the buffer has already been overwritten.

A 4109-byte reply is nine such packets back to back, which is the first workload
on this badge to send *and* receive large payloads in one exchange.

**Why `probe/` never saw it:** `echo-host.py` fills every page with a single
byte (`FILL = 0xa5`), so a duplicated packet is byte-for-byte indistinguishable
from a correct one, and the probe's only content check was `d[0] != FILL ||
d[last] != FILL`. It was blind to this by construction — which is exactly why
"the probe received real data through this same syscall" was never the
reassurance it looked like.

The address report now dumps 32 bytes at `base` and 16 bytes each at
`base + 512` and `base + 1024`, and prints `pkt_repeat=true|false`: if the
512-byte windows are identical, the driver re-read one staging buffer for
several packets' worth of counts, and the defect above is confirmed on the
device. Fixing it means a patch to `usb-bao1x` (copy at IRQ time into a queue or
ring) and therefore a full `xous.uf2` rebuild — the same path
`usb-bao1x-serialflush-repair.patch` already takes.

**A main-thread `wait_binary` runs once at construction, before the reader
thread exists.** `UsbTransport::probe_main_thread` primes the listen mode, sends
one `ReadReq`, and fingerprints the answer — on the main thread, with no reader
competing for the listener. It is bounded by the flush watchdog (spawned first,
which is why the watchdog now precedes the reader in `new`) and by a 1 s
deadline, and it reports either way through `Transport::status`.

It exists because the sixth run proved the delivery is already wrong when
`wait_binary` returns, and the largest remaining difference from `probe/` — which
returned real wire data on this badge through this same syscall — is that the
probe called it on the **main thread** and this crate calls it on a spawned one.
If the main-thread call returns `c1 b0 02 04 10 …` and the reader returns nops,
that is the answer with no reasoning about rkyv internals required.

**The reader fingerprints its first delivery, and the probe is why.**
`../probe/src/main.rs` checked its deliveries' *content* (`d[0]` and
`d[d.len() - 1]` against its fill byte, counted in `RX_BAD`) — which is the only
reason it could claim its receive numbers were real and not just byte counts.
This crate took the probe's receive code and dropped that check, and five
hardware runs could then not tell "the bytes were wrong on arrival" from "the
bytes were corrupted after arrival". `Rx::first` restores it: the first 16 bytes
of the first non-empty delivery, captured the instant `wait_binary` returns and
reported by `Transport::status`, next to the decoder's discarded sample in the
same fault line. If they agree the fault is above the reader; if they differ it
is below.

**The flush watchdog is on the critical path of every page read, not a
backstop.** This fell out of modelling `usb-bao1x`'s real listener semantics
(`usbhost.rs`'s `BaoLink` test): a page response is 4109 bytes and
`SERIAL_BINARY_BUFLEN` is 3840, so the IRQ delivers the first chunk and there is
**no second IRQ to carry the tail** — only the periodic `SerialFlush` can. That
is the strongest argument yet for why the badge image must carry
`usb-bao1x-serialflush-repair.patch`: without a working flush, every page read
strands its own tail and the link goes quiet after the first request.

The reason the watchdog exists at all is worth knowing before touching
`usbhost.rs`: `Opcode::SerialHookBinary` parks a listener but does **not** drain
`serial_buf`, so against a synchronous peer a reply that lands before the park is
lost rather than delayed. `probe/` measured that as round 3's `120 ms/rt` -- the
watchdog period, not the link. `UsbHost` therefore confirms a parked listener
*before* every request leaves the badge, and primes `BinaryListener` at
construction. See the module docs in `app/src/usbhost.rs`.

### `app/src/oled.rs` — the guest console, on the screen

The other end of the guest from `usbhost.rs`, and the one the project exists to
photograph. Guest UART bytes go in a byte at a time; a character grid comes out
and is blitted with one `draw_textview`.

**The grid is 16 columns by 8 rows, not 18 by 8.** The plan and an earlier
revision of `../docs/xous-api-notes.md` §4c both said 18, from
`128/7`. Seven is the width of a mono glyph's *ink*; the *advance* is
`wide + kern`, `DEFAULT_KERN` is 1, and `128/8` is 16. That section now carries
the full correction, the file-and-line citations, and the measurement that
settled it — xous-core's real typesetter and blitter, run on the host into a
128x128 bit buffer, where 18 characters demonstrably wrap and eight rows of 16
land on eight clean 15-pixel bands.

Task 1 had shaped the guest's output for 18; `nix/guest/init.sh` has been
corrected to 16 and now nothing it prints spills. Its model line is
`cut -c1-16`, which fills a row exactly, the 32-character store hash lands on
precisely two full rows rather than straddling three, and the package-name line
is `cut -c1-16` as well. `crates/rv64-host/tests/boot.rs` asserts the name line
is no wider than the display, so a regression in those widths fails there
rather than on a photograph.

Everything that decides what the screen says — wrapping, scrolling, `\n`,
`\r`, backspace, tab expansion, CSI stripping, the deferred wrap at the right
margin, the heartbeat — is plain Rust with unit tests. Only `GfxScreen`, which
builds a `TextView` and calls `draw_textview`/`flush`, is under
`#[cfg(target_os = "xous")]`, and it contains no branches. `tests/oled_boot.rs`
boots the real nixpkgs guest and asserts the grid of characters the display
would show, store path included; it skips when the images are absent, like the
workspace's other integration suites.

Two things to know before touching the drawing:

- The `TextBounds::BoundingBox` is deliberately **wider than the screen**, and
  `clip_rect` is what bounds the drawing. A screen-sized box makes the
  typesetter re-wrap rows the grid has already wrapped, because a full row is
  exactly 128px and its fit predicates are strict. Overhang is clipped per
  glyph and per pixel, so the failure mode of a grid one column too wide is a
  character falling off the right edge rather than the layout cascading.
- Every row is padded to the full width and the frame has no trailing newline.
  An empty first line would not advance `y` at all — `move_candidate_to_newline`
  adds `cursor.line_height`, which starts at zero — and the whole screen would
  shift up a row.

**Diagnosability.** The badge has a screen and a log mirror, and they answer
different questions. Panics reach the wire (see above — hook `TryHookUsbMirror`
directly), and this module reports its own failures through `log::` so they ride
the same channel. The screen answers what the mirror cannot:

| what a photograph shows | what it means |
|---|---|
| dark, and `PANIC in PID n:` on the wire | it panicked before drawing |
| dark, wire silent | the draw path never ran. The constructor paints the banner before it returns, so a live app cannot show this |
| the banner and its ruler, unchanging | the display works end to end; the *guest* produced nothing. Look at the transport |
| the ruler wrapped, or short of the right edge | the column count disagrees with the font. `f` must sit flush right with nothing spilled below |
| three solid bars | `draw_textview` failed three times running but `draw_rectangle` still works |
| the corner spinner stopped | the run stopped. It ticks only into a *blank* bottom-right cell, so it can never eat a character of a store path |

`GfxScreen::new` also asks the font its own metrics and `log::error!`s if they
disagree with the constants. That is the one assumption no laptop test can
reach, and it is the first thing to check if the screen ever looks wrong.

`Cargo.lock` is committed and seeded from `probe/Cargo.lock`, because a fresh
resolve fails: `heapless 0.7.17` requires `spin ^0.9.2` and `spin 0.9.8` is
yanked. A lock file is allowed to name a yanked version; a fresh resolve is not.

## `probe/` — the memory and throughput probe

Five numbers, because only hardware can answer them:

1. USB-serial throughput for 4 KiB transfers, transmit-only;
2. **sustained receive**, swept — bursts of 4 KiB pages streamed back to back at
   doubling sizes (4 KiB … 512 KiB), each drained before the next is asked for.
   This leg reports *two* numbers and the second is the more important: a rate,
   and **the largest burst the badge can absorb** before the USB server's message
   queue overruns. The emulator's transfer design turns on both, because it will
   read ahead and pipeline rather than stop and wait for each page — and the
   ceiling is how far ahead it is allowed to read. See *The receive ceiling*
   below; it is not a soft limit, it kills the receiving process;
3. **per-request round-trip latency** — the probe emits a request and a host
   script echoes one whole page back — printed beside the noise floor of the
   instrument that measured it;
4. free physical pages;
5. how far a demand-paged `map_memory` region can be touched, and how far a
   *heap* allocation can climb.

All three throughput legs run *before* the memory climbs: they are the numbers the
transfer-path design hangs on, and the climbs are the part that takes the system
down. Within throughput, transmit runs first because it needs no host cooperation,
so a run with nothing attached still yields that number; the stream runs before the
latency rounds because it is the figure the design turns on and the latency leg is
the one that can time out. Within the climbs, the mapping climb runs first
**because it can be given back** and the heap climb runs last **because it cannot**
— see *What the numbers mean*, below.

A run with no host on the other end must say so rather than produce a plausible
figure. Two counters enforce that: the transmit leg reports the bytes `serial_send`
actually accepted, and both receive legs are entered only when that count is
non-zero (and report `NO DATA` or `TIMEOUT` rather than a rate).

### Reading the transcript

Three failure modes — panicked, wedged, never started — otherwise look identical
from the host: output stops. The probe is built so they can be told apart, and
this is worth more than any individual measurement, because it is the difference
between one flash and three.

| what you see | what happened |
|---|---|
| nothing at all, not even `=== probe start` | **never started**: the image did not boot, the loader refused it, or the probe is not in the image |
| `PANIC in PID n:` then a message | **panicked**, in that PID. PID 2 is the swapper, so a swap-exhaustion `panic!` shows up here too |
| `..hb` still ticking, no new `##` line | **wedged**: a blocking call never returned |
| output stops mid-stream, `..hb` stops too | the process is gone with no panic text — a kernel panic (kernel `println!` goes only to the physical debug UART, which is not brought out to CDC) |
| transcript ends on a `## <stage>` line | that stage killed the run |
| `mirror: NOT HOOKED` near the top | the panic path is dead for this run: panics will print nothing. The run still produces numbers, but panic and wedge can then only be told apart by the heartbeat |

The panic path works because the probe hooks the log server's USB mirror
(`TryHookUsbMirror`, `services/xous-log/src/main.rs:250`); the std panic path —
log-server scalar opcodes 1000 and 1101..=1132 — mirrors to that same CDC port.
The probe must **never** call `serial_clear_input_hooks()`, whose handler sends
`UnhookUsbMirror`. An earlier revision of this probe did, which is why every
failure used to present as silence.

**The hook is checked, and the transcript says so in a `mirror:` line.** It is
asked for directly rather than through `serial_console_input_injection()`, which
cannot report: that is a *non-blocking* scalar (`services/usb-bao1x/src/lib.rs:256`)
and the usb-bao1x handler discards the log server's answer into `log::error!`
(`services/usb-bao1x/src/main.rs:686-710`), which goes to the physical debug UART
this badge does not bring out. Asked directly, `TryHookUsbMirror` is a *blocking*
scalar returning 1 for established and 0 for "could not connect to the USB
driver", so the probe prints `mirror: HOOKED` or `mirror: NOT HOOKED` (after three
attempts 200 ms apart) and the host knows which. The banner is printed *before*
the hook, so a transcript with a banner and nothing else points at the hook
itself.

Both climbs print a `##`-marked stage line before they start and a progress line
after every step, because neither can report its own boundary as an error.
`map_memory` reserves PTEs only; the physical allocation happens on the touching
store, and there OOM arrives as a kernel `.expect("Couldn't allocate new page")`
or a swapper `panic!("Ran out of swap space, hard OOM!")` — never an `Err` the
probe could print. The last step line printed *is* the boundary.

The probe is an init process that ends in `terminate_process`, so **it re-runs on
every boot**. A missed or garbled capture costs a power cycle, not a flash. Every
risk here except a bad first flash is retryable at that price.

### The receive ceiling — why streaming has a hard limit

Fix round 4 asked the host for 1 MiB in one uninterrupted push and the probe died
mid-stream, with the badge naming `usb-bao1x/src/lib.rs:243:14: Internal error`.
The USB server was **not** the casualty — its CDC node was still enumerated
afterwards. What overruns is the kernel's per-server message queue, and the
failure mode is worth knowing before designing any transfer path on this badge:

- The USB interrupt handler posts one `IrqSerialRx` scalar per CDC packet
  (`services/usb-bao1x/src/hw.rs:333-337`) of at most 512 B (`hw.rs:29`).
- A Xous server's message queue is exactly one page
  (`kernel/src/services.rs:1841-1844`) of 32-byte `QueuedMessage` — **128 slots**.
- A blocking memory send lends the client's page into the server *first*
  (`kernel/src/syscall.rs:117-131`) and only then tries to queue
  (`:288`). On `ServerQueueFull` the kernel retries the whole instruction
  (`:1013-1014`) **without undoing the lend**, so the retry meets a page that is
  already `SHARED` and not `VALID` and can only fail with `BadAddress`
  (`kernel/src/arch/riscv/mem.rs:1047-1049`).
- `serial_wait_binary()` discards that error and `.expect()`s a substitute, which
  is why the panic named `Internal error` rather than the fault.

So the ceiling is a queue-depth property, not a memory one: 128 slots x 512 B is
64 KiB of receive in flight if the server drained nothing concurrently, and it
does drain, so the real figure is higher — which is exactly what the sweep is for.
The probe calls `wait_binary()` rather than `serial_wait_binary()` specifically to
turn this from a fatal panic into a recorded reading; a run can now cross the
ceiling and keep going.

**This is a kernel defect and there is nothing `usb-bao1x` could do about it.**
Fixing it properly means undoing the lend before returning `ServerQueueFull`, in
`kernel/src/syscall.rs` — which lives in `xous.uf2` and costs a full three-file
flash. Nothing in this directory does that. The app-side answer is to stay under
the ceiling, which is why the number is measured rather than assumed.

### What the numbers mean, and do not

Most of them are reported with their limits stated in the output line itself,
because a number that lies quietly is worse than no number. Every memory line also
names **what it was measured against** — the free-page count standing when that
climb started — so a swap-limited figure cannot be read as a RAM-limited one:

- **The two receive numbers measure different things and neither substitutes for
  the other.** `rx-burst:` is sustained receive throughput: the clock runs from
  the first arriving byte to the last, the first delivery's bytes are excluded
  from the numerator, and the request's turnaround is therefore outside the window
  by construction. That is the shape the emulator will actually use, because it
  reads ahead. `rt:` is one request at a time, and its figure necessarily contains
  one turnaround, the host's own service time, and the probe's IPC costs. The wall
  figure that *does* include the turnaround is printed on the same `rx-burst:`
  line beside the steady-state one, so the two can be compared directly rather
  than confused. At the small end of the sweep the steady window falls inside one
  tick of the 1 ms clock, and the line says so: those sizes are there for the
  error column, not the rate column.
- **`rx-sweep:` is the headline, not the KiB/s.** It names the largest burst that
  completed with zero listener-lend errors. That number is the emulator's receive
  window, and a design that pushes more than it without waiting for the badge to
  drain does not merely go slower — it takes the receiving process down.
- **The latency figure is printed with its own noise floor, on the `rt-floor:`
  line**, because a latency number that cannot resolve below its instrument is
  worse than one that admits it. Four quantities are on that line: the 1 ms
  ticktimer resolution; the measured cost of one blocking IPC (timed over 1000
  `elapsed_ms()` calls, itself a blocking scalar to the ticktimer server), of
  which a round trip contains at least four; the residual watchdog period and how
  many rounds landed at or above it; and how many rounds went out without the
  reader confirming it was parked. The host's own turnaround is inside the figure
  and cannot be separated on the badge — `echo-host.py` measures and prints it
  from the other end, on a `host:` line in the same transcript.
- **`free_pages`** is read twice — once as a `BASELINE` before anything else
  allocates, and once `POST-RECLAIM` between the two climbs — and both are
  labelled `BIASED` in the transcript. There is no app-callable pure query: the
  kernel's `GetFreePages` is `SWAPPER_PID`-gated (`kernel/src/syscall.rs:1083`).
  The only reachable path is `xous_swapper::garbage_collect_pages(n)`, which is
  **not a query** — the swapper sets `pages_to_free = n.max(HARD_OOM_PAGE_TARGET *
  2)` (`services/xous-swapper/src/main.rs:779`, `HARD_OOM_PAGE_TARGET = 24`), so
  asking for 0 forces 48 pages (192 KiB) out to SPI RAM with interrupts masked and
  only *then* reads the count. On a 2 MiB machine (`HW_SRAM_MEM_LEN = 2097152`)
  that is a large fraction, and the eviction filter exempts only PID 1 and PID 2 so
  it can steal the probe's own pages. The
  two readings are not a "trend" — which is the one thing this call cannot support
  — they are the *same call with the same bias* either side of the reclaim, which
  is exactly what makes the pair meaningful. The probe sends the opcode itself
  rather than calling `xous_swapper::Swapper::garbage_collect_pages()`, which
  **cannot return this number to anyone** — see *A third upstream bug*, below — and
  it distinguishes a failed call from a zero count: an IPC failure prints
  `IPC-ERROR`, so a printed `0` now means zero free pages and nothing else.
- **The `map_memory` climb runs first**, and is measured against the `BASELINE`
  count: the machine as the boot set left it, with nothing allocated by the probe.
  `MapMemory` allocates from `MemoryType::Default` (`kernel/src/syscall.rs:771`),
  a separate 256 MiB virtual window — the heap ceiling does not constrain it at
  all. `MemoryFlags::RESERVE` is **ignored on RISC-V** (it is read only in the ARM
  backend; the RISC-V `map_range` takes the lazy path unconditionally when
  `phys == 0`, `kernel/src/mem.rs:651`), so every `map_memory(None, None, ..)` is
  demand-paged whether or not the flag is passed, and there is no way to ask this
  API for eager backing.
- **The reclaim between the climbs is the reason both numbers now happen**, and it
  is measured rather than assumed. Each 256 KiB step is *read back first* and
  *unmapped second*, in that order and only that order: `unmap_page`
  (`kernel/src/mem.rs:774`) releases a physical page only if `virt_to_phys`
  resolves, and a swapped-out page does not (`kernel/src/arch/riscv/mem.rs:647`) —
  the PTE is zeroed, nothing tells the swapper, and the swap slot stays marked
  used. `FLG_SWAP_USED` is cleared in exactly one place, the swapper's read-back
  path (`services/xous-swapper/src/main.rs:517`). So unmapping a swapped-out region
  without reading it back leaks the swap under it. The transcript reports how many
  KiB came back and a checksum of the marker bytes, which is also the only
  integrity check on the encrypted-swap round trip we get for free.
- **The heap climb runs last**, and is measured against the `POST-RECLAIM` count,
  which it prints next to the baseline. It is the one that measures what would
  actually bind an emulator page cache built out of `Vec`/`Box`:
  `AdjustProcessLimit(HeapMaximum)` governs `MemoryType::Heap` only — `IncreaseHeap`
  (`kernel/src/syscall.rs:830`) and the Heap arm of `find_virtual_address`
  (`kernel/src/mem.rs:451`). The default is 512 KiB unless the kernel carries
  `big-heap`, and the probe prints the value it read before raising it. It is last
  because heap memory **cannot be given back**: dropping a `Vec` returns pages to
  this process's allocator, not to the kernel.
- **The transmit figure is an upper bound.** It is one thread hammering the USB
  server with blocking IPC, uninterleaved with any emulation work.

Both climbs stop at the same 6 MiB cap, and **that number is arbitrary** — nothing
in the source derives it, and the code says so where it is defined. What *is*
derived, and cited there, is the ceiling it sits below: `SWAP_RAM_LEN` is 8 MiB
(`libs/bao1x-api/src/offsets/baosec.rs:16`) and `derive_usable_swap`
(`loader/src/swap.rs:78`) subtracts a 16-byte MAC per 4 KiB page, leaving 8160 KiB
of usable swap. That ceiling is system-wide — how much of it the kernel and the ten
services booting ahead of the probe already hold is the thing no constant can know
— so 6 MiB is a guess with roughly 2 MiB of headroom. Because the climbs are
ordered and the first is reclaimed before the second starts, 6 MiB is the *peak*,
not half of it. Anything a climb reports above 2048 KiB is being served by swap,
not SRAM, and the transcript line says which regime it is in.

### Toolchain

The Xous target needs a custom sysroot whose version must match the running
`rustc` **exactly**. `bunnie/dabao-console`'s `build.rs` automates this; the same
thing done by hand, once:

```sh
rustc --version                     # these artifacts were built with 1.97.1
curl -L -o xous-tc.zip \
  https://github.com/betrusted-io/rust/releases/download/1.97.1.1/riscv32imac-unknown-xous_1.97.1.zip
unzip -o xous-tc.zip -d "$(rustc --print sysroot)"
cat "$(rustc --print sysroot)/lib/rustlib/riscv32imac-unknown-xous-elf/RUST_VERSION"
```

The zip unpacks to `lib/rustlib/riscv32imac-unknown-xous-elf/` and ships its own
`RUST_VERSION` stamp. Releases exist for 1.75 through 1.98. Note the release
**tag** is `<rustc-version>.N` where `N` is a rebuild counter that is *not*
derivable from the rustc version — you have to look it up on the releases page.
For 1.97.1 it is `1.97.1.1`.

> **`rustup update` destroys this sysroot, silently.** There is no
> `rust-toolchain.toml` here and `rustup show` does not list
> `riscv32imac-unknown-xous-elf` at all — rustup does not know the target exists,
> because the files were hand-unzipped into the *`stable` channel's* sysroot. Move
> `stable` and the sysroot moves with it; the next build fails with `can't find
> crate for 'std'`. Check before building:
>
> ```sh
> cat "$(rustc --print sysroot)/lib/rustlib/riscv32imac-unknown-xous-elf/RUST_VERSION"  # must equal rustc --version
> ```
>
> Recovery is the `curl`/`unzip` above with the new version's tag. A
> `rust-toolchain.toml` pin was deliberately *not* added: it would move the
> sysroot path to `1.97.1-aarch64-apple-darwin` (breaking the working install
> until it is re-unzipped there), and it would cover only `probe/` — the kernel,
> loader and services are built in the xous-core checkout, which carries no pin of
> its own. A pin covering half the build is a worse trap than a documented hazard.

### Build and package — the full image

This is the sequence for a **first flash**. Run it top to bottom.

```sh
# 0. Absolute paths throughout: step 3 runs inside $XC, and the probe ELF must be
#    named by a path that is still valid from there.
BADGE=$PWD                       # baochip/badge, i.e. the directory this file is in

# 1. The xous-core checkout. $XC must be at exactly this revision: it supplies the
#    kernel, loader, all eleven services and the signing keys, and the swap image's
#    encryption nonce is derived from its HEAD commit.
XC=/path/to/xous-core
git clone https://github.com/betrusted-io/xous-core "$XC"
git -C "$XC" checkout 9844906ddc1214438d0d942d2db2922846ae4722

# 2. The repairs. The serialflush one is required -- see below; without it the
#    probe panics the USB server, which is its only output channel. The next two
#    are required for the emulator app, which moves multi-packet frames in both
#    directions; apply serialrx *after* serialflush, since its context includes it.
#    The log one is required for any image that hooks the USB console mirror --
#    without it a full USB message queue deadlocks the log server against the USB
#    server and the whole badge goes silent with no fault of any kind.
patch -d "$XC" -p1 < "$BADGE/usb-bao1x-serialflush-repair.patch"
patch -d "$XC" -p1 < "$BADGE/usb-bao1x-serialrx-repair.patch"
# bao1x-hal-usb-in-completion.patch is deliberately NOT applied: it is the
# writeback regression. See "The four patches" and task-8-report §34/§35.
patch -d "$XC" -p1 < "$BADGE/usb-bao1x-drop-in-completion-reset.patch"
patch -d "$XC" -p1 < "$BADGE/xous-log-usb-mirror-nonblocking.patch"
patch -d "$XC" -p1 < "$BADGE/xous-app-uf2-repair.patch"   # only needed for app-only updates

# 3. Build the probe out of tree.
(cd "$BADGE/probe" && cargo build --release --target riscv32imac-unknown-xous-elf \
   --features board-baosec --features bao1x --features oem-baosec-lite \
   --features utralib/bao1x)
PROBE="$BADGE/probe/target/riscv32imac-unknown-xous-elf/release/probe"

# 4. Build the whole image, with the probe as a path cratespec. `~swap` is
#    explicit for documentation; `baosec-lite` already defaults cratespecs to the
#    swap region (xtask/src/main.rs:1320).
(cd "$XC" && cargo xtask baosec-lite "$PROBE~swap")

# 5. Collect the three artifacts.
mkdir -p "$BADGE/probe/out"
cp "$XC"/target/riscv32imac-unknown-xous-elf/release/loader.uf2 \
   "$XC"/target/riscv32imac-unknown-xous-elf/release/xous.uf2 \
   "$XC"/target/riscv32imac-unknown-xous-elf/release/swap.uf2 \
   "$BADGE/probe/out/"
```

`cargo xtask baosec-lite` signs the loader and kernel with `devkey/dev.key` and
`devkey/dev-pq.key` (`xtask/src/builder.rs:202,206,231`) — that is exactly the
dev-key-signed loader that trips developer mode. It needs no extra flags; run from
inside `$XC` it derives `--git-rev` and the semver from that checkout's git state
itself, which is why `$XC` being at the right revision matters.

The probe reaches the image as a `CrateSpec::BinaryFile` — a prebuilt ELF the
builder does not compile, only packages (`xtask/src/builder.rs:1608`,
`split_region` at `:788`). It lands in the swap image as an `IniS` init process
and starts on every boot.

### Build and package — an app-only update

Only valid **after** developer mode has been tripped by a full flash. Rebuilds
`swap.uf2` alone from the probe ELF; the loader and kernel on the badge are left
as they are.

```sh
# NOT /tmp. macOS sweeps /tmp on an idle timer, and it took both the tool and
# most of a xous-core checkout with it between two hardware runs -- `tools/src`
# survived, `tools/Cargo.toml` did not, which is a confusing way to lose an
# afternoon. `baochip/target/` is gitignored and durable.
cargo install --locked --path "$XC/tools" --bin xous-app-uf2 \
  --root "$(git rev-parse --show-toplevel)/baochip/target/xous-tools"
TOOLS="$(git rev-parse --show-toplevel)/baochip/target/xous-tools"
(cd probe && cargo build --release --target riscv32imac-unknown-xous-elf \
   --features board-baosec --features bao1x --features oem-baosec-lite \
   --features utralib/bao1x)
mkdir -p probe/out
(cd probe/out && "$TOOLS"/bin/xous-app-uf2 --swap \
  --git-rev      "$(git -C "$XC" rev-parse HEAD)" \
  --git-describe "$(git -C "$XC" describe)" \
  --elf ../target/riscv32imac-unknown-xous-elf/release/probe)
```

**The emulator payload, `app/`** — the same shape, with two differences. It
needs no `--features` flags (its dependencies carry their board features inline
in `app/Cargo.toml`, unlike `probe/`, which declares aliases so its command
reads the same), and the binary is `rv64-badge`:

```sh
./check.sh                          # from baochip/, inside `nix develop`
(cd badge/app && cargo build --release --target riscv32imac-unknown-xous-elf)
mkdir -p badge/app/out
(cd badge/app/out && "$TOOLS"/bin/xous-app-uf2 --swap \
  --git-rev      9844906ddc1214438d0d942d2db2922846ae4722 \
  --git-describe v0.10.2-beta1-153-g9844906dd \
  --elf ../target/riscv32imac-unknown-xous-elf/release/rv64-badge)
```

If `$XC` is gone (see the /tmp note above), cargo's own checkout of xous-core at
the pinned revision is a complete tree and works as `$XC`:
`XC=~/.cargo/git/checkouts/xous-core-*/9844906` — copy it somewhere writable
first, since the repair patch has to be applied to it.

`--git-rev` supplies the swap encryption *nonce* and must match the xous-core
revision the badge's loader was built from — it is the reference revision at the
top of this file, so it is written out literally rather than derived from `git`
in a repo that is not xous-core. `--git-describe` is the embedded semver and has
no default that works here either (this repo has no tags; the tool dies with
`SemVer::from_git: no major version`).

The tool prints a section table and `Size on disk`. The loader reports that same
figure back on the next boot — `swap - 768k` against a `770kiB` build is the
matched pair to look for, and a mismatch means the badge booted an older image
than the one just copied.

`badge/app/out/` is gitignored, like `probe/out/`. Copy `swap.uf2` alone: hold
`PROG`, plug in, copy **only** `swap.uf2` to `BAOCHIP`, `sync`, unmount, press
`PROG`.

`--git-rev` supplies the swap encryption nonce and `--git-describe` the embedded
semver; both default to running `git` in the *current directory*, which is wrong
here (this repo is not xous-core and has no tags — the tool dies with
`SemVer::from_git: no major version`). Pass them explicitly.

### The four patches

All four apply against `$XC` at `9844906` with `patch -p1`, all four carry
their rationale as comments in the source they touch, and all four are reported
here upstream-style. **Order matters for the first two**: the serialrx patch's
context includes the serialflush fix, because both touch the `SerialFlush`
handler. The other two touch files nobody else does.

**`xous-log-usb-mirror-nonblocking.patch` — the log server must not block.**
Required for any image that hooks the USB console mirror, which this project's
does. **This is the patch that turns a wedge into a fault**, and the wedge it
removes cost the nineteenth hardware run.

`usb_send_str` (`services/xous-log/src/main.rs:34`) mirrors each log line to
`usb-bao1x` with `Buffer::send`, under a comment saying *"this API doesn't
block."* It does. `Buffer::send` is `xous::send_message(.., Message::Move(..))`,
and the kernel answers `ServerQueueFull` for a `SendMessage` with
`retry_syscall(pid, tid)` (`kernel/src/syscall.rs:1010-1017`) — it parks the
caller and retries. Only `try_send_message` returns the error.

That closes a cycle with a service that logs:

1. `usb-bao1x`'s main loop calls `log::error!`, which is a **blocking** lend to
   the log server (`api/xous-api-log/src/lib.rs:55`).
2. The log server handles the record and mirrors it back to `usb-bao1x`.
3. `usb-bao1x`'s 128-slot queue is full — which is exactly the condition its own
   log line usually reports, because the queue fills when its main loop falls
   behind the receive interrupt. The log server parks in `retry_syscall`.
4. Neither runs again. Every process that logs blocks behind the log server, and
   the emulator, blocked in `SerialSendDataBlocking`, is parked in the kernel
   below every deadline it has. No fault, no frames, no output at all.

Observed 2026-09-01: two `serial rx ring overflowed` lines and then silence for
five minutes. The badge's own `serial rx ring` diagnostic was the trigger; it now
reports from the flush watchdog rather than the per-packet handler, which is the
second half of the repair (`usb-bao1x-serialrx-repair.patch`).

This is the same class as the ordering trap recorded above — hooking the mirror
before the name server exists wedges the log server inside its own loop — reached
from the other side. **A blocking call from the log server into a service that
logs is a deadlock, whatever direction it is written in.**

**`bao1x-hal-usb-in-completion.patch` — a bulk IN endpoint that never says
"not yet".** Written for anything that transmits more than one packet, which on
this project means every 4109-byte page writeback.

> **Do not apply it. It is the regression that stopped writebacks working, and
> the current recommendation is to build without it and keep `TX_PACE_MS = 1`.**
>
> The bisect is one variable. The boot that reached a shell
> (`logs/2026-09-01-SHELL-*`, flashed from `e98cb6f`) had `TX_PACE_MS = 1` and
> **no** patch, and wrote the host's memory image repeatedly. `2c31ede` added
> the patch and set `TX_PACE_MS` to 0; every run since has had **zero**
> successful writebacks, including the one after `49861e1` put the millisecond
> back. Pace 1 works without the patch and fails with it, and the only frame
> kind the patch can affect — multi-packet bulk IN — is the only frame kind
> that fails.
>
> The mechanism is that the refusal is invisible from above.
> `SerialPort::write` returns `write_buf.write(data)` — bytes *buffered* — and
> tolerates its own `flush` failing with `WouldBlock`, so a refused packet still
> comes back as `Ok(512)` through `serial_send`. The refused bytes then wait for
> `endpoint_in_complete` (on a driver whose own comment says interrupts do go
> missing) or the 5 ms flush watchdog, neither of which is on the send's
> critical path. A stranded *tail* carries the frame's CRC, so the host's
> decoder holds an incomplete frame silently: neither accepted nor reported as
> discarded, which is exactly what the twenty-third transcript showed. The
> `IN_REFUSAL_LIMIT` valve is counted inside `write`, so after the last packet
> only the watchdog advances it — ~320 ms per stranded packet, against a 250 ms
> attempt deadline.
>
> If the revert brings back the REPEATED-block corruption this patch was written
> for, the fix is to make the refusal **observable** — have `usb-bao1x` report
> `flush`'s result alongside `total_sent`, so `send_paced` can wait for it —
> rather than to hide it inside `bao1x-hal`. See task-8-report §34.
>
> **Checking that a built image really lacks it**, because a stale build
> directory looks identical from the outside. The patch's safety valve carries
> the only string literal it adds, so grep the *shipped* `xous.uf2` for it —
> after stripping the UF2 block headers, since a 512-byte block is 32 bytes of
> header and 256 of payload and a literal can straddle two:
>
> ```python
> import struct
> raw = open("xous.uf2","rb").read(); out = bytearray()
> for i in range(len(raw)//512):
>     b = raw[i*512:(i+1)*512]; psize = struct.unpack("<I", b[16:20])[0]
>     out += b[32:32+psize]
> print(bytes(out).count(b"enqueueing anyway"))   # 1 == patched, 0 == clean
> ```
>
> Corroborating signs: `libs/bao1x-hal/src/usb/driver.rs` shows as unmodified
> against `9844906dd`; `strings target/riscv32imac-unknown-xous-elf/release/usb-bao1x`
> has no "refusal" in it; and the kernel shrinks by ~4 KiB
> (2 818 576 → 2 814 480 bytes).

`CorigineWrapper::write` (`libs/bao1x-hal/src/usb/driver.rs:2567`) copies each IN
packet into a **single** 512-byte hardware buffer and enqueues a transfer
pointing at it with no check that the previous transfer has completed.
`get_app_buf_ptr` computes `new_index = enq + mps` and resets `enq` to 0 whenever
`new_index + mps > CRG_UDC_APP_BUF_LEN`; with `mps == CRG_UDC_APP_BUF_LEN == 512`
that is **every** call, so the address is the same one every time. Nine packets
handed over back to back are nine writes into one buffer.

The layers above were always written for the answer this patch adds:
`usbd-serial`'s `flush` sends one packet, keeps the rest in its own buffer and
propagates `WouldBlock`, and its `endpoint_in_complete` calls `flush` again when
the transfer event lands (`serial_port.rs:153`, `:221`). Returning `WouldBlock`
while a transfer is in flight therefore turns the whole class into a
completion-driven pipeline at hardware speed. It was expected to retire the
app-side millisecond in `usbhost::TX_PACE_MS`; it did not — see "the millisecond
came back" above — so both are in force.

Completion is observed through `app_ptr`, which `handle_event_inner` sets for
both directions on a transfer event — not through `poll()`, which on this bus is
only `event_inner.take()` and depends on the class stack calling it. `write` was
already taking that slot and discarding it.

**It has a safety valve, deliberately.** After `IN_REFUSAL_LIMIT` (64)
consecutive refusals with no completion the endpoint enqueues anyway and says so
on the log — **except that on this build it says nothing at all.** It reports
through `crate::println!`, which is `bao1x_hal::debug::Uart`, whose `write_str`
is `#[cfg(not(all(feature = "std", not(feature = "debug-print"))))]` and whose
`putc` is a no-op without `debug-print`. `services/usb-bao1x/Cargo.toml` takes
`bao1x-hal` with `std` and has `debug-print-usb` and `verbose-debug` commented
out, and `--features debug-print` is passed only to the `loader` package. So the
valve works and is silent, and an earlier note saying to watch for
`ep<n> IN: no completion after 64 refusals` in a transcript was wrong: its
absence is not evidence of anything. The driver's own comment in
`get_app_buf_ptr` says "for some reason,
we aren't getting all the interrupts we expect to be getting", so a completion
event that never arrives is a documented possibility — and a completion check
with no escape converts a corrupting bug into a hanging one, which is worse
because a hang leaves no next frame to diagnose it with. The worst case of the
repair is the behaviour it replaces.

**`usb-bao1x-serialflush-repair.patch` — two live bugs in the stock USB server.**
Required for the full image; without it the probe cannot complete.

1. `services/usb-bao1x/src/main.rs:748`, the `SerialFlush` handler's binary
   branch, does `buf.d.copy_from_slice(serial_buf.drain(..chars_avail).as_slice())`.
   `buf.d` is the client's freshly constructed `Vec::new()` (`lib.rs:239`) —
   length 0 — and `copy_from_slice` panics unless the lengths match. **The flush
   can therefore only deliver when there is nothing to deliver, and panics
   whenever there is.** The IRQ arrival path two hundred lines above gets it right
   with `extend_from_slice` (`:661`); the flush path was never fixed. The probe's
   watchdog reaches this state at the tail of every round trip — bytes land while
   no listener is parked, then a listener is parked, then the flush fires with a
   non-empty buffer — so it is not a theoretical race. One-word fix.
2. Same handler, `:732`: when the device is not `Configured` it `continue`s,
   skipping the listener release entirely. A client blocked in
   `serial_wait_binary()` then has no way out — a disconnect, a charge-only port
   or a hub that does not enumerate wedges it permanently, which is precisely the
   case the watchdog exists for. Only the hardware flush needs a configured
   device, so the state check now guards just that call.

**`xous-app-uf2-repair.patch` — the tool does not compile at `9844906`.** It is a
workspace member nothing in `cargo xtask` builds, and it has bit-rotted as fallout
from the post-quantum signing argument being threaded through
`SwapWriter::encrypt_to` without updating this caller. Three defects: a dead
`pq_sq` binding referencing two identifiers (`Params`, `die`) that do not exist in
the file, a missing `encrypt_to` argument, and an unconstrained type parameter in
the non-swap branch. Deleting `pq_sq` leaves three `slh_dsa` imports unused —
warnings, not errors; the patch leaves those upstream lines alone rather than
growing to cover them. The patch also spills `DEV_KEY_PQ` to a temp file, because
`encrypt_to` takes a *path*; it uses a unique name and `create_new` (O_EXCL) and
removes the file on the error path, so it is not the arbitrary-file-overwrite
primitive a fixed name under a world-writable `/tmp` would be.

The published `xous-tools 0.1.2` on crates.io *does* build, but it predates PQ
signing and emits a swap image with no PQ signature — 3840 bytes shorter than a
valid one, and it fails closed on any device with the `REQUIRE_PQ` one-way counter
set (`libs/bao1x-hal/src/sigcheck.rs:412`). Do not use it.

### A third upstream bug — no patch needed

`xous_swapper::Swapper::garbage_collect_pages()`
(`services/xous-swapper/src/lib.rs:100-110`) **returns a constant 0 to every
caller, on every kernel.** Round 3's transcript printed `free_pages=0` while the
swapper's own mirrored log line said 91 and 265 from the same two calls; the
swapper was right and the client library was reading the wrong field.

The swapper answers by mutating the scalar body in place — `scalar.arg1 =
free_pages` (`services/xous-swapper/src/main.rs:795`) — and the reply travels back
as the whole body: `ScalarMessage::to_usize()` packs `[id, arg1, arg2, arg3, arg4]`
(`xous-rs/src/definitions/messages/mod.rs:106`), `reply_and_receive_next` hands
that array to the kernel (`xous-rs/src/syscall.rs:1856`), and the kernel returns
`Result::Scalar5(arg0, arg1, arg2, arg3, arg4)` in that order
(`kernel/src/syscall.rs:562`). The client destructures the **first** field, which
is the message id — and `Opcode::GarbageCollect` is `0`. Reading `arg1` is the
entire fix.

It needs no patch here because it is an *app-side* read: the swapper on the badge
is already answering correctly, so the probe sends the opcode itself and reads
`arg1` (`gc_free_pages` in `probe/src/main.rs`, which carries the derivation).
Worth reporting upstream; costs no reflash. The one other in-tree caller,
`services/cram-console/src/cmds/mbox.rs:90`, discards the return value, which is
presumably why nobody noticed.

### Flashing procedure

Nothing in this repository performs any of these steps.

1. Confirm the three files exist in `probe/out/` and that you know which build
   they came from (see *Identifying what a badge is running*, below).
2. Hold `PROG` (the button closest to the USB connector) while plugging the badge
   into USB. It enumerates as a mass-storage volume labelled **`BAOCHIP`**. It is
   not a general-purpose drive.
3. Copy **all three** — `loader.uf2`, `xous.uf2`, `swap.uf2` — onto that volume.
4. `sync`, then cleanly unmount the volume.
5. Press `PROG` again to run.
6. Start `echo-host.py` on the CDC port **before** the probe's five-second startup
   delay expires — or start `reattach.sh` first and let it wait; see *Host echo*.

**An app-only update copies one file.** Once developer mode is tripped, a rebuilt
probe ships as `swap.uf2` alone: hold `PROG`, plug in, copy **only** `swap.uf2` to
`BAOCHIP`, `sync`, unmount, press `PROG`. Do not copy the older `loader.uf2` and
`xous.uf2` sitting beside it in `probe/out/` — they are unchanged, so copying them
does nothing but add two more chances to copy the wrong file. Everything from
*Backing out* onwards is unchanged.

**Backing out.** Keep your own copy of the badge's stock
`{loader,xous,swap}.uf2` — take it off the badge *before* the first flash. Nothing
here redistributes DEF CON's firmware. Repeating steps
2–5 with those three restores stock firmware. It does **not** restore the erased
secrets or decrement `DEVELOPER_MODE`; that part is one-way and permanent.

### Host echo

`echo-host.py` is the two receive legs' counterpart: a plain serial echo, stdlib
only (raw tty via `termios`; no pyserial). It answers exactly two requests and
counts everything the probe sends, so the transmit figure gets an independent
check.

| the probe sends | the host answers |
|---|---|
| `REQ\n` | exactly one 4096-byte page — the per-request latency leg |
| `STREAM <n>\n` | n pages back to back, in 64 KiB writes, with no waiting in between — one burst of the sustained-receive sweep |

It also measures **its own** turnaround for every `REQ` — from the read that
delivered the request to the completion of the reply write — and prints
min/median/max on a `host:` line at exit. That number is inside the badge's `rt:`
figure and cannot be separated on the badge, so it is measured on the end that
can see it. It includes any time the reply write spent blocked on flow control,
which is correct: the badge's latency does not end until the last byte lands.

```sh
ls /dev/cu.usbmodem*            # the badge enumerates one CDC-ACM node
./echo-host.py /dev/cu.usbmodem<serial> | tee probe-transcript.txt
```

Better, across power cycles:

```sh
./reattach.sh [transcript-path]   # default: badge/probe-transcript.txt
```

The badge's CDC node disappears when it powers down and comes back a moment after
it boots, which kills any reader holding it; the probe then waits five seconds and
starts talking. `reattach.sh` polls for the node, launches `echo-host.py` the
moment it appears, and loops when it goes away, writing attach and detach markers
into one transcript. It merges stderr in deliberately, so the diagnostics land
next to the report lines.

Report lines go to stdout and everything else to stderr, so piping stdout captures
exactly the transcript. Use the `cu.*` node, not `tty.*`: `tty.*` blocks on DCD,
which a CDC-ACM gadget does not assert. On Linux the node is `/dev/ttyACM0`; the
script sets `CLOCAL` explicitly, but that path is untested on real hardware.

This is deliberately **not** the Task 4 frame protocol. That protocol is what the
emulator will eventually speak; a framing layer here would be inside the thing
being timed.

`./test-echo-host.py` exercises the script against ptys standing in for the badge
— **20 cases, all passing**, and it is the regression net for anything changed
here. A whole `REQ\n` answered with exactly 4096 bytes; a `REQ\n` split across two
writes answered once (not zero, not twice); three batched into one write answered
three times; a request surviving the 8 KiB buffer trim; `STREAM 4` answered with
four pages; a `STREAM` split across three writes; `STREAM 48` spanning several
64 KiB bursts; a `STREAM` and a `REQ` in one write served in arrival order;
`STREAM 0` consumed without a reply; a report line *containing* the word `STREAM`
printed rather than eaten to the next newline; a `STREAM` with a non-numeric
argument treated as prose; traffic with no request drawing no reply; an all-fill
pseudo-line dropped; a long fill run before text stripped; a literal `Z` preserved
at both ends of a report line (`0x5a` *is* ASCII `Z`, and a blanket `strip`
corrupts real text); a line with a NUL printed as an escaped `repr` rather than
silently deleted; EOF exiting promptly with the byte counters printed rather than
busy-spinning; and a 20-round randomized fuzz over chunk boundaries carrying both
request forms. Only the badge end is untested.

## Identifying what a badge is actually running

A swap image carries no version string, but it does carry its build commit. The
`SwapSourceHeader.partial_nonce` at offset 4 of the decoded payload is, per
`tools/src/swap_writer.rs::git_rev`, **the low 16 hex digits of the commit the
image was built from**. That is the only handle on a stock badge's provenance, so
write it down when you need it:

```sh
# UF2 block 0 is 32 bytes of header then payload; payload[4..12] is the nonce,
# stored as raw bytes, so read it flat -- not with xxd's -e endian-swap.
xxd -s 36 -l 8 -p swap.uf2                      # stock -> 6d2808e2451ece1d
                                                # ours  -> 2db2922846ae4722

# It is the commit's *suffix*, so rev-parse cannot resolve it. Grep instead:
git -C "$XC" log --all --format='%H %ad' | grep '^[0-9a-f]*6d2808e2451ece1d '
# -> f3e687b2bc4314ca2dfc25566d2808e2451ece1d Wed Jul 29 16:24:36 2026 +0800
```

The stock badge image resolves to `f3e687b2bc4314ca2dfc25566d2808e2451ece1d`
(2026-07-29); ours to our build revision `9844906`. Diffing the two — 82 commits —
shows **no revision skew** in the syscall ABI, the IPC opcode numbering, or the
swap image format:

- `xous-rs`, `kernel/src`, `services/log-server`, `services/ticktimer-server`,
  `services/usb-bao1x`, `services/xous-swapper`, `loader/src/platform/bao1x`,
  `api/xous-api-{log,ticktimer,names}`: **zero changes**.
- `libs/bao1x-api`: +23 lines, purely additive — a new `Boot1DeveloperState`
  one-way enum in previously-unallocated slot 17. Slot 18 and every existing claim
  are unmoved.
- `tools/src/elf.rs`: a *fix* to `alignment_offset` — the first **contributing**
  section now sets the page phase, rather than the first section surviving an
  earlier filter. The newer tool is the safer one to pack with, so keep packing at
  `9844906`.

That is a genuine result and it is why the crates.io `xous 0.9.70` /
`xous-api-log 0.1.69` / `xous-api-ticktimer 0.9.70` / `xous-ipc 0.10.10` that
`probe/Cargo.toml` resolves are byte-identical to the in-tree ones. **It is not,
however, what decides whether a swap-only flash works.** That is a signing-policy
question a revision diff cannot see, and the answer is at the top of this file.

## Note on `probe/Cargo.toml`'s feature flags

Three dependencies exist only to make the graph compile, and each is commented in
place: `getrandom` (pinned + `[patch]`ed to xous-core's fork, which has the
riscv32-xous backend), `ux-api` (declared with default features on, which
`bao1x-hal` does not do), and `xous-swapper` with `board-baosec` (without a board
feature its `FlashPage` references an undefined `SPINOR_ERASE_SIZE`).

`board-baosec = []` and `oem-baosec-lite = []` in `[features]` are **empty and
inert**. The real board features are pinned on the `usb-bao1x` / `xous-swapper` /
`ux-api` dependency declarations, which is the safer arrangement; the two empty
features exist only so the documented command line is accepted verbatim. Do not
"fix" the dependency declarations on the strength of the command line.
