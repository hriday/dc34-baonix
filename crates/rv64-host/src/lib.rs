//! Host-side (`std`) support for the `rv64` emulator core: a file-backed
//! `MemBacking`, a minimal ELF loader, and the `riscv-tests` harness that
//! drives them.
//!
//! The `rv64` crate itself stays `no_std`; everything that needs the
//! filesystem or an allocator-backed error type lives here.

pub mod elf;
pub mod fdt;
pub mod hostfile;
pub mod rawtty;
pub mod serve;
pub mod stdout_sink;

pub use hostfile::HostFile;
pub use stdout_sink::StdoutSink;

use rv64::backing::{FakeBacking, MemBacking};
use rv64::bus::Bus;
use rv64::cache::{PageCache, Stats};
use rv64::csr::{self, Priv};
use rv64::exception::Exception;
use rv64::sbi::SbiOutcome;
use rv64::uart::{ConsoleSink, VecSink};
use rv64::Cpu;
use std::path::Path;

/// Writes an arbitrary byte blob at a guest physical address: 8 bytes at a
/// time where alignment allows, one byte at a time otherwise. This is the
/// DTB's loader — a raw blob, not an ELF, so `elf::load`'s segment handling
/// (which also zero-fills a `.bss` tail the DTB has none of) does not apply.
///
/// Lives here rather than in `main.rs` so `tests/dtb.rs` can call the exact
/// function the CLI uses to place `--dtb`, instead of a hand-copy that could
/// drift from it.
pub fn write_blob<B: MemBacking, S: ConsoleSink>(
    bus: &mut Bus<B, S>,
    addr: u64,
    data: &[u8],
) -> Result<(), Exception> {
    let mut off = 0usize;
    while off < data.len() {
        let a = addr + off as u64;
        if a.is_multiple_of(8) && data.len() - off >= 8 {
            let word = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
            bus.store(a, 8, word)?;
            off += 8;
        } else {
            bus.store(a, 1, data[off] as u64)?;
            off += 1;
        }
    }
    Ok(())
}

/// Set this to make a missing integration-suite prerequisite a hard failure
/// instead of a skip. The devShell sets it, because inside `nix develop`
/// every prerequisite is present by construction — so a skip there means
/// something is wrong with the environment, not with the developer.
pub const REQUIRE_SUITES_VAR: &str = "RV64_REQUIRE_SUITES";

/// What an integration suite does when the external tool or fixture it
/// needs is missing.
///
/// All three of this workspace's integration suites (`riscv_tests.rs`,
/// `differential.rs`, `dtb.rs`) sit behind something that only exists
/// inside `nix develop` — `RISCV_TESTS`, `spike`, `dtc` — and each returns
/// early without it. That keeps a plain `cargo test --workspace` usable,
/// but libtest captures stderr for *passing* tests, so the skip notice is
/// swallowed and the developer sees three green tests that ran nothing.
/// Every non-vacuity assertion in all three sits after the early return.
///
/// The fix is not to remove the skip but to make it refusable: with
/// [`REQUIRE_SUITES_VAR`] set, a missing prerequisite panics, so an
/// environment that is *supposed* to have the tools can demand the suites
/// actually ran rather than trusting that they did.
pub fn suite_prerequisite_missing(test: &str, reason: &str) {
    report_missing_prerequisite(std::env::var_os(REQUIRE_SUITES_VAR).is_some(), test, reason)
}

/// The policy itself, split from the environment lookup so it can be tested
/// in both directions without mutating process-global state.
fn report_missing_prerequisite(required: bool, test: &str, reason: &str) {
    assert!(
        !required,
        "{test}: {reason}. {REQUIRE_SUITES_VAR} is set, so this suite is \
         required to run and a skip is a failure."
    );
    eprintln!(
        "SKIPPED {test}: {reason}. Run inside `nix develop` to execute it, \
         or set {REQUIRE_SUITES_VAR} to make this a failure instead."
    );
}

/// Guest physical address a raw kernel `Image` is loaded at, and the address
/// the CPU starts executing from in either case — fixed by the boot protocol
/// this emulator implements rather than read from the image.
pub const KERNEL_LOAD_ADDR: u64 = 0x8020_0000;

/// Length of the riscv64 boot header at the front of every `Image`
/// (`struct riscv_image_header`, `arch/riscv/include/asm/image.h`).
pub const RISCV_IMAGE_HEADER_LEN: usize = 64;

/// `RISCV_IMAGE_MAGIC2`, at offset 56. The older 8-byte `"RISCV\0\0\0"` at
/// offset 48 is deprecated upstream, so this is the field to key on.
pub const RISCV_IMAGE_MAGIC2: &[u8; 4] = b"RSC\x05";

/// The memory footprint a raw riscv64 `Image` declares in its own boot
/// header: `image_size` at offset 16, little-endian, which the kernel links
/// as `_end - _start`.
///
/// **This is not the file's length, and the difference is the whole point.**
/// `arch/riscv/boot/Makefile` produces `Image` with `objcopy -O binary`,
/// which emits only sections that have contents; `.bss` is `NOBITS` and is
/// dropped. So the file stops at `__bss_start` while the kernel goes on to
/// occupy everything up to `_end` — 313,344 bytes further for the guest
/// kernel this project builds. `head.S` zeroes that range in `clear_bss`
/// *before* `setup_vm` reads the device tree, so anything the runner parks
/// at `load_addr + file_length` is either clipped or wiped outright.
///
/// Returns `None` when the blob carries no valid header, which is not a
/// hard error only because the unit tests here load short synthetic blobs
/// that are not kernels at all. Every real `Image` has one; the CLI warns
/// when it is missing rather than letting the weaker fallback pass unnoticed.
pub fn riscv_image_footprint(bytes: &[u8]) -> Option<u64> {
    if !has_riscv_image_header(bytes) {
        return None;
    }
    let size = u64::from_le_bytes(bytes[16..24].try_into().ok()?);
    // A footprint smaller than the file itself is a corrupt header, not a
    // kernel that somehow occupies less memory than it takes on disk. Fall
    // back rather than placing the DTB *inside* the image.
    (size >= bytes.len() as u64).then_some(size)
}

/// Whether the blob carries a riscv64 boot header at all, keyed on
/// `RISCV_IMAGE_MAGIC2`.
///
/// Split out from [`riscv_image_footprint`] because that returns `None` for
/// two quite different reasons — no header, or a header declaring a
/// footprint smaller than the file — and a diagnostic that conflates them
/// sends the reader looking for the wrong problem.
pub fn has_riscv_image_header(bytes: &[u8]) -> bool {
    bytes.len() >= RISCV_IMAGE_HEADER_LEN && &bytes[56..60] == RISCV_IMAGE_MAGIC2
}

/// The load offset from the base of RAM that a raw `Image` was linked for
/// (`text_offset`, offset 8 in the boot header).
///
/// The runner does not honour this — it always writes to
/// [`KERNEL_LOAD_ADDR`] — so a kernel linked for a different offset would
/// be loaded at the wrong address and hang before it can say why. Rather
/// than start relocating, [`load_kernel`] reads this and refuses.
pub fn riscv_image_text_offset(bytes: &[u8]) -> Option<u64> {
    has_riscv_image_header(bytes).then(|| u64::from_le_bytes(bytes[8..16].try_into().unwrap()))
}

/// Loads a kernel image and returns the guest physical address one byte past
/// the end of the memory the kernel will occupy — `.bss` included, which is
/// what makes it safe to place the DTB and initrd there. See
/// [`riscv_image_footprint`] for why the file's length is the wrong number.
///
/// The format is decided by the file's own first four bytes, not by a flag:
///
///  - `\x7fELF` — an ELF64 loaded at its `PT_LOAD` segments' *physical*
///    addresses, as before.
///  - anything else — a raw riscv64 `Image` (`arch/riscv/boot/Image`, which
///    is what the guest build produces) written verbatim at
///    [`KERNEL_LOAD_ADDR`]. It has no headers of any kind, so there is
///    nothing else to key on.
///
/// Sniffing rather than requiring a flag matters because *neither* format
/// alone is sufficient. A raw `Image` obviously has no ELF header. But
/// `vmlinux` — the obvious ELF alternative — does not load either:
/// `arch/riscv/kernel/vmlinux.lds.S` sets no `AT>`, so a riscv64 `vmlinux`'s
/// `PT_LOAD` carries `p_paddr == p_vaddr == 0xffffffff80000000`, and
/// `elf::load` (correctly, in general) writes to `p_paddr` — which is
/// nowhere near guest RAM and faults immediately. So an ELF-only `--kernel`
/// can load neither artifact a real riscv Linux build produces.
///
/// A file whose magic *is* `\x7fELF` but which is not a loadable riscv64
/// object is reported as a broken ELF rather than falling back to the raw
/// path: silently writing a corrupt ELF into guest memory as if it were an
/// `Image` would produce a blank-screen hang instead of a diagnostic.
pub fn load_kernel<B: MemBacking, S: ConsoleSink>(
    bus: &mut Bus<B, S>,
    bytes: &[u8],
) -> Result<u64, String> {
    if bytes.starts_with(b"\x7fELF") {
        elf::load(bus, bytes).map_err(|e| e.to_string())?;
        return elf::extent(bytes).map_err(|e| e.to_string());
    }
    // The boot protocol lets a kernel name the offset from the base of RAM
    // it was linked for. This runner implements exactly one, so a kernel
    // asking for another must be refused rather than loaded at the wrong
    // address — that failure looks like a hang with no output, which is the
    // single hardest symptom to diagnose in this whole project.
    if let Some(want) = riscv_image_text_offset(bytes) {
        let ours = KERNEL_LOAD_ADDR - rv64::RAM_BASE;
        if want != ours {
            return Err(format!(
                "raw kernel image declares text_offset {want:#x}, but this runner \
                 loads at {KERNEL_LOAD_ADDR:#x}, i.e. {ours:#x} above RAM base \
                 {:#x}. Loading it anyway would put the kernel at an address it \
                 was not linked for and hang with no output.",
                rv64::RAM_BASE
            ));
        }
    }
    write_blob(bus, KERNEL_LOAD_ADDR, bytes).map_err(|e| {
        format!(
            "raw kernel image ({} bytes) does not fit in guest memory at \
             {KERNEL_LOAD_ADDR:#x}: {e:?}",
            bytes.len()
        )
    })?;
    let span = riscv_image_footprint(bytes).unwrap_or(bytes.len() as u64);
    // `span` comes straight from the file's `image_size` field when the
    // header is present, and `riscv_image_footprint` only bounds it from
    // below (`size >= bytes.len()`) — a crafted 64-bit `image_size` can
    // still be large enough that adding it to `KERNEL_LOAD_ADDR` overflows
    // `u64`. Unchecked, that panics a debug build ("attempt to add with
    // overflow") and silently wraps in release. Same class of corrupt
    // header the "declares an image_size smaller than the file itself"
    // diagnostic above already names, so say so rather than let a bogus
    // huge value read as a real one.
    KERNEL_LOAD_ADDR.checked_add(span).ok_or_else(|| {
        format!(
            "raw kernel image ({} bytes) declares image_size {span:#x}, which overflows \
             the address space when added to the load address {KERNEL_LOAD_ADDR:#x} — \
             this is a corrupt boot header, not a kernel too large to load",
            bytes.len()
        )
    })
}

/// Guest physical addresses of the blobs the runner places above the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootLayout {
    /// Where the DTB goes, and what lands in `a1`.
    pub dtb: u64,
    /// `(start, end)` of the initrd, end exclusive — the two values that go
    /// into `/chosen`'s `linux,initrd-start` / `linux,initrd-end`. `None`
    /// when the run has no initrd.
    pub initrd: Option<(u64, u64)>,
}

/// Places the DTB and initrd above `kernel_end` (the value [`load_kernel`]
/// returns — a *memory* extent, not a file length).
///
/// Two alignment rules, and neither is cosmetic:
///
///  - The DTB is 8-byte aligned, as the DT spec requires.
///
///  - The initrd starts on a **page** boundary above the end of the DTB.
///    `reserve_initrd_mem()` (`init/initramfs.c`) rounds the region *down*
///    to a page before calling `memblock_is_region_reserved()`, and bails
///    out with "INITRD: ... overlaps in-use memory region - disabling
///    initrd" if that page touches anything already reserved. The kernel
///    image is reserved before that check runs
///    (`arch/riscv/mm/init.c:245`, inside `setup_bootmem`, versus `:299`
///    for `reserve_initrd_mem`), so an initrd sharing the kernel's last
///    page is discarded, and the boot ends at `No working init found` with
///    the one line of explanation far above the panic.
///
///    Note what this does *not* protect against, because the obvious
///    symmetric claim is false on this architecture: the **DTB is not yet
///    reserved** at that point. riscv reserves it at `:314`, after the
///    initrd check, and deliberately avoids `early_init_fdt_reserve_self()`
///    — see the comment at `:308`, `__pa()` does not work on the fixmap
///    address the DTB is reached through. So sharing a page with the DTB
///    would not be caught here; it would merely get the DTB's page swept
///    into the initrd's reservation and never freed.
///
///    The kernel-image case is also latent rather than active in the config
///    this project ships: `_end` is page-aligned only because
///    `CONFIG_EFI=y` appends `ALIGN(PECOFF_SECTION_ALIGNMENT)` in
///    `vmlinux.lds.S`. Turn EFI off — which `kernel.fragment` explains is
///    one `CONFIG_NONPORTABLE` away — and `_end` lands wherever `.bss`
///    ends. The alignment is kept because it costs at most one page, it is
///    what every other bootloader does, and it is the difference between a
///    guarantee and an accident.
///
/// Returns an error rather than truncating if the result runs past the end
/// of guest RAM, since a partially-written initrd fails as a corrupt cpio
/// ("Invalid magic at start of compressed archive") rather than as the
/// out-of-memory condition it actually is.
pub fn boot_layout(
    kernel_end: u64,
    dtb_len: usize,
    initrd_len: Option<usize>,
) -> Result<BootLayout, String> {
    let page = rv64::PAGE as u64;
    let ram_end = rv64::RAM_BASE + rv64::RAM_SIZE;

    let dtb = kernel_end.next_multiple_of(8);
    let dtb_end = dtb + dtb_len as u64;

    let initrd = match initrd_len {
        None => None,
        Some(len) => {
            let start = dtb_end.next_multiple_of(page);
            Some((start, start + len as u64))
        }
    };

    let top = initrd.map_or(dtb_end, |(_, end)| end);
    if top > ram_end {
        return Err(format!(
            "boot images do not fit in guest RAM: kernel ends at {kernel_end:#x}, \
             dtb at {dtb:#x} (+{dtb_len} bytes){}, which reaches {top:#x} — past \
             the top of RAM at {ram_end:#x}",
            initrd.map_or(String::new(), |(s, e)| format!(
                ", initrd at {s:#x} (+{} bytes)",
                e - s
            )),
        ));
    }
    Ok(BootLayout { dtb, initrd })
}

/// Instruction budget for a single ISA test. The longest of them settles in
/// well under a hundred thousand steps; the cap exists so that an emulator
/// bug that turns a test into an infinite loop is reported as a timeout
/// against a named test rather than hanging the suite.
const MAX_STEPS: u64 = 10_000_000;

/// The `p` environment's marker for "a trap the test did not expect":
/// `other_exception` does `ori TESTNUM, TESTNUM, 1337` before reporting.
const UNEXPECTED_TRAP: u64 = 1337;

/// Recovers the sub-test number from a `tohost` verdict that carries the
/// [`UNEXPECTED_TRAP`] marker.
///
/// The two failure paths in `riscv_test.h` encode TESTNUM differently, and
/// conflating them halves every reported number. `RVTEST_FAIL` shifts —
/// `sll TESTNUM, TESTNUM, 1; or TESTNUM, TESTNUM, 1` — but the
/// `other_exception` path does *not*: it does `ori TESTNUM, TESTNUM, 1337`
/// and falls straight through to `write_tohost`, which stores `gp`
/// unshifted. So the marker is cleared from the value as-is, with no shift.
///
/// The recovery is lossy where `n` and 1337 share bits (1337 has bit 0 set,
/// so an odd `n` loses it), which is inherent to an `ori` and not something
/// a different extraction could fix. Diagnostics-only either way — but
/// diagnostics are the entire reason this harness exists, and that number
/// is the first thing anyone debugging a conformance failure greps for.
fn unexpected_trap_subtest(tohost: u64) -> u64 {
    tohost & !UNEXPECTED_TRAP
}

/// Loads and runs one `riscv-tests` ISA binary to its verdict.
///
/// The protocol comes from `env/p/riscv_test.h`: the test ends by storing
/// its result to the `tohost` symbol and then spinning. `1` is a pass; any
/// other value is `(n << 1) | 1` where `n` is the number of the sub-test
/// that failed (`gp` at the point of failure), or `n | 1337` for a trap the
/// test's own handler did not expect.
///
/// This drives `Cpu::step` and delivers exceptions with `Cpu::trap`
/// directly, rather than going through `Cpu::step_trapping`.
/// `step_trapping` intercepts `ecall` from S-mode and routes it to the SBI
/// stub — correct for the Linux guest this emulator is built for, but fatal
/// here: the `riscv-tests` pass/fail protocol *is* an `ecall`, issued from
/// whatever privilege the test runs at, and the `rv64si-p-*` suite runs at
/// S-mode. Servicing those as SBI calls would swallow the verdict and every
/// supervisor test would time out. riscv-tests validates the ISA, not the
/// firmware layered above it, so the firmware layer is left out.
pub fn run_test_elf(bytes: &[u8]) -> Result<(), String> {
    let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
    let mut bus = Bus::new(PageCache::new(FakeBacking::new(pages), 64), VecSink::default());

    let entry = elf::load(&mut bus, bytes).map_err(|e| e.to_string())?;
    let tohost = elf::find_symbol(bytes, "tohost")
        .ok_or_else(|| "no `tohost` symbol; not a riscv-tests binary".to_string())?;

    let mut cpu = Cpu::new(entry);
    for _ in 0..MAX_STEPS {
        // Before `step`, never inside it: `step` clears its private
        // `next_pc` before dispatch and applies it at the end, so a trap
        // vector installed from within would be overwritten by the
        // interrupted instruction's own fallthrough pc.
        cpu.check_interrupts(&mut bus);

        match cpu.step(&mut bus) {
            Ok(()) => {}
            // Not a RISC-V exception — the backing store itself failed, so
            // there is nothing to deliver to the guest.
            Err(Exception::BackingFailure(a)) => {
                return Err(format!("backing store failed at {a:#x}"))
            }
            Err(e) => cpu.trap(e),
        }
        bus.clint.tick(1);

        let v = bus
            .load(tohost, 8)
            .map_err(|_| format!("tohost ({tohost:#x}) is not readable guest memory"))?;
        if v == 0 {
            continue;
        }
        return match v {
            1 => Ok(()),
            // The suite's own trap handler ORs 1337 into the sub-test
            // number when it takes a trap it did not expect, so a verdict
            // with those bits set means "the emulator trapped where the
            // reference hardware would not" rather than "instruction N
            // computed the wrong value". Those are very different bugs, and
            // the trap CSRs say which instruction did it — report them.
            _ if v & UNEXPECTED_TRAP == UNEXPECTED_TRAP => Err(format!(
                "unexpected trap: mcause = {:#x}, mepc = {:#x}, mtval = {:#x} \
                 (sub-test {}, tohost = {v:#x})",
                cpu.csrs.read(csr::MCAUSE),
                cpu.csrs.read(csr::MEPC),
                cpu.csrs.read(csr::MTVAL),
                unexpected_trap_subtest(v),
            )),
            _ => Err(format!("sub-test {} failed (tohost = {v:#x})", v >> 1)),
        };
    }
    Err(format!("no verdict after {MAX_STEPS} instructions (pc = {:#x})", cpu.pc))
}

/// How a run through [`run_until`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The guest issued an SBI shutdown request after `executed`
    /// instructions.
    Shutdown { executed: u64 },
    /// `max_insns` instructions ran without the guest asking to stop.
    Capped { executed: u64 },
    /// The backing store failed at `addr` after `executed` instructions.
    BackingFailure { executed: u64, addr: u64 },
    /// The guest took a trap that was not delegated to S-mode, so it landed
    /// in M-mode — where this emulator has no handler. See [`run_until`] for
    /// why that is a dead end rather than something to continue from.
    MachineTrap { executed: u64, mcause: u64, mepc: u64, mtval: u64 },
}

/// Names the standard RISC-V exception causes this machine can actually
/// deliver to M-mode, so a diagnostic says "illegal instruction" rather than
/// making the reader look up `mcause = 0x2`.
fn mcause_name(mcause: u64) -> &'static str {
    // Bit 63 distinguishes interrupts from exceptions. `Csrs::default` never
    // sets `mie.MTIE`, so an M-mode *interrupt* cannot be delivered here —
    // but naming it is one line and beats printing "unknown" if that ever
    // changes.
    if mcause >> 63 == 1 {
        return "interrupt";
    }
    match mcause {
        0 => "instruction address misaligned",
        1 => "instruction access fault",
        2 => "illegal instruction",
        3 => "breakpoint",
        4 => "load address misaligned",
        5 => "load access fault",
        6 => "store/AMO address misaligned",
        7 => "store/AMO access fault",
        8 => "environment call from U-mode",
        9 => "environment call from S-mode",
        11 => "environment call from M-mode",
        12 => "instruction page fault",
        13 => "load page fault",
        15 => "store/AMO page fault",
        _ => "unknown cause",
    }
}

impl RunOutcome {
    /// Instructions retired before the run stopped, whichever way it stopped.
    pub fn executed(&self) -> u64 {
        match *self {
            RunOutcome::Shutdown { executed }
            | RunOutcome::Capped { executed }
            | RunOutcome::BackingFailure { executed, .. }
            | RunOutcome::MachineTrap { executed, .. } => executed,
        }
    }

    /// The same outcome with its instruction count replaced. Used by
    /// [`boot_capturing`], which drives [`run_until`] in slices so it can
    /// watch the console between them: each slice reports its own count, and
    /// the caller wants the total.
    fn with_executed(self, executed: u64) -> Self {
        match self {
            RunOutcome::Shutdown { .. } => RunOutcome::Shutdown { executed },
            RunOutcome::Capped { .. } => RunOutcome::Capped { executed },
            RunOutcome::BackingFailure { addr, .. } => RunOutcome::BackingFailure { executed, addr },
            RunOutcome::MachineTrap { mcause, mepc, mtval, .. } => {
                RunOutcome::MachineTrap { executed, mcause, mepc, mtval }
            }
        }
    }

    /// What went wrong, in a form fit to print, or `None` for the one ending
    /// that is not a problem (a clean SBI shutdown).
    ///
    /// Written once here rather than at each call site because the
    /// [`RunOutcome::MachineTrap`] text is the whole point of that variant:
    /// before it existed, an undelegated trap produced *no output at all*
    /// (see [`run_until`]), and a diagnostic that only the CLI printed would
    /// leave the boot test — the one place a new unimplemented instruction is
    /// most likely to be discovered — just as blind as before.
    pub fn diagnostic(&self) -> Option<String> {
        match *self {
            RunOutcome::Shutdown { .. } => None,
            RunOutcome::Capped { executed } => {
                Some(format!("stopped after reaching the {executed}-instruction cap"))
            }
            RunOutcome::BackingFailure { addr, .. } => {
                Some(format!("guest memory backing store failed at {addr:#x}"))
            }
            RunOutcome::MachineTrap { mcause, mepc, mtval, .. } => Some(format!(
                "unhandled M-mode trap: mcause={mcause:#x} ({}), mepc={mepc:#x}, \
                 mtval={mtval:#x}.{} This emulator plays the M-mode firmware role in \
                 host code and installs no trap vector, so `mtvec` is 0 and the guest \
                 would otherwise spin at address 0 forever with no output.",
                mcause_name(mcause),
                if mcause == 2 {
                    " mtval is the offending instruction encoding — decode it with the \
                     Task 16 harness, and mepc is the pc to look up in the kernel's \
                     System.map."
                } else {
                    ""
                },
            )),
        }
    }
}

/// Runs `cpu` against `bus` until the guest issues an SBI shutdown, the
/// backing store fails, or `max_insns` instructions have executed —
/// whichever comes first.
///
/// This is the CLI runner's actual boot loop. It is not duplicated in
/// `main.rs`: a hand-copy of it there would be exercised by nothing (no
/// test invokes the compiled binary), so a regression in it — the interrupt
/// check landing after the step instead of before, say, or a dropped
/// `bus.clint.tick(1)` — could pass every test in the suite while the
/// actual boot path silently stopped ticking. `main.rs` and
/// [`run_program_capturing`] both call this one function instead, so the
/// path that boots a kernel is the path the tests below cover.
///
/// The per-instruction dispatch is `Cpu::step_trapping` itself, not a copy
/// of it: this loop used to hold a hand-reproduction that differed only in
/// reporting `BackingFailure` rather than panicking on it, which left the
/// core crate's own exported entry point dead outside tests while the code
/// that boots a kernel lived in another crate. `step_trapping` now returns
/// that case as `Err(addr)`, so there is one dispatch and it is the
/// exported one.
///
/// # The undelegated-trap dead end
///
/// `Csrs::default` delegates the standard S-mode exception set to the guest,
/// deliberately matching what OpenSBI delegates. But OpenSBI backs that up
/// with an M-mode trap handler that emulates some causes and
/// `sbi_trap_redirect()`s the rest into S-mode; this emulator has neither. So
/// a cause outside the delegated set — cause 2, **illegal instruction**,
/// above all, which is what an unimplemented opcode raises — vectors to
/// `mtvec`, which nothing has ever written and which is therefore 0. The CPU
/// lands at address 0, faults fetching there (0 is not RAM), traps to M-mode
/// again, lands at 0 again, and spins forever.
///
/// Before this loop checked for it, that was **completely silent**: the run
/// burned its whole instruction budget and reported nothing but a count.
/// Task 20 lost most of a day to it, and pinned the state down only by
/// noticing that the page-cache counters were byte-identical between a
/// 600 M- and a 4 000 M-instruction run.
///
/// The check sits at the top of the loop, before anything else runs, so it
/// fires on the *first* arrival at address 0 — while `mcause`/`mepc`/`mtval`
/// still describe the instruction that actually caused it. One more iteration
/// and the re-trap from the fetch at 0 would overwrite them with `mepc = 0`,
/// losing the very pc the reader needs.
///
/// The condition requires `mtvec == 0` rather than merely "we are in M-mode",
/// so what it detects is precisely "a trap was vectored to a handler that
/// does not exist". A future M-mode firmware that installs a real vector goes
/// on working unchanged.
pub fn run_until<B: MemBacking, S: ConsoleSink>(
    cpu: &mut Cpu,
    bus: &mut Bus<B, S>,
    max_insns: u64,
) -> RunOutcome {
    let mut executed = 0u64;
    loop {
        if cpu.priv_ == Priv::M && cpu.pc == 0 && cpu.csrs.read(csr::MTVEC) == 0 {
            return RunOutcome::MachineTrap {
                executed,
                mcause: cpu.csrs.read(csr::MCAUSE),
                mepc: cpu.csrs.read(csr::MEPC),
                mtval: cpu.csrs.read(csr::MTVAL),
            };
        }
        if executed >= max_insns {
            return RunOutcome::Capped { executed };
        }
        // Before the step, never inside it — see `Cpu::check_interrupts`'s
        // doc comment for why.
        cpu.check_interrupts(bus);
        match cpu.step_trapping(bus) {
            // The shutdown-triggering `ecall` itself completed (SBI
            // serviced it, pc advanced past it) and must be counted — an
            // earlier cut of this loop returned before this increment and
            // undercounted every clean shutdown by exactly one.
            Ok(SbiOutcome::Shutdown) => {
                executed += 1;
                return RunOutcome::Shutdown { executed };
            }
            Ok(SbiOutcome::Handled) => executed += 1,
            // A backing failure means the faulting instruction never
            // completed — nothing retired, so it is *not* counted here;
            // `executed` reports how many instructions completed before
            // the run aborted, not an attempt count.
            Err(addr) => return RunOutcome::BackingFailure { executed, addr },
        }
        bus.clint.tick(1);
    }
}

/// Runs a hand-assembled instruction stream starting in S-mode at
/// `RAM_BASE`, exactly as the Linux guest this emulator targets runs under
/// its SBI stub, and returns whatever the program wrote to the console.
///
/// This is `rv64-host`'s own end-to-end test fixture: with no kernel image
/// available yet, it is the only way to exercise the whole runner path —
/// `Cpu` started in S-mode (not the `Priv::M` `Cpu::new` defaults to),
/// `step_trapping`-equivalent SBI interception, the CLINT tick, and a
/// `ConsoleSink` — without a real kernel to boot. It shares [`run_until`]
/// with `main.rs`'s CLI loop verbatim, so this test also covers that loop.
pub fn run_program_capturing(program: &[u32], max_insns: u64) -> String {
    let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
    let mut bus = Bus::new(PageCache::new(FakeBacking::new(pages), 64), VecSink::default());
    for (i, w) in program.iter().enumerate() {
        bus.store(rv64::RAM_BASE + 4 * i as u64, 4, *w as u64).unwrap();
    }

    let mut cpu = Cpu::new(rv64::RAM_BASE);
    // `Cpu::new` starts in M-mode; `Cpu::step_trapping`'s SBI interception
    // only fires on `EnvironmentCallFromSMode`, so a program run at the
    // default privilege would raise `EnvironmentCallFromMMode` instead,
    // which is trapped rather than serviced — no SBI call would ever be
    // intercepted and this would silently hang instead of producing output.
    cpu.priv_ = Priv::S;

    match run_until(&mut cpu, &mut bus, max_insns) {
        RunOutcome::Shutdown { .. } => {}
        RunOutcome::Capped { executed } => {
            panic!("program did not shut down within {executed} instructions")
        }
        // `FakeBacking` never fails, so the backing case is unreachable for
        // this fixture in practice; treated the same as a real failure rather
        // than silently ignored. The machine-trap case is very much
        // reachable — a mistyped instruction word in one of these
        // hand-assembled programs is an illegal instruction, and reporting it
        // as "did not shut down" is what used to make that a puzzle.
        other => panic!(
            "{}",
            other.diagnostic().expect("only Shutdown has no diagnostic, and it matched above")
        ),
    }

    String::from_utf8_lossy(&bus.uart.sink.bytes).into_owned()
}

/// Resident page-cache frames used by the CLI runner and by
/// [`boot_capturing`].
///
/// 256 frames is 1 MiB of resident guest RAM out of 32 MiB — deliberately
/// badge-like, because the whole point of the counters these two paths report
/// is to predict what happens on a device with roughly that much memory to
/// spare. It is also the size every measurement in this project's reports was
/// taken at, so changing it invalidates the comparison rather than merely
/// changing performance.
///
/// This used to read: "Bigger is *slower* here, not faster: `PageCache::resident`
/// linear-scans every frame on every access, so an 8× cache measured 5.7× slower
/// (182 s against 32 s). That is a known, deliberately deferred issue … nobody
/// should optimize this number without fixing the scan first."
///
/// **That was fixed in 67a653f** — `PageCache` now finds a resident frame
/// through a direct-mapped residency index (`crates/rv64/src/cache.rs`), so a
/// lookup is one array read regardless of frame count and the scan the warning
/// guarded no longer exists. The constant stays at 256 for the first reason
/// above (comparability), not the second.
pub const DEFAULT_FRAMES: usize = 256;

/// Which of the boot images a [`load_boot_images`] failure is about.
///
/// The variants exist so the caller — which is the only party that knows
/// where the bytes came from — can name the file, without this function
/// having to take three paths it would otherwise never use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootError {
    /// Reading one of the images off disk failed.
    Io(String),
    /// The kernel image could not be loaded (bad ELF, wrong `text_offset`,
    /// too large for guest RAM).
    Kernel(String),
    /// The three images do not fit in guest RAM together.
    Layout(String),
    /// The device tree could not be patched or placed.
    Dtb(String),
    /// The initrd could not be placed.
    Initrd(String),
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootError::Io(m) => write!(f, "reading a boot image: {m}"),
            BootError::Kernel(m) => write!(f, "loading the kernel image: {m}"),
            BootError::Layout(m) => write!(f, "{m}"),
            BootError::Dtb(m) => write!(f, "the device tree blob: {m}"),
            BootError::Initrd(m) => write!(f, "the initrd: {m}"),
        }
    }
}

impl std::error::Error for BootError {}

/// Loads a kernel, a device tree and an optional initrd into `bus` and
/// returns a `Cpu` positioned exactly where the riscv64 boot protocol says
/// the kernel expects to start: at [`KERNEL_LOAD_ADDR`], in S-mode, with
/// `a0 = 0` (hartid) and `a1` pointing at the DTB.
///
/// This is the *one* implementation of that sequence. `main.rs`'s CLI and
/// [`boot_capturing`]'s in-process test harness both call it, for the same
/// reason [`run_until`] is not duplicated in `main.rs`: the placement rules
/// it encodes (the DTB above the kernel's declared *memory* footprint rather
/// than its file length; the initrd on a page of its own above that; the
/// `/chosen` patch applied to the bytes that are actually written) each fail
/// as a silent hang or a `No working init found` far from their cause, so a
/// second copy that drifted would be discovered the hard way. See
/// [`load_kernel`] and [`boot_layout`] for the reasoning behind each rule.
pub fn load_boot_images<B: MemBacking, S: ConsoleSink>(
    bus: &mut Bus<B, S>,
    kernel: &[u8],
    dtb: &[u8],
    initrd: Option<&[u8]>,
) -> Result<(Cpu, BootLayout), BootError> {
    let kernel_end = load_kernel(bus, kernel).map_err(BootError::Kernel)?;
    let layout =
        boot_layout(kernel_end, dtb.len(), initrd.map(<[u8]>::len)).map_err(BootError::Layout)?;

    // Patched before it is written, not after: the guest must see the
    // addresses the initrd was actually placed at, and the DTB goes into
    // guest memory exactly once.
    let mut dtb = dtb.to_vec();
    if let Some((start, end)) = layout.initrd {
        fdt::set_u64(&mut dtb, "/chosen", "linux,initrd-start", start)
            .and_then(|()| fdt::set_u64(&mut dtb, "/chosen", "linux,initrd-end", end))
            .map_err(|m| BootError::Dtb(format!("cannot record the initrd in it: {m}")))?;
    }

    write_blob(bus, layout.dtb, &dtb)
        .map_err(|e| BootError::Dtb(format!("cannot be placed at {:#x}: {e:?}", layout.dtb)))?;
    if let (Some(bytes), Some((start, _))) = (initrd, layout.initrd) {
        write_blob(bus, start, bytes)
            .map_err(|e| BootError::Initrd(format!("cannot be placed at {start:#x}: {e:?}")))?;
    }

    let mut cpu = Cpu::new(KERNEL_LOAD_ADDR);
    // `Cpu::new` starts in M-mode. The SBI console/shutdown calls this guest
    // issues are only intercepted from `EnvironmentCallFromSMode` (see
    // `run_until` / `Cpu::step_trapping`); left in M-mode, `ecall` would raise
    // `EnvironmentCallFromMMode` instead, which is not intercepted, and the
    // run would produce no output and never see a shutdown request.
    cpu.priv_ = Priv::S;
    cpu.set_reg(10, 0); // a0 = hartid 0
    cpu.set_reg(11, layout.dtb); // a1 = dtb guest physical address
    Ok((cpu, layout))
}

/// Everything one [`boot_capturing`] run produced: what the guest said, and
/// what it cost.
///
/// The counters travel with the output because they are the numbers that
/// decide whether this stack is viable on the badge, and a caller that had to
/// boot a second time to collect them would be measuring a second boot.
#[derive(Debug, Clone)]
pub struct BootRun {
    /// Everything the guest wrote to the console, in order.
    pub output: String,
    /// Instructions retired when the run stopped.
    pub executed: u64,
    /// True when the run was stopped deliberately because `stop_marker`
    /// appeared on the console — the success case, and the one in which
    /// `executed` is the cost of reaching that marker rather than an
    /// artifact of the instruction cap.
    pub reached_marker: bool,
    /// How the run ended when the marker never appeared; `None` when it did.
    /// Its [`RunOutcome::diagnostic`] is what to print on failure.
    pub ending: Option<RunOutcome>,
    /// Page-cache counters over the whole run, at [`DEFAULT_FRAMES`] frames.
    pub cache: Stats,
    /// Guest page-table walks performed.
    pub mmu_walks: u64,
}

/// How often [`boot_capturing`] pauses to look at the console.
///
/// 100 000 instructions is under a hundredth of a second at the measured
/// throughput, so the reported `executed` overshoots the true first
/// appearance of the marker by at most that — about one part in ten thousand
/// of a boot. Making it exact would mean pushing the predicate down into the
/// step loop, which would put a string search on the path of every retired
/// instruction to sharpen a number that is already far more precise than the
/// 2 MIPS estimate it feeds.
const CONSOLE_POLL_INSNS: u64 = 100_000;

/// Whether `marker` has appeared anywhere in `out`, resuming from `*scanned`
/// and leaving `*scanned` ready for the next call.
///
/// Rescanning the whole buffer on every poll would be quadratic in the output
/// length for no benefit, but the cursor cannot simply advance to the end:
/// the marker can straddle two polls, with `~ ` arriving in one slice and `#`
/// in the next. So the cursor is left `marker.len() - 1` bytes short of the
/// end, which is exactly the longest prefix that could still be completed.
/// Getting this wrong would not fail loudly — the boot would just run to its
/// cap as if the prompt had never appeared — which is why it is a function
/// with its own tests rather than three lines inline.
fn console_reached(out: &[u8], marker: &[u8], scanned: &mut usize) -> bool {
    if marker.is_empty() {
        return false;
    }
    let found = out[*scanned..].windows(marker.len()).any(|w| w == marker);
    *scanned = out.len().saturating_sub(marker.len() - 1);
    found
}

/// Boots the real guest images in-process and returns what the guest printed
/// along with what it cost.
///
/// This is a wrapper, not a second runner: it reads three files, builds a
/// `Bus` over an in-memory backing with a [`DEFAULT_FRAMES`]-frame
/// [`PageCache`], and then hands off to [`load_boot_images`] and
/// [`run_until`] — the same two functions the `rv64-host` CLI boots a kernel
/// with. The only logic of its own is the slicing that lets it watch the
/// console.
///
/// **`stop_marker` is why the slicing exists.** The guest's shell blocks
/// reading `/dev/console` once it starts, and there is no host stdin plumbing
/// (`main.rs`'s standing TODO), so a boot never terminates on its own — it
/// runs until the cap. Stopping at the first sight of the marker turns
/// `executed` into "what it cost to get here", which is the number the badge
/// decision rests on, and lets `max_insns` be a generous safety valve rather
/// than a dial that also sets the runtime.
///
/// Two deliberate departures from the task brief's proposed signature:
/// `stop_marker` for the reason above, and a struct return rather than a bare
/// `String`, so the page-cache and MMU counters come from the same boot as
/// the output instead of a second one.
///
/// The backing store is [`FakeBacking`] rather than a file: the cache
/// counters this returns are decided entirely by the access pattern and the
/// frame count, not by what sits behind the cache, so a temporary file would
/// add I/O to the measurement without changing any number in it.
pub fn boot_capturing(
    kernel: &Path,
    dtb: &Path,
    initrd: &Path,
    max_insns: u64,
    stop_marker: &str,
) -> Result<BootRun, BootError> {
    boot_capturing_frames(kernel, dtb, initrd, max_insns, stop_marker, DEFAULT_FRAMES)
}

/// [`boot_capturing`] with the page-cache frame count spelled out.
///
/// The frame count is the one input that moves the cache counters without
/// moving a single retired instruction, so a caller that compares its own
/// counters against this reference has to boot it at the same size or the
/// comparison measures the frame count instead of the guest. `badge/app`'s dry
/// run is that caller: it reconciles the badge's misses against this run's, and
/// the badge's cache is whatever `run::FRAMES` currently says.
pub fn boot_capturing_frames(
    kernel: &Path,
    dtb: &Path,
    initrd: &Path,
    max_insns: u64,
    stop_marker: &str,
    frames: usize,
) -> Result<BootRun, BootError> {
    let read = |p: &Path| {
        std::fs::read(p).map_err(|e| BootError::Io(format!("{}: {e}", p.display())))
    };
    let (kernel, dtb, initrd) = (read(kernel)?, read(dtb)?, read(initrd)?);

    let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
    let mut bus = Bus::new(PageCache::new(FakeBacking::new(pages), frames), VecSink::default());
    let (mut cpu, _) = load_boot_images(&mut bus, &kernel, &dtb, Some(&initrd))?;

    let marker = stop_marker.as_bytes();
    let mut executed = 0u64;
    // Where the next console scan resumes; `console_reached` maintains it.
    let mut scanned = 0usize;
    let ending = loop {
        if executed >= max_insns {
            break Some(RunOutcome::Capped { executed });
        }
        let slice = run_until(&mut cpu, &mut bus, CONSOLE_POLL_INSNS.min(max_insns - executed));
        executed += slice.executed();
        if !matches!(slice, RunOutcome::Capped { .. }) {
            break Some(slice.with_executed(executed));
        }

        if console_reached(&bus.uart.sink.bytes, marker, &mut scanned) {
            break None;
        }
    };

    let stats = bus.cache_mut().stats();
    Ok(BootRun {
        output: String::from_utf8_lossy(&bus.uart.sink.bytes).into_owned(),
        executed,
        reached_marker: ending.is_none(),
        ending,
        cache: stats,
        mmu_walks: cpu.mmu.walks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rv64::backing::Error as BackingError;

    /// The same 8-instruction program `tests/runner.rs` drives through
    /// `run_program_capturing`, duplicated here rather than shared: these
    /// tests need `run_until`'s structured `RunOutcome`, which that
    /// black-box integration test deliberately never sees.
    const HI_AND_SHUTDOWN: [u32; 8] = [
        0x00100893, // li a7, 1        (console_putchar)
        0x06800513, // li a0, 'h'
        0x00000073, // ecall
        0x00100893, // li a7, 1
        0x06900513, // li a0, 'i'
        0x00000073, // ecall
        0x00800893, // li a7, 8        (shutdown)
        0x00000073, // ecall
    ];

    fn setup() -> (Cpu, Bus<FakeBacking, VecSink>) {
        let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(pages), 64), VecSink::default());
        for (i, w) in HI_AND_SHUTDOWN.iter().enumerate() {
            bus.store(rv64::RAM_BASE + 4 * i as u64, 4, *w as u64).unwrap();
        }
        let mut cpu = Cpu::new(rv64::RAM_BASE);
        cpu.priv_ = Priv::S;
        (cpu, bus)
    }

    #[test]
    fn run_until_reports_shutdown_and_the_exact_instruction_count() {
        let (mut cpu, mut bus) = setup();
        let outcome = run_until(&mut cpu, &mut bus, 1000);
        assert_eq!(
            outcome,
            RunOutcome::Shutdown { executed: 8 },
            "the shutdown ecall itself must be counted, not just the instructions before it"
        );
    }

    #[test]
    fn run_until_stops_at_the_instruction_cap_without_finishing() {
        let (mut cpu, mut bus) = setup();
        // The 3rd instruction is the first `ecall` (`li a7,1; li a0,'h';
        // ecall`), so it runs and writes 'h' — but the shutdown `ecall`
        // five instructions later never does.
        let outcome = run_until(&mut cpu, &mut bus, 3);
        assert_eq!(outcome, RunOutcome::Capped { executed: 3 });
        assert_eq!(bus.uart.sink.bytes, b"h", "the first ecall ran; the program never shut down");
    }

    struct FailingBacking;
    impl MemBacking for FailingBacking {
        fn read_page(&mut self, _p: u32, _b: &mut [u8; rv64::PAGE]) -> Result<(), BackingError> {
            Err(BackingError::Medium)
        }
        fn write_page(&mut self, _p: u32, _b: &[u8; rv64::PAGE]) -> Result<(), BackingError> {
            Err(BackingError::Medium)
        }
        fn flush(&mut self) -> Result<(), BackingError> {
            Ok(())
        }
    }

    /// End-to-end version of the hazard noted on `Cpu::step_trapping`: a
    /// backing failure must surface as `RunOutcome::BackingFailure`, and leave
    /// the CPU untouched — `trap` was never called, so `priv_` cannot have
    /// changed, which is exactly what would happen if cause 5 had instead
    /// been silently delegated to the guest as a load access fault.
    #[test]
    fn run_until_reports_backing_failure_instead_of_delegating_it_as_a_guest_trap() {
        let mut bus = Bus::new(PageCache::new(FailingBacking, 4), VecSink::default());
        let mut cpu = Cpu::new(rv64::RAM_BASE);
        cpu.priv_ = Priv::S;

        let outcome = run_until(&mut cpu, &mut bus, 10);
        assert!(
            matches!(outcome, RunOutcome::BackingFailure { executed: 0, .. }),
            "expected an immediate BackingFailure, got {outcome:?}"
        );
        assert_eq!(
            cpu.priv_,
            Priv::S,
            "a backing failure must not be delivered to the guest as a trap"
        );
    }

    /// `riscv_test.h`'s `other_exception` path stores TESTNUM *unshifted*
    /// (`ori TESTNUM, TESTNUM, 1337`, then straight into `write_tohost`),
    /// unlike `RVTEST_FAIL`, which shifts. Treating the two the same halved
    /// every reported sub-test number: for TESTNUM 4 the old
    /// `(v >> 1) & !(1337 >> 1)` reported 2.
    #[test]
    fn the_unexpected_trap_subtest_number_is_not_shifted() {
        // TESTNUMs chosen to share no bits with the 1337 marker, since an
        // `ori` cannot preserve a bit the marker already sets.
        assert_eq!(unexpected_trap_subtest(4 | UNEXPECTED_TRAP), 4);
        assert_eq!(unexpected_trap_subtest(64 | UNEXPECTED_TRAP), 64);
        assert_eq!(unexpected_trap_subtest(518 | UNEXPECTED_TRAP), 518);
    }

    /// Without the environment gate, a missing prerequisite is a silent
    /// green test — the exact state all three integration suites were in.
    #[test]
    fn a_missing_prerequisite_is_only_a_skip_when_the_suite_is_not_required() {
        report_missing_prerequisite(false, "not_a_real_suite (self-test)", "a tool is missing");
    }

    #[test]
    #[should_panic(expected = "required to run")]
    fn a_missing_prerequisite_is_a_failure_when_the_suite_is_required() {
        report_missing_prerequisite(true, "not_a_real_suite (self-test)", "a tool is missing");
    }

    /// A raw riscv64 `Image` (what Task 19's flake output produces, and
    /// what `arch/riscv/boot/Makefile` actually emits) has no ELF header at
    /// all. Before this, `--kernel` accepted ELF only, so the runner could
    /// not load the one artifact the guest build was specified to produce.
    #[test]
    fn load_kernel_places_a_raw_image_at_the_boot_protocols_load_address() {
        let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(pages), 64), VecSink::default());
        // Deliberately not ELF, and deliberately not a multiple of 8 long.
        let image: Vec<u8> = (0..21u8).map(|i| i.wrapping_mul(7)).collect();

        let end = load_kernel(&mut bus, &image).expect("a raw Image must load");

        assert_eq!(end, KERNEL_LOAD_ADDR + image.len() as u64);
        for (i, b) in image.iter().enumerate() {
            assert_eq!(
                bus.load(KERNEL_LOAD_ADDR + i as u64, 1).unwrap(),
                *b as u64,
                "byte {i} of the raw image"
            );
        }
    }

    /// A synthetic raw riscv64 `Image`: a valid boot header (the layout in
    /// `arch/riscv/include/asm/image.h`) followed by `body` bytes of
    /// payload, declaring a memory footprint `bss` bytes larger than the
    /// file — exactly the shape a real `Image` has, since `objcopy -O
    /// binary` drops `.bss` from the file but `image_size` counts it.
    fn riscv_image(body: usize, bss: u64) -> Vec<u8> {
        let mut v = vec![0u8; RISCV_IMAGE_HEADER_LEN + body];
        // `c.li s4,-13` (the ASCII "MZ") then `j _start_kernel`, as head.S
        // emits under CONFIG_EFI. Only decoration here, but it keeps the
        // fixture honest about what byte 0 of a real Image looks like.
        v[0..4].copy_from_slice(&0x106f_5a4du32.to_le_bytes());
        v[8..16].copy_from_slice(&0x20_0000u64.to_le_bytes()); // text_offset
        v[16..24].copy_from_slice(&((RISCV_IMAGE_HEADER_LEN + body) as u64 + bss).to_le_bytes());
        v[32..36].copy_from_slice(&2u32.to_le_bytes()); // version 0.2
        v[48..56].copy_from_slice(b"RISCV\0\0\0");
        v[56..60].copy_from_slice(RISCV_IMAGE_MAGIC2);
        v[RISCV_IMAGE_HEADER_LEN..].fill(0xAB);
        v
    }

    /// The regression this pins is the one that silently destroys a boot.
    ///
    /// A riscv `Image` contains no `.bss` — `objcopy -O binary` drops NOBITS
    /// sections — so the file is *shorter* than the memory the kernel
    /// occupies. Returning `load_addr + file_length` therefore points at an
    /// address the kernel is about to zero in `clear_bss`, and the runner
    /// puts the DTB (and, now, the initrd) exactly there. For the real Task
    /// 19 kernel the gap is 313,344 bytes, which is the number used here.
    #[test]
    fn load_kernel_reports_the_headers_declared_footprint_not_the_file_length() {
        let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(pages), 64), VecSink::default());
        let bss = 313_344u64;
        let image = riscv_image(4096, bss);

        let end = load_kernel(&mut bus, &image).expect("a raw Image must load");

        assert_eq!(end, KERNEL_LOAD_ADDR + image.len() as u64 + bss);
        assert_ne!(
            end,
            KERNEL_LOAD_ADDR + image.len() as u64,
            "the file length is not the kernel's footprint; using it puts the \
             DTB inside .bss"
        );
    }

    /// The end-to-end version of the bug, stated in the terms the guest
    /// actually fails in: with the real kernel's numbers, the DTB must not
    /// land anywhere the kernel is going to zero.
    #[test]
    fn the_dtb_is_placed_clear_of_the_kernels_bss() {
        // The Task 19 guest kernel, measured: a 2,811,904-byte Image
        // declaring a 3,125,248-byte footprint.
        let file_len = 2_811_904u64;
        let footprint = 3_125_248u64;
        let bss_start = KERNEL_LOAD_ADDR + file_len;
        let bss_end = KERNEL_LOAD_ADDR + footprint;

        let layout = boot_layout(bss_end, 1247, None).unwrap();

        assert!(
            layout.dtb >= bss_end,
            "the DTB at {:#x} is inside .bss ({bss_start:#x}..{bss_end:#x}), which \
             clear_bss zeroes before setup_vm parses it",
            layout.dtb
        );
    }

    /// `reserve_initrd_mem()` rounds the initrd down to a page boundary
    /// before asking `memblock_is_region_reserved()`, and the kernel image
    /// is already reserved when it does. An initrd sharing the kernel's
    /// last page is not slightly wrong: the kernel prints one line and
    /// disables it, and the boot ends at `No working init found`. (The DTB
    /// is *not* reserved at that point on riscv — see `boot_layout`'s doc
    /// comment — so this pins the alignment, not a claim about the DTB.)
    #[test]
    fn the_initrd_starts_on_a_page_of_its_own() {
        let page = rv64::PAGE as u64;
        // A DTB length that lands mid-page, so "just after the DTB" and
        // "the next page" are provably different answers.
        let layout = boot_layout(KERNEL_LOAD_ADDR + 3_125_248, 1247, Some(64 * 1024)).unwrap();
        let (start, end) = layout.initrd.unwrap();

        assert_eq!(start % page, 0, "initrd at {start:#x} is not page-aligned");
        assert!(
            start >= (layout.dtb + 1247).next_multiple_of(page),
            "initrd at {start:#x} shares a page with the DTB at {:#x}",
            layout.dtb
        );
        assert_eq!(end - start, 64 * 1024);
    }

    /// Truncating here would surface later as a corrupt cpio ("Invalid
    /// magic at start of compressed archive"), which points the reader at
    /// the initramfs build rather than at the memory budget.
    #[test]
    fn boot_layout_refuses_to_run_past_the_top_of_ram() {
        let err = boot_layout(KERNEL_LOAD_ADDR + 3_125_248, 1247, Some(rv64::RAM_SIZE as usize))
            .unwrap_err();
        assert!(err.contains("past the top of RAM"), "unhelpful message: {err}");
    }

    /// A blob with no riscv header has no declared footprint, so the file
    /// length is all there is. This keeps the short synthetic images the
    /// other tests use loadable — and pins that the fallback is reached by
    /// a missing *magic*, not by accident.
    #[test]
    fn a_blob_without_the_riscv_header_has_no_declared_footprint() {
        assert_eq!(riscv_image_footprint(b"far too short"), None);
        let mut img = riscv_image(64, 4096);
        assert!(riscv_image_footprint(&img).is_some());
        img[56] = b'X'; // corrupt RISCV_IMAGE_MAGIC2
        assert_eq!(riscv_image_footprint(&img), None);
    }

    /// A header claiming to occupy less memory than the file takes on disk
    /// is corrupt. Believing it would put the DTB *inside* the loaded
    /// image, which is worse than the bug being fixed here.
    #[test]
    fn a_footprint_smaller_than_the_file_is_rejected_as_corrupt() {
        let mut img = riscv_image(4096, 0);
        img[16..24].copy_from_slice(&16u64.to_le_bytes());
        assert_eq!(riscv_image_footprint(&img), None);
    }

    /// `text_offset` is the kernel saying where in RAM it was linked to
    /// run. This runner implements exactly one answer, so a mismatch has to
    /// be refused: loading it at 0x8020_0000 regardless would put the
    /// kernel at an address it was not linked for, and that fails as a hang
    /// with no console output at all — the symptom this whole task exists
    /// to avoid.
    #[test]
    fn a_kernel_linked_for_a_different_text_offset_is_refused() {
        let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(pages), 64), VecSink::default());
        let mut image = riscv_image(4096, 0);
        image[8..16].copy_from_slice(&0x40_0000u64.to_le_bytes());

        let err = load_kernel(&mut bus, &image).unwrap_err();

        assert!(err.contains("text_offset"), "unhelpful message: {err}");
        assert!(err.contains("0x400000"), "the message must name the offset: {err}");
    }

    /// And the offset the guest kernel actually declares must be accepted,
    /// so the check cannot be satisfied by rejecting everything.
    #[test]
    fn the_load_addresss_own_text_offset_is_accepted() {
        assert_eq!(
            riscv_image_text_offset(&riscv_image(64, 0)),
            Some(KERNEL_LOAD_ADDR - rv64::RAM_BASE)
        );
    }

    /// `riscv_image_footprint` returns `None` for two unrelated reasons,
    /// and the CLI prints a different diagnostic for each. This is what
    /// lets it tell them apart.
    #[test]
    fn a_corrupt_footprint_is_distinguishable_from_a_missing_header() {
        let mut corrupt = riscv_image(4096, 0);
        corrupt[16..24].copy_from_slice(&16u64.to_le_bytes());
        assert_eq!(riscv_image_footprint(&corrupt), None);
        assert!(has_riscv_image_header(&corrupt), "the header is present, just wrong");

        assert!(!has_riscv_image_header(b"far too short"));
        let mut headerless = riscv_image(64, 0);
        headerless[56] = b'X';
        assert!(!has_riscv_image_header(&headerless));
    }

    /// A crafted `image_size` large enough that `KERNEL_LOAD_ADDR + span`
    /// overflows `u64` must be reported as a corrupt header, not panic a
    /// debug build ("attempt to add with overflow") or silently wrap in
    /// release and return a bogus, too-small end address.
    #[test]
    fn an_image_size_that_would_overflow_the_load_address_is_rejected() {
        let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(pages), 64), VecSink::default());
        // `bss` chosen so the declared `image_size` lands at exactly
        // `u64::MAX` — large enough that `KERNEL_LOAD_ADDR + span` must
        // overflow, without overflowing the fixture's own arithmetic first.
        let image = riscv_image(64, u64::MAX - (RISCV_IMAGE_HEADER_LEN + 64) as u64);

        let err = load_kernel(&mut bus, &image).unwrap_err();

        assert!(err.contains("overflow"), "unhelpful message: {err}");
        assert!(err.contains("corrupt boot header"), "unhelpful message: {err}");
    }

    /// The ELF path must keep working, and must still be chosen by the
    /// magic rather than by a flag: `\x7fELF` -> `elf::load` at the
    /// segment's own physical addresses.
    #[test]
    fn load_kernel_still_takes_the_elf_path_when_the_magic_says_elf() {
        let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(pages), 64), VecSink::default());
        let obj = elf::tests::tiny_elf(&[0xAA, 0xBB, 0xCC, 0xDD], 32);

        let end = load_kernel(&mut bus, &obj).expect("an ELF must still load");

        assert_eq!(end, rv64::RAM_BASE + 32, "the ELF's own extent, .bss tail included");
        assert_eq!(bus.load(rv64::RAM_BASE, 1).unwrap(), 0xAA);
    }

    /// A file that begins with the ELF magic but is not a loadable riscv64
    /// object must be reported as a broken ELF, not silently written into
    /// guest memory as if it were a raw `Image`.
    #[test]
    fn load_kernel_reports_a_broken_elf_rather_than_treating_it_as_a_raw_image() {
        let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(pages), 64), VecSink::default());
        assert!(load_kernel(&mut bus, b"\x7fELFnope").is_err());
    }

    /// The diagnosability gap Task 20 reported (§8.1) and Task 21 closed.
    ///
    /// `medeleg` does not delegate cause 2, matching OpenSBI — but unlike
    /// OpenSBI this emulator has no M-mode trap handler, so an illegal
    /// instruction used to vector to `mtvec` (0), fault fetching there, and
    /// spin at address 0 for the entire instruction budget while printing
    /// nothing whatsoever. Every symptom pointed somewhere else.
    ///
    /// What this pins is not just "the run stops" but that it stops carrying
    /// the two values that identify the instruction: `mepc`, the pc, and
    /// `mtval`, the encoding. Reporting the wedge a moment later — after the
    /// fetch at 0 re-traps — would overwrite both with 0 and give the reader
    /// nothing.
    #[test]
    fn an_illegal_instruction_is_reported_with_its_pc_and_encoding_not_silently_wedged() {
        // opcode 0b1111111: reserved, and not something a future extension
        // can quietly make legal.
        const ILLEGAL: u32 = 0x0000_007F;
        let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(pages), 64), VecSink::default());
        bus.store(rv64::RAM_BASE, 4, ILLEGAL as u64).unwrap();
        let mut cpu = Cpu::new(rv64::RAM_BASE);
        cpu.priv_ = Priv::S;

        let outcome = run_until(&mut cpu, &mut bus, 1_000_000);

        assert_eq!(
            outcome,
            RunOutcome::MachineTrap {
                // The illegal instruction itself does not retire, but the
                // trap it caused is what `step_trapping` reports as handled,
                // so one step has been taken by the time the CPU is at 0.
                executed: 1,
                mcause: 2,
                mepc: rv64::RAM_BASE,
                mtval: ILLEGAL as u64,
            },
            "an undelegated trap must be reported with the pc and encoding that \
             caused it, not run to the instruction cap in silence"
        );
        let msg = outcome.diagnostic().expect("a machine trap is not a clean ending");
        assert!(msg.contains("illegal instruction"), "unhelpful message: {msg}");
        assert!(msg.contains("0x8000000"), "the message must name the pc: {msg}");
        assert!(msg.contains("0x7f"), "the message must name the encoding: {msg}");
    }

    /// The detection keys on `mtvec == 0` — "vectored to a handler that does
    /// not exist" — rather than on being in M-mode at all, so firmware that
    /// installs a real vector is not cut short at its first trap. Nothing
    /// installs one today; this is what lets the badge port do so later
    /// without having to rediscover why the runner kept stopping.
    #[test]
    fn a_machine_trap_with_a_real_handler_installed_is_not_treated_as_a_wedge() {
        const ILLEGAL: u32 = 0x0000_007F;
        let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(pages), 64), VecSink::default());
        bus.store(rv64::RAM_BASE, 4, ILLEGAL as u64).unwrap();
        // A handler that immediately shuts down, so the run ends in a way
        // that could not be confused with the wedge report.
        let handler = rv64::RAM_BASE + 0x1000;
        for (i, w) in [0x00800893u32, 0x00000073].iter().enumerate() {
            bus.store(handler + 4 * i as u64, 4, *w as u64).unwrap();
        }
        let mut cpu = Cpu::new(rv64::RAM_BASE);
        cpu.priv_ = Priv::S;
        cpu.csrs.write(csr::MTVEC, handler);

        // The shutdown `ecall` is taken from M-mode, which `step_trapping`
        // does not intercept, so it traps back to the handler rather than
        // shutting down — the point here is only that the run was *not* cut
        // short with a `MachineTrap` report.
        let outcome = run_until(&mut cpu, &mut bus, 50);
        assert!(
            matches!(outcome, RunOutcome::Capped { .. }),
            "a trap into an installed M-mode handler is not a wedge, got {outcome:?}"
        );
    }

    /// The marker can arrive split across two polls. Losing that case would
    /// not fail loudly — the boot would simply run to its instruction cap as
    /// though the prompt had never appeared — so it is pinned here.
    #[test]
    fn the_console_scan_finds_a_marker_split_across_two_polls() {
        let mut scanned = 0usize;
        assert!(!console_reached(b"booting...\n~ ", b"~ #", &mut scanned));
        assert!(
            console_reached(b"booting...\n~ #", b"~ #", &mut scanned),
            "the marker straddles the poll boundary; the cursor must not skip past it"
        );
    }

    /// And the cursor must actually advance, or the scan is quadratic in the
    /// output length over a boot that emits several kilobytes across ten
    /// thousand polls.
    #[test]
    fn the_console_scan_advances_its_cursor_past_what_it_has_read() {
        let mut scanned = 0usize;
        assert!(!console_reached(b"0123456789", b"~ #", &mut scanned));
        assert_eq!(scanned, 8, "10 bytes read, less the 2 that could start a 3-byte marker");
    }

    /// `write_blob` moved here from `main.rs` unmoved in behavior — this
    /// pins the odd-length, unaligned-start case (the DTB's actual guest
    /// address is only 8-byte aligned, not necessarily word-count-aligned
    /// in length) so the 8-bytes-then-1-byte-at-a-time split is covered.
    #[test]
    fn write_blob_places_bytes_exactly_including_an_unaligned_tail() {
        let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(pages), 16), VecSink::default());
        let data: Vec<u8> = (0..11u8).collect(); // 8 bytes + 3-byte tail
        write_blob(&mut bus, rv64::RAM_BASE, &data).unwrap();
        assert_eq!(bus.load(rv64::RAM_BASE, 8).unwrap(), 0x0706050403020100);
        assert_eq!(bus.load(rv64::RAM_BASE + 8, 1).unwrap(), 8);
        assert_eq!(bus.load(rv64::RAM_BASE + 9, 1).unwrap(), 9);
        assert_eq!(bus.load(rv64::RAM_BASE + 10, 1).unwrap(), 10);
    }
}
