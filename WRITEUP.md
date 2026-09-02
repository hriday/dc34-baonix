# How this ended up the way it did

The `README.md` describes a thing that works. This describes how it got there,
which is a different and more interesting document, because most of the work was
not building an emulator. Most of the work was finding out why the badge's USB
stack would not carry the bytes.

Nothing here needs prior knowledge of the project. It does assume you are
comfortable with the idea of an interpreter, a page table and a serial port.

---

## One fact forces everything else

The premise was: boot a Linux that nixpkgs built, on the DEF CON 34 badge.
Not a vendor image, not a buildroot, not a hand-rolled kernel — something whose
`/nix/store` paths you could `ls` at a prompt and whose version banner said
`nixbld@localhost`.

The badge is a Baochip-1x: a **32-bit** RISC-V microcontroller, RV32-IMAC with an
MMU, 2 MiB of on-die SRAM, 8 MiB of off-chip PSRAM used as encrypted swap, a
128×128 monochrome OLED, and a USB-C port. No SD slot. No storage of any kind
that a guest OS could live in.

The obvious design is a riscv32 guest. Same width as the host, so you could
plausibly do something clever, and a 32-bit Linux is smaller.

nixpkgs does not have one. `riscv32-linux` is not a supported platform there;
riscv32 exists in nixpkgs only as a bare-metal cross target. `riscv64-linux` is
supported and well-exercised. Since "nixpkgs built it" was the entire point, the
guest architecture was decided by a table of supported platforms in someone
else's repository.

Everything downstream follows mechanically:

1. **The guest is riscv64.** Forced by nixpkgs.
2. **Therefore the badge must interpret.** You cannot execute RV64 instructions
   on an RV32 core. Not slowly, not with tricks — the register file is the wrong
   width. Every 64-bit add becomes at least two host operations; variable shifts
   and `DIV`/`REM` become considerably more.
3. **Therefore the guest needs 32 MiB of RAM.** A riscv64 Linux with a busybox
   initramfs does not fit in less. The badge has 2 MiB of SRAM, about 308 KiB of
   it free once Xous and its eleven services have taken theirs.
4. **Therefore the guest's RAM is not on the badge.** 8 MiB of swap cannot hold
   32 MiB, and there is no filesystem to put an image in. The only device
   attached to the badge that is big enough to hold 32 MiB is the laptop at the
   other end of the USB cable.
5. **Therefore there is a wire protocol, and it is on the critical path of every
   single memory access the guest makes.** Which is why the rest of this document
   is about a USB driver.

There is a smaller version of the same story inside the guest. `pkgsCross.riscv64`
targets `rv64gc`/`lp64d` — the D extension is in the ISA string and doubles are
passed in floating-point registers. The kernel escapes that because its own
Makefile forces `-march=rv64imac -mabi=lp64` from `CONFIG_*`, but nothing forces
it on ordinary packages, so the busybox from that set contains FP instructions and
this machine has no FPU. That was not reasoned out in advance; it was measured. An
`lp64d` busybox trapped at the eighth instruction of user code with `mcause=2`
(illegal instruction) and `mtval=0xb920`, which decodes as `c.fsd` — a
double-precision store. Worse, the emulator's `medeleg` did not delegate cause 2
to S-mode, so the trap went to M-mode, `mtvec` was 0, and the machine spun at
address 0 forever printing nothing. The userland now comes from a separate
static, soft-float, musl cross set, and `nix/guest/initramfs.nix` asserts it.

---

## Four defects in shipped firmware

The badge runs [Xous](https://github.com/betrusted-io/xous-core), a Rust
microkernel, with a USB stack of its own. This project is, as far as anyone
involved knows, the first workload on this hardware that both *sends* and
*receives* multi-packet payloads in a synchronous exchange. That turned out to
matter a great deal.

Four defects were found. Each is a patch in `badge/*.patch`, each carries its
rationale as a comment in the source it touches, and each is written to be
reported upstream. What follows is the mechanism of each, and — more usefully —
what actually identified it.

### 1. A flush that can only work when it has nothing to do

`usb-bao1x`'s `SerialFlush` handler ends with, in effect:

```rust
buf.d.copy_from_slice(serial_buf.drain(..chars_avail).as_slice());
```

`buf.d` is the client's freshly constructed `Vec::new()`. Length zero.
`copy_from_slice` panics unless the two slices are the same length.

So the flush succeeds when `chars_avail` is 0 and panics whenever it is not. The
handler can only deliver when there is nothing to deliver. The arrival path two
hundred lines above the same file gets it right with `extend_from_slice`; the
flush path was never fixed, presumably because nothing before this had a
periodic flush on its critical path.

It is on ours, and not as a backstop. A page response is 4,109 bytes and
`SERIAL_BINARY_BUFLEN` is 3,840, so the interrupt delivers the first chunk and
**there is no second interrupt to carry the tail** — only the periodic flush can.
Without this one-word fix, every page read strands its own tail and the link goes
quiet after the first request.

The same handler has a second bug: when the device is not `Configured` it
`continue`s, skipping the listener release entirely. A disconnect, a charge-only
port or a hub that does not enumerate then wedges a blocked client permanently —
which is precisely the case a watchdog exists for.

### 2. A shared 512-byte buffer, and only a count crossing to the consumer

This one took five hardware rounds, and it is the interesting one.

The symptom: the badge asked for a page, the host answered, the badge's decoder
got RISC-V `nop` padding instead of the page. Every layer we could instrument
said it was fine.

The instrumentation was thorough. The transport dumped the addresses and lengths
of rkyv's archive fields as they arrived and compared them against values computed
on the host: offsets, lengths, resolved pointers — **all correct, to the byte**.
The buffer was the right size, in the right place, with the right length recorded
in it. The content was wrong.

That is a strange failure. If a length crosses correctly and the bytes do not,
the bytes and the length were not travelling together.

**What identified it was periodicity.** The badge dumped 32 bytes at the payload
base and 16 bytes each at `base + 512` and `base + 1024`, and reported whether the
512-byte windows matched. They did: `base` and `base + 1024` were byte-identical,
`base + 512` was different. A **1024-byte repeat period** in a 3,840-byte payload.

Wire data from a page of a Linux kernel does not repeat with a period of exactly
two USB bulk packets. Two 512-byte blocks cycling through the payload is the
signature of a consumer re-reading one staging buffer several times.

Reading the driver with that hypothesis in hand made it obvious:

```
hw.rs:333     if let Ok(count) = serial.read(&mut usb.serial_rx) {
hw.rs:335         try_send_message(conn, new_scalar(IrqSerialRx, count, ...)).ok();
main.rs:595   serial_buf.extend_from_slice(&cu.serial_rx[..valid_bytes]);
```

`claim_interrupt` hands the interrupt handler the same `Box<Bao1xUsb>` the main
loop holds, so `usb.serial_rx` and `cu.serial_rx` are the same 512 bytes. There is
no queue and no copy at interrupt time — only a *byte count* crosses to the
consumer. A second packet arriving before the main loop drains the first message
satisfies the older count from the newer bytes. And `try_send_message(...).ok()`
drops the notification outright if the server's queue is full, *after* the buffer
has already been overwritten.

A 4,109-byte reply is nine such packets back to back.

There was a second half, found only after the first fix ran: with the copy in
place, the first 512 bytes of every delivery were correct and the tail was still
stale. `SerialPort::write`'s counterpart, `SerialPort::read`, moves **at most one
packet** out of the hardware per call, and one interrupt can cover several
arrivals. Taking one packet and returning left the rest in the endpoint. The
interrupt handler now drains, bounded by the ring's capacity so an interrupt
cannot run unboundedly.

**The part worth remembering is why the earlier probe never caught this.** The
throughput probe's host script answered every request with `b"\xa5" * 4096` — a
page of one repeated byte. Against such a page, **a duplicated packet is
byte-for-byte indistinguishable from a correct one.** The probe's only content
check was `d[0] != FILL || d[last] != FILL`, which such a page passes no matter
how scrambled its middle is. So "the probe receives real data over this exact
mechanism, on this exact badge" was never the reassurance it looked like, and it
pointed five rounds of debugging away from the driver.

Both ends are fixed now: the host's test page is position-dependent, every
512-byte block distinct, and the probe checks *every* byte against
`fill_byte(stream_offset + k)`. A check that cannot fail is not cheaper than one
that can. It is worthless.

### 3. The log server deadlocking the system it reports on

`usb_send_str` in the Xous log server mirrors each log line to `usb-bao1x` with
`Buffer::send`, under a comment saying *"this API doesn't block."*

It blocks. `Buffer::send` is `xous::send_message(.., Message::Move(..))`, and the
kernel's answer to `ServerQueueFull` for a blocking `SendMessage` is
`retry_syscall(pid, tid)` — it parks the caller and retries the instruction. Only
`try_send_message` returns the error to the caller.

That closes a cycle with any service that logs:

1. `usb-bao1x`'s main loop calls `log::error!`, which is a **blocking** lend to
   the log server.
2. The log server handles the record and mirrors the line back to `usb-bao1x`.
3. `usb-bao1x`'s 128-slot message queue is full — which is *exactly the condition
   its own log line was reporting*, because the queue fills when the main loop
   falls behind the receive interrupt. The log server parks in `retry_syscall`.
4. Neither runs again. Every process that logs then blocks behind the log server.

No fault, no frames, no output at all. On the wire it looked like: two
`serial rx ring overflowed` lines, then five minutes of silence. The badge's own
diagnostic was the trigger for its own deadlock.

The general statement is worth carrying to other systems: **a blocking call from
a log server into a service that logs is a deadlock, in whichever direction it is
written.**

### 4. A fix that was tried, made things worse, and was reverted

The transmit direction has the exact mirror of defect 2, and this one is not
fixed. It is avoided.

`CorigineWrapper::write` copies every bulk IN packet into a **single** 512-byte
hardware buffer. `get_app_buf_ptr` computes `new_index = enq + mps` and resets
`enq` to 0 whenever `new_index + mps > CRG_UDC_APP_BUF_LEN`; with
`mps == CRG_UDC_APP_BUF_LEN == 512` that is *every* call, so the address is the
same one every time. It then enqueues a transfer pointing at that buffer with no
check that the previous transfer completed. Nine packets handed over back to back
are nine writes into one buffer.

Everything this link had ever successfully sent fit in **one** packet — a 13-byte
page request, and console lines short enough not to split. The first transmit that
does not is the first page writeback.

Finding *that* was a nice piece of arithmetic. A boot stopped at 1,313,691
instructions with `misses=258 evictions=1 writebacks=0` and a memory fault at
`0x81fc800c`, and the transcript contained **device-tree content** — unframed,
badge→host, and absent from the guest console. The badge has no device tree of
its own; the only device-tree bytes it has ever held are ones it received.

Those bytes were not a leaked buffer. They were the badge's first `WriteReq`. The
laptop dry run measures the writeback sequence directly:

```
writebacks: 3921 WriteReq frames; first ten (requests before it, page):
  [(258, 1275), (261, 1200), (263, 1201), ...]
```

Page 1275 is the DTB's own page, sent after exactly 258 requests — matching the
hardware counters to the digit. And the page cache reads the incoming page
*first* and evicts *after*, so a failed writeback leaves both `writebacks` and
`evictions` where they were and surfaces as a fault at the address of the load
that missed. The badge was never failing to read the page it faulted on. It was
failing to write a different page back, inside the same call.

**The patch that was written for this is in the tree and is not applied.**
`bao1x-hal-usb-in-completion.patch` makes the endpoint return `WouldBlock` while a
transfer is in flight. That is exactly the answer the layers above it were written
for: `usbd-serial`'s `flush` sends one packet, keeps the rest, propagates
`WouldBlock`, and its `endpoint_in_complete` calls `flush` again when the transfer
event lands. On paper it turns the whole class of bug into a completion-driven
pipeline at hardware speed.

It made things worse, and the reason is a good one.

**The refusal is invisible from above.** `SerialPort::write` returns bytes
*buffered*, not bytes sent, and it tolerates its own `flush` failing with
`WouldBlock`. So a refused packet still comes back as `Ok(512)`. The refused
bytes then wait for either a completion interrupt — on a driver whose own comment
says *"for some reason, we aren't getting all the interrupts we expect to be
getting"* — or a 5 ms watchdog. Neither is on the send's critical path.

And the bytes that get stranded are the *tail*, which is where the frame's CRC
lives. So the host's decoder receives an incomplete frame and holds it: neither
accepted nor reported as discarded. Silence, with no diagnostic on either end.

The bisect is one variable. The boot that reached a shell had **no** patch and a
1 ms app-side transmit pace, and wrote the host's memory image back repeatedly.
The commit that added the patch and set the pace to 0 produced **zero** successful
writebacks, and so did every run after — including the one that put the
millisecond back. Pace 1 works without the patch and fails with it, and the only
frame kind the patch can affect (multi-packet bulk IN) is the only frame kind that
fails.

So the driver source has now been wrong about this twice, in opposite directions,
and the standing rule is that going back to a zero pace requires a hardware run
whose writebacks climb — not an argument from the code. The correct fix is to make
the refusal **observable**: have `usb-bao1x` report `flush`'s result alongside
`total_sent`, so the sender can wait for it, rather than hiding the refusal inside
the HAL.

This is the part that still does not work. With the patch reverted and the
millisecond in place, writebacks succeed — but the transmit path can still wedge
inside `serial_send`, and when it does, that run is over.

---

## Instruments, not guesses

Two of the four were found by instruments built specifically to answer "what
exactly did you throw away?", and they are more transferable than the bugs.

**The decoder reports what it discards.** The frame decoder on the badge captures
a 64-byte sample of every discarded run plus an *exact* count, and both reach the
wire through the log mirror. That sounds unremarkable until you see what it
distinguishes:

| first bytes of the discarded hex | what it means |
|---|---|
| `c1 b0 02 04 10 …` | the frame arrived intact and was rejected on its CRC |
| a shifted or interleaved copy | something upstream of the decoder is mangling the stream |
| `c1 b0 01 04 00 …` (13 bytes) | the CDC endpoint is looping our own request back |
| nothing discarded at all | the bytes never reached the decoder despite being counted |

The last two are distinguishable *because* a well-formed frame of an unwanted type
is held rather than discarded: a loopback shows no discarded bytes at all, while a
CRC rejection shows thousands.

The host's decoder does the same thing in the other direction, and goes further —
a discarded run that is frame-shaped gets one line naming its type, its page and,
for a page-sized payload, whether any two 512-byte blocks are byte-identical:

```
[rv64-host: discarded 4109 bytes; frame-shaped at offset 0: WriteReq,
 declared len 4100, page 1275; 8 full 512-byte blocks, REPEATED: 4=5]
```

`REPEATED: 4=5` is the transmit-side defect naming itself.

**Byte arithmetic exonerating a suspect.** The fourth hardware run of the receive
investigation received **16,436 bytes** and decoded no frame at all. A page reply
is 4,109 bytes. 16,436 = 4 × 4,109, exactly. Four complete replies, in order, with
a healthy reader on every counter, and nothing decoded.

That single multiplication cleared the receive path outright. Nothing was being
lost, truncated or reordered — the right number of bytes arrived in the right
order. Whatever was wrong was in *content*, not in *count*, which is what turned
the search toward a staging buffer and away from the framing, the transport and
the queue depths where four rounds had already been spent.

An exact division is a cheap, strong result. It is worth arranging your counters
so you can do one.

---

## Two mistakes, which are the useful part

**The display is 16 columns, not 18.** The project's specification derived an
18-column grid from 128 / 7, seven being the width of a glyph in xous-core's mono
font. Every ASCII entry in that font's `WIDTHS` table really is 7. But 7 is the
width of the glyph's *ink*; the *advance* is `wide + kern`, `DEFAULT_KERN` is 1,
and 128 / 8 is 16.

The wrong number survived a spec review and a first implementation. What settled
it was not re-reading the derivation and not a photograph of the badge. It was
taking xous-core's **actual typesetter and blitter**, running them on the laptop
into a 128×128 bit buffer, and looking at where the pixels landed: 18 characters
demonstrably wrap, and eight rows of 16 land on eight clean 15-pixel bands.

The guest's `/init` was then corrected to 16 columns, and a host test asserts the
exact grid of characters the display would show, store path included. So the
consequence is visible: at 18 columns, the 32-character `/nix/store` hash would
have split 18 + 14 and run onto a ninth row of an eight-row screen. The
screenshot the whole project exists to produce would have been wrong, and nobody
would have known why.

The general lesson: when a derived constant matters, run the code that consumes
it rather than re-checking the derivation. The derivation is where the error is.

**A "fix" that broke writebacks for hours.** The in-completion patch above is the
other one, and it is worth restating as a mistake rather than as a finding. It was
a correct diagnosis of a real bug, a patch that made the code match its own
documented contract, and applying it took the system from "writebacks work" to
"writebacks never work" — silently, with no error anywhere, for several hardware
runs.

What eventually resolved it was not more reading. It was a **regression bisect
against a known-good run**: there existed one recorded boot that reached a shell
and wrote memory back repeatedly, its exact configuration was known, and every
subsequent run differed from it in exactly two variables. Setting them back one
at a time found the culprit in two runs.

That only worked because the good run was written down. On hardware with no
debugger, no storage and a power cycle per attempt, the transcripts in
`badge/logs/` are not documentation. They are the instrument.

---

## What the numbers say

The run loop times itself into three disjoint spans: `wall` (everything), `link`
(every millisecond blocked inside a page exchange — accumulated inside the
transport, because a page fault happens deep inside `Cpu::step` where the loop
cannot see it), and `svc` (console pump, log mirror, heartbeat, screen repaint).
`cpu = wall − link − svc`, and `ips = insn / cpu`.

A real line, mid-run:

```
rv64 rate: insn=117626295 wall=615452ms link=23521ms svc=3258ms cpu=588673ms ips=199816
```

| | measured |
|---|---|
| Guest instructions/second on the badge | 146,000–207,000; 190–207 K in steady state |
| Instructions in one boot to a shell | 173,500,000 (exact) |
| Page operations in one boot | ~2,952 |
| Share of wall time spent on page I/O | **3.8%** (23.5 s of 615 s) |
| 4 KiB page round trip | min 2 / mean 2.0 / max 3 ms |
| USB transmit, badge → host | 5,797 KiB/s (8 MiB in 1,413 ms) |
| Xous swap fault | ~4 ms |
| Laptop dry run, same code | 38,351,016 ips |

The headline is the page-I/O share. **This is compute-bound, not I/O-bound.** An
earlier and quite confident explanation of the forty-minute boot blamed the
host-side transmit pacing; the instrument closed that account and showed the
pacing was about five minutes of forty. The wire is not the problem. The
interpreter is — 190 K instructions per second against 38.4 M for the same code
on a laptop, a factor of about 200.

There is one honest caveat the instrument cannot resolve, and it is the strangest
fact in the project. The page cache is 1,024 frames — 4 MiB — against ~308 KiB of
free SRAM, so most of the cache lives in Xous's encrypted off-chip swap. A page
cache *hit* on a swapped-out frame therefore costs a ~4 ms swap fault, and that
time lands in `cpu` where nothing here can see it. Meanwhile a page fetched from
the laptop over USB costs ~2 ms.

**Reading a page of guest memory from a laptop over a serial cable is faster than
reading it out of the badge's own memory.** Only locality keeps that from
dominating the run.

The path to going faster has been profiled rather than guessed. The biggest single
win available is a decoded-instruction cache keyed by physical PC, estimated at
1.8–2.2×, and the risk that cannot be measured on a laptop is memory: 4,096
entries at 12–16 bytes is 64 KiB against 308 KiB free, and if it pushes the page
cache further into swap, a 4 ms fault eats the entire win. The same profile
overturned a prior: the 32-entry TLB has a 0.5% miss rate here, so doubling it
would buy about 1%. The MMU is not slow because it walks. It is slow because it is
called twice per instruction.

---

## What it is honestly like

A boot takes roughly twenty minutes. You start the host before the badge, because
the badge asks for its first page as soon as it powers on and the transport gives
up after about two seconds, so a human cannot win that race by hand. Then you
watch kernel messages arrive at reading speed for a third of an hour.

At the end there is a busybox prompt on a 128×128 screen, sixteen characters wide.
Typing `uname -r` produces `6.12.103` after a couple of seconds per keystroke.
`ls /nix/store` prints a real store hash that a real nixpkgs build put there.

It can also fail. The transmit path can wedge inside `serial_send` and end the run
with no diagnostic. Twenty-three hardware runs produced the transcripts in
`badge/logs/`, and most of them are failures. They are kept because for several of
them they are the only record that a thing happened at all.

The most reusable output of the project is probably not the emulator. It is the
five patch files in `badge/`, and the observation that on hardware with no
debugger, no storage and a power cycle per attempt, almost every question can be
answered by running the real code somewhere it can be observed — or by reading
the source until you can cite a line number — and the few that genuinely cannot
should be measured once, deliberately, by an instrument built for the purpose.
