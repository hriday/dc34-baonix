# Xous / Baochip API reference notes

Everything below is copied verbatim from source. Anything I could not find is listed in
**"What I could NOT find"** at the end. Nothing here is reconstructed from memory.

## Clone provenance

| repo | ref used | date | notes |
|---|---|---|---|
| `https://github.com/betrusted-io/xous-core` | **`9844906ddc1214438d0d942d2db2922846ae4722`** on branch **`dev`** (the repo's default branch — `origin/HEAD -> origin/dev`) | 2026-08-23 | Cloned to `/tmp/xous-scratch/xous-core`. `origin/main` is far behind at `5397e1b488c081566cef2c0e597e05426f67c1c3` (2026-02-16). **Use `dev`.** |
| `https://github.com/bunnie/dabao-console` | `090dbaac92bef56aa57a779cdd4bb850f82d41fd` | 2026-06-02 | out-of-tree dabao example |
| `https://github.com/bunnie/dc34-console` | `cf5b090acbe13ebb4e2189e740bd8bc0f965ea7b` | 2026-08-19 | **the DEF CON 34 badge app, out-of-tree form.** README says: *"NOTICE: this repo is now orphaned and will be archived soon, as it is now merged into xous-core."* |
| `https://github.com/bunnie/xous-core` | `main` = `fbf90efe0e9c983cf18853c4da156c2ee1c0d584` | **2022-03-18** | dead fork, **7899 commits behind** `betrusted-io/xous-core@dev`. |

### Re: "is there a bunnie/xous-core branch closer to the DC34 badge?"

**No.** I enumerated all 34 branches of `bunnie/xous-core`: `add-xous-ipc, aes, ancient-dabao,
backlight-on-keypress, braille, bunnie-dev, cb-test, check_buffer, cosine_test, dabao-tester, debugger,
dither2, exception-handler, fcc_test_branch, feat/usb-mass-storage, ffi, fix-section-loading, i18n-jp,
irc-client, loader-monorepo, main, make-monorepo, message-reordering, net-dbg, pid-support, png-iter,
renode-pairs, sign-image, signal-xous, syscall-ptr, tts, usbdev, weird-bug`. None mention dc34, baosec,
or bao1x. `bunnie/xous-core@main` has no `bao1x`, no `baosec`, no `services/dc34-console`, no
`libs/bao1x-api`. It is a 2022-era Precursor snapshot.

You were right that there is no `board-dc34`. The badge is exactly `board-baosec` + `oem-baosec-lite`.
The canonical badge source is **in-tree at `betrusted-io/xous-core@dev:services/dc34-console/`**, and
`libs/dc34-api/` alongside it. Related: `apps-baosec/dc34-vault`. Image uploader:
`https://github.com/bunnie/dc34-image` (a pipx-installed Python tool that pushes a 128x128 B&W PNG over
the badge's USB CDC serial port).

---

## 1. Minimal detached-app skeleton

There are three references. I give all three because they differ in ways that matter.

### 1a. `bunnie/dc34-console` — the badge app, out-of-tree (closest to what you want)

Note this repo has **no `build.rs`** (unlike dabao-console) and it uses a `[patch]` table that assumes a
**sibling `../xous-core` checkout on disk**. That is a surprise: despite pinning git revs, it will not
build standalone without `../xous-core` and `../dc34-api` next to it.

`Cargo.toml` (verbatim, `/tmp/xous-scratch/dc34-console/Cargo.toml`):

```toml
[package]
name = "dc34-console"
version = "0.1.0"
edition = "2021"

[dependencies]
utralib = { version = "0.1.27", optional = true, default-features = false }
xous-names = { package = "xous-api-names", version = "0.9.71" }
ticktimer = { package = "xous-api-ticktimer", version = "0.9.70" }
xous = "0.9.70"
log-server = { package = "xous-api-log", version = "0.1.69" }
log = "0.4.14"
num-derive = { version = "0.4.2", default-features = false }
num-traits = { version = "0.2.14", default-features = false }
bao1x-hal = { features = [
    "std",
], optional = true, git = "https://github.com/betrusted-io/xous-core", rev = "616bf65f6e379165464f50b1e79ec42aff77a683" }
bao1x-api = { git = "https://github.com/betrusted-io/xous-core", rev = "616bf65f6e379165464f50b1e79ec42aff77a683" }
bao1x-hal-service = { optional = true, git = "https://github.com/betrusted-io/xous-core", rev = "616bf65f6e379165464f50b1e79ec42aff77a683" }
bao1x-emu = { optional = true, git = "https://github.com/betrusted-io/xous-core", rev = "616bf65f6e379165464f50b1e79ec42aff77a683" }
usb-bao1x = { git = "https://github.com/betrusted-io/xous-core", rev = "616bf65f6e379165464f50b1e79ec42aff77a683", features = ["bao1x", "board-baosec", "oem-baosec-lite"] }
modals = { default-features = false, optional = true, git = "https://github.com/betrusted-io/xous-core", rev = "616bf65f6e379165464f50b1e79ec42aff77a683" }
ux-api = { optional = true, git = "https://github.com/betrusted-io/xous-core", rev = "616bf65f6e379165464f50b1e79ec42aff77a683" }
pddb = { optional = true, git = "https://github.com/betrusted-io/xous-core", rev = "616bf65f6e379165464f50b1e79ec42aff77a683" }
xous-swapper = { optional = true, git = "https://github.com/betrusted-io/xous-core", rev = "616bf65f6e379165464f50b1e79ec42aff77a683" }
bytemuck = { version = "1.24.0", features = ["derive"] }
susres = { package = "xous-api-susres", git = "https://github.com/betrusted-io/xous-core", rev = "616bf65f6e379165464f50b1e79ec42aff77a683" }
keystore = { git = "https://github.com/betrusted-io/xous-core", rev = "616bf65f6e379165464f50b1e79ec42aff77a683" }
rkyv = { version = "0.8.8", default-features = false, features = [
    "std",
    "alloc",
] }
dc34-api = { path = "../dc34-api/" }
base64 = { version = "0.22.1", default-features = false, features = ["alloc"] }
crc32fast = "1.5.0"

bio-lib = { optional = true, git = "https://github.com/betrusted-io/xous-core", rev = "616bf65f6e379165464f50b1e79ec42aff77a683" }
arbitrary-int = "1.2"

# pddb testing
rand = { version = "0.8.5", features = ["getrandom"] }
rand_chacha = { version = "0.3.1" }
rand_core = "0.6.4"

aes = { default-features = false, features = [
    "chaffing",
], git = "https://github.com/betrusted-io/xous-core", rev = "616bf65f6e379165464f50b1e79ec42aff77a683" }

# time testing
chrono = "0.4.33"

# QR code encoding
base45 = "3.1.0"

# hid bringup
xous-usb-hid = { git = "https://github.com/betrusted-io/xous-usb-hid.git", branch = "main", optional = true }

getrandom = "=0.2.12"

[patch.crates-io.getrandom]
git = "https://github.com/betrusted-io/xous-core"
rev = "616bf65f6e379165464f50b1e79ec42aff77a683"

[patch."https://github.com/betrusted-io/xous-core"]
bao1x-hal = { path = "../xous-core/libs/bao1x-hal" }
bao1x-hal-service = { path = "../xous-core/services/bao1x-hal-service" }
bao1x-api = { path = "../xous-core/libs/bao1x-api" }
modals = { path = "../xous-core/services/modals" }
ux-api = { path = "../xous-core/libs/ux-api" }
bio-lib = { path = "../xous-core/libs/bio-lib" }
xous-api-susres = { path = "../xous-core/api/xous-api-susres" }
keystore = {path = "../xous-core/services/keystore" }
usb-bao1x = { path = "../xous-core/services/usb-bao1x" }

[patch.crates-io.aes]
path = "../xous-core/services/aes"

[profile.release]
codegen-units = 1  # 1 better optimizations
debug = false
strip = false
lto = "fat"
incremental = true
#panic = "abort" # Remove panic output, which can reduce file size
opt-level = "s" # z,s: Optimize for size instead of performance; 1 for easier debugging; comment out for "best performance" (but in Rust 1.72 this causes regressions)

# aes testing
[features]
bao1x = ["utralib/bao1x", "usb-bao1x/bao1x", "aes/bao1x"]
board-baosec = [
    "modals/board-baosec",
    "bao1x-hal-service",
    "bao1x-hal",
    "usb-bao1x/board-baosec",
    "pddb/board-baosec",
    "xous-swapper",
    "ux-api/board-baosec",
    "bio-lib",
    "bao1x-hal/board-baosec",
    "bio-lib/ws2812",
]
oem-baosec-lite = ["bao1x-hal/sensor-lis2dh12"]
hosted-baosec = [
    "modals/hosted-baosec",
    "usb-bao1x/hosted-baosec",
    "bao1x-emu",
    "pddb/hosted-baosec",
    "bao1x-hal/hosted-baosec",
]
owc-test = ["keystore/owc-inc"]
# stress test for WFI resume
wfi-stress-test = []
# gate for misc test routines
misc-test = []
# for qa testing
qa-test = []
# this flag leaks secrets
hazardous-test = []

# these flags are for factory only builds
# owc-inc is necessary because we use a OWC to disable the factory cheat routine after run-once.
factory-mismatch = ["keystore/owc-inc"]
factory-wipe = ["keystore/owc-inc"]

uber = []

# Activating this makes this crate *try* to grab the DUART. To improve its odds
# of winning the race, shift its position to first in the list in the PIDs to be
# loaded and run.
duart-debug-hal = ["bao1x-hal/debug-print-duart"]

default = ["oem-baosec-lite"]
```

`build.sh` (verbatim, `/tmp/xous-scratch/dc34-console/build.sh`, identical to
`/tmp/xous-scratch/xous-core/services/dc34-console/build.sh`):

```sh
cargo build --release --target riscv32imac-unknown-xous-elf --features board-baosec --features bao1x \
  --features oem-baosec-lite --features utralib/bao1x
```

`.vscode/settings.json` rust-analyzer features (`/tmp/xous-scratch/xous-core/services/dc34-console/.vscode/settings.json`):
```json
{
    "rust-analyzer.cargo.features": [
        "bao1x",
        "board-baosec",
        "oem-baosec-lite",
    ],
    ...
}
```

`src/main.rs` (verbatim, `/tmp/xous-scratch/xous-core/services/dc34-console/src/main.rs`) — the in-tree
copy; the standalone copy is the same file modulo the `mod bio;` line:

```rust
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
mod bio;
mod cmds;
mod repl;
mod shell;
use cmds::*;
// mod fxcore;
mod leds;
mod power;

// .\baosign.ps1 -Config baosec-lite -Target bunnie@10.0.245.164:code/testjig/images/

fn main() {
    // first thing: initialize the WDT
    let mut wdt = bao1x_hal::wdt::Wdt::new();
    // set for nominally 20 seconds to WDT reset - assuming 50 MHz pclk on boot
    // this is "properly" set later on once the system has fully booted and
    // the clock manager is queryable
    wdt.enable((50_000_000 / 2) * 20, true);

    log_server::init_wait().unwrap();
    log::set_max_level(log::LevelFilter::Info);
    log::info!("my PID is {}", xous::process::id());
    #[cfg(feature = "duart-debug-hal")]
    bao1x_hal::claim_duart();

    let tt = ticktimer::Ticktimer::new().unwrap();
    tt.sleep_ms(500).ok(); // pause for the system to startup

    shell::start_shell();

    tt.sleep_ms(500).ok();
    leds::start_leds();

    let run_led_fade = Arc::new(AtomicBool::new(false));
    let plugged_in = Arc::new(AtomicBool::new(false));

    let usb = usb_bao1x::UsbHid::new();
    usb.serial_console_input_injection();

    std::thread::spawn({
        let run_led_fade = run_led_fade.clone();
        let plugged_in = plugged_in.clone();
        move || {
            let xns = xous_names::XousNames::new().unwrap();
            let gfx = ux_api::service::gfx::Gfx::new(&xns).unwrap();
            /* ... LED fade loop elided ... */
        }
    });

    // attempting to see if threading the power manager improves stability
    // safety: the wdt object is "retired" after this call, so there are no concurrency
    // issues, and the wdt memory address range is guaranteed to be correct and defined
    let wdt_addr = unsafe { wdt.to_raw() };
    std::thread::spawn({
        move || {
            power::power_manager(run_led_fade, plugged_in, wdt_addr);
        }
    });

    // idle forever, maybe turn this into a full blocking server that just parks and ends
    let dummy_sid = xous::create_server().unwrap();
    loop {
        // this just blocks forever, since the server ID is never passed to anyone else, effectively
        // parking the main thread
        let _ = xous::receive_message(dummy_sid);
    }
}
```

### 1b. `apps-dabao/helloworld` — the true minimum (in-tree)

`/tmp/xous-scratch/xous-core/apps-dabao/helloworld/Cargo.toml` (complete file, verbatim). **There is no
`build.rs` in this crate.**

```toml
[package]
name = "helloworld"
version = "0.1.0"
edition = "2021"

[dependencies]
log = "0.4.14"
num-derive = { version = "0.4.2", default-features = false }
num-traits = { version = "0.2.14", default-features = false }
xous = "0.9.70"
xous-ipc = "0.10.10"
log-server = { package = "xous-api-log", version = "0.1.69" }
xous-names = { package = "xous-api-names", version = "0.9.71" }
utralib = { version = "0.1.27", optional = true, default-features = false }

[features]
bao1x = ["utralib/bao1x"]
board-dabao = []
default = []
```

`/tmp/xous-scratch/xous-core/apps-dabao/helloworld/src/main.rs` (complete file, verbatim):

```rust
fn main() -> ! {
    // This boilerplate code sets up the logging infrastructure.
    log_server::init_wait().unwrap();
    log::set_max_level(log::LevelFilter::Info);
    log::info!("Hello world PID is {}", xous::process::id());

    // This delay of 5 seconds allows users watching over USB serial
    // console a few seconds to connect the console and see the output.
    // Otherwise, it will "print" to a console that's not listening.
    std::thread::sleep(std::time::Duration::from_secs(5));

    // This is the hello world!
    println!("Hello world!");

    // This ensures a graceful exit from the process.
    xous::terminate_process(0)
}
```

### 1c. `bunnie/dabao-console` — the documented out-of-tree reference

Only the delta from 1a matters, plus it is the one that **has a `build.rs`**.

`/tmp/xous-scratch/dabao-console/Cargo.toml` pins `rev = "6f71359d18f457855562e712d48034595de7c342"` on all
`betrusted-io/xous-core` deps, has **no `[patch."https://github.com/betrusted-io/xous-core"]` table** (so it
genuinely builds standalone), and adds:

```toml
[build-dependencies]
serde_json = "1"
zip = "2"
ureq = "2"
```

`build.rs` (`/tmp/xous-scratch/dabao-console/build.rs`, 340 lines) is a standalone port of
`cargo xtask install-toolkit`. Its purpose, verbatim from its own header comment:

```
// build.rs - standalone equivalent of `cargo xtask install-toolkit`
//
// Runs automatically when this crate is built. It ensures that:
//   1. The custom riscv32imac-unknown-xous-elf sysroot (with version-matching) is present.
//   2. The bare-metal riscv32imac-unknown-none-elf target is present (via rustup).
```

Key constants inside it (verbatim):
```rust
const TARGET_TRIPLE_RISCV32: &str = "riscv32imac-unknown-xous-elf";
const TARGET_TRIPLE_RISCV32_KERNEL: &str = "riscv32imac-unknown-none-elf";
const TOOLCHAIN_RELEASE_URL_RISCV32: &str = "https://api.github.com/repos/betrusted-io/rust/releases";
```
Escape hatches it honours: `SKIP_TOOLKIT_INSTALL=1`, `REINSTALL_TOOLKIT=1`.

Its build command (from `/tmp/xous-scratch/dabao-console/README.md`, verbatim):
```
cargo build --release --target riscv32imac-unknown-xous-elf --features board-dabao --features bao1x --features utralib/bao1x
```

`apps-dabao/helloworld`'s in-tree build command, from `/tmp/xous-scratch/xous-core/apps-dabao/README.md`:
```
cargo xtask dabao dabao-console --no-timestamp --kernel-feature debug-proc
```

### Badge vs. dabao — the concrete deltas

| | dabao | badge (baosec-lite) |
|---|---|---|
| target triple | `riscv32imac-unknown-xous-elf` | same |
| board feature | `board-dabao` | `board-baosec` |
| OEM feature | (none) | `oem-baosec-lite` — expands to `["bao1x-hal/sensor-lis2dh12"]` in the app, and `["bao1x-hal/oem-baosec-lite"]` in `usb-bao1x` |
| SoC features | `bao1x`, `utralib/bao1x` | same |
| app storage | RRAM. `bao1x_api::offsets::dabao::APP_RRAM_OFFSET = 0x30_0000`, `APP_RRAM_LEN = 0xD_A000 + SIGBLOCK_LEN` | **swap**. `bao1x_api::offsets::baosec::APP_RRAM_OFFSET = 0`, **`APP_RRAM_LEN = 0`** — there is literally no on-chip app region on baosec. `SWAP_RAM_LEN = 8192 * 1024` |
| uf2 artifact | `apps.uf2` | `swap.uf2` (see §5) |
| extra services available | ticktimer, keystore, log, names, usb-bao1x, bao1x-hal-service | + `xous-swapper`(PID 2), `keystore`(PID 3), `modals`, `pddb`, `bao-video` (graphics), and swap |

---

## 2. Memory syscalls

### 2a. `xous::rsyscall`

`/tmp/xous-scratch/xous-core/xous-rs/src/syscall.rs:1941`
```rust
pub fn rsyscall(call: SysCall) -> SysCallResult { crate::arch::syscall(call) }
```
`SysCallResult` is `core::result::Result<Result, Error>` where `Result` is `xous::Result`.

### 2b. `SysCall::AdjustProcessLimit` and `Limits`

`/tmp/xous-scratch/xous-core/xous-rs/src/syscall.rs:435-439` (with its doc-comment, lines 425-431):
```rust
    /// ## Returns
    ///
    /// Returns a Scalar2 containing `(Index, Limit)`.
    ///
    /// ## Errors
    ///
    /// * **InvalidLimit**: The specified index was not valid
    AdjustProcessLimit(
        usize, /* process limit index */
        usize, /* expected current limit */
        usize, /* proposed new limit */
    ),
```
Syscall number: `AdjustProcessLimit = 38` (`syscall.rs:619`).

`/tmp/xous-scratch/xous-core/xous-rs/src/definitions/limits.rs` (complete file, verbatim):
```rust
#[repr(usize)]
#[derive(Debug)]
pub enum Limits {
    HeapMaximum = 1,
    HeapSize = 2,
}
```

Kernel side, `/tmp/xous-scratch/xous-core/kernel/src/syscall.rs:1043-1057`:
```rust
        SysCall::AdjustProcessLimit(index, current, new) => match index {
            1 => arch::process::Process::with_inner_mut(|p| {
                if p.mem_heap_max == current {
                    p.mem_heap_max = new;
                }
                Ok(xous_kernel::Result::Scalar2(index, p.mem_heap_max))
            }),
            2 => arch::process::Process::with_inner_mut(|p| {
                if p.mem_heap_size == current && new < p.mem_heap_max {
                    p.mem_heap_size = new;
                }
                Ok(xous_kernel::Result::Scalar2(index, p.mem_heap_size))
            }),
            _ => Err(xous_kernel::Error::InvalidLimit),
        },
```
Surprise worth knowing: the syscall is a compare-and-set. Passing a `current` that doesn't match leaves
the limit unchanged **but still returns `Ok(Scalar2(index, actual))`** — that's exactly why the two-call
idiom works (call 1 with `current = 0` is a deliberate no-op read).

### 2c. The two-call idiom, verbatim

`/tmp/xous-scratch/xous-core/services/swaptest1/src/main.rs:1-21` (complete, verbatim). Identical code
appears in `services/swaptest2`, `services/test-swapper`, `services/pddb/src/main.rs:722`,
`apps/mtxcli`, `apps/mtxchat`, `apps/chat-test`.

```rust
fn main() {
    log_server::init_wait().unwrap();
    log::set_max_level(log::LevelFilter::Info);
    log::info!("my PID is {}", xous::process::id());

    const HEAP_LARGER_LIMIT: usize = 4096 * 1024;
    let new_limit = HEAP_LARGER_LIMIT;
    let result =
        xous::rsyscall(xous::SysCall::AdjustProcessLimit(xous::Limits::HeapMaximum as usize, 0, new_limit));

    if let Ok(xous::Result::Scalar2(1, current_limit)) = result {
        xous::rsyscall(xous::SysCall::AdjustProcessLimit(
            xous::Limits::HeapMaximum as usize,
            current_limit,
            new_limit,
        ))
        .unwrap();
        log::info!("Heap limit increased to: {}", new_limit);
    } else {
        panic!("Unsupported syscall!");
    }
```
`services/swaptest1/Cargo.toml` uses `xous = { version = "0.9.70", features = ["raw-trng"] }` — no `swap`
feature needed for `AdjustProcessLimit`.

### 2d. `xous::map_memory`

`/tmp/xous-scratch/xous-core/xous-rs/src/syscall.rs:1215-1231` (verbatim, including its doc comment):
```rust
/// Map the given physical address to the given virtual address.
/// The `size` field must be page-aligned.
pub fn map_memory(
    phys: Option<MemoryAddress>,
    virt: Option<MemoryAddress>,
    size: usize,
    flags: MemoryFlags,
) -> core::result::Result<MemoryRange, Error> {
    crate::arch::map_memory_pre(&phys, &virt, size, flags)?;
    let result =
        rsyscall(SysCall::MapMemory(phys, virt, MemorySize::new(size).ok_or(Error::InvalidSyscall)?, flags))?;
    if let Result::MemoryRange(range) = result {
        Ok(crate::arch::map_memory_post(phys, virt, size, flags, range)?)
    } else if let Result::Error(e) = result {
        Err(e)
    } else {
        Err(Error::InternalError)
    }
}
```
Also re-exported as `xous::syscall::map_memory` (both spellings appear in-tree).
`MemoryAddress = NonZeroUsize`, `MemorySize = NonZeroUsize`.
Companion: `pub fn unmap_memory(range: MemoryRange) -> core::result::Result<(), Error>` at `syscall.rs:1237`,
and `pub fn increase_heap(bytes: usize, flags: MemoryFlags) -> core::result::Result<MemoryRange, ()>` at
`syscall.rs:1930`.

`MemoryFlags`, `/tmp/xous-scratch/xous-core/xous-rs/src/definitions/memoryflags.rs:9-28` (verbatim):
```rust
impl MemoryFlags {
    /// Marks the page as the 'device' page for on-chip peripherals.
    pub const DEV: Self = Self { bits: 0b0001_0000 };
    const FLAGS_ALL: usize = 0b11_1111;
    /// Free this memory
    pub const FREE: Self = Self { bits: 0b0000_0000 };
    /// Page is swapped
    pub const P: Self = Self { bits: 0b10_0000_0000 };
    /// Allow the CPU to read from this page.
    pub const R: Self = Self { bits: 0b0000_0010 };
    /// Immediately allocate this memory.  Otherwise it will
    /// be demand-paged.  This is implicitly set when `phys`
    /// is not 0.
    pub const RESERVE: Self = Self { bits: 0b0000_0001 };
    /// Marks the page as pure virtual; i.e., memory mapped SPI FLASH
    pub const VIRT: Self = Self { bits: 0b0010_0000 };
    /// Allow the CPU to write to this page.
    pub const W: Self = Self { bits: 0b0000_0100 };
    /// Allow the CPU to execute from this page.
    pub const X: Self = Self { bits: 0b0000_1000 };
```

**How to request demand-paged anonymous memory of a given size:** pass `phys = None`, `virt = None`,
a page-aligned `size`, and flags **without** `RESERVE`. Per the `RESERVE` doc comment above, omitting
`RESERVE` is exactly what makes it demand-paged.

Real in-tree example, `/tmp/xous-scratch/xous-core/services/graphics-server/src/main.rs:354-359`:
```rust
                        let mut stashmem = xous::syscall::map_memory(
                            None,
                            None,
                            ((FB_SIZE * 4) + 4096) & !4095,
                            xous::MemoryFlags::R | xous::MemoryFlags::W,
                        )
                        .expect("couldn't map stash frame buffer");
```
Contrast (immediately-committed), `/tmp/xous-scratch/xous-core/services/cram-console/src/cmds/pddb_cmd.rs:33-39`:
```rust
        let perfbuf = xous::syscall::map_memory(
            None,
            None,
            BUFLEN,
            xous::MemoryFlags::R | xous::MemoryFlags::W | xous::MemoryFlags::RESERVE,
        )
        .expect("couldn't map in the performance buffer");
```
And the memory-mapped SPI FLASH case (baosec-only), `/tmp/xous-scratch/xous-core/services/bao-console/src/main.rs:31-37`:
```rust
        let mut spimap = xous::map_memory(
            None,
            xous::MemoryAddress::new(xous::arch::MMAP_VIRT_BASE),
            bao1x_hal::board::SPINOR_LEN as usize,
            xous::MemoryFlags::R | xous::MemoryFlags::VIRT,
        )
        .expect("couldn't map spi range");
```

Kernel enforcement (`kernel/src/syscall.rs:746-749`): `size.get() & (PAGE_SIZE - 1) != 0` →
`Error::BadAlignment`. `PAGE_SIZE = 4096`.

`MemoryRange` accessors, `/tmp/xous-scratch/xous-core/xous-rs/src/definitions/memoryrange.rs`:
```rust
    pub unsafe fn new(addr: usize, size: usize) -> core::result::Result<MemoryRange, Error>
    pub fn len(&self) -> usize
    pub fn is_empty(&self) -> bool          // NOTE: body is `self.size.get() > 0` — this looks like a bug
    pub fn as_ptr(&self) -> *const u8
    pub fn as_mut_ptr(&self) -> *mut u8
    pub unsafe fn as_slice<T>(&self) -> &[T]
    pub unsafe fn as_slice_mut<T>(&mut self) -> &mut [T]
```

### 2e. The free/total pages syscall — **it is NOT callable from an app**

This is the biggest correction to your brief.

The name is right — `GetFreePages` — but it is a **swapper-only** operation reached through
`SysCall::SwapOp`, and the kernel rejects it from any PID other than the swapper.

`/tmp/xous-scratch/xous-core/xous-rs/src/syscall.rs:538-543` (verbatim, with doc comment):
```rust
    /// Swapper operation.
    ///
    /// This syscall can only be called by PID 2, the swapper. The form of the
    /// call is deliberately left flexible, so that the swapper ABI can evolve
    /// without impacting version compatibility with application ABIs.
    ///
    /// ## Arguments
    ///     * Up to 7 `usize` values, whose ABI is determined by the swapper's implementation.
    ///
    /// ## Returns
    /// Returns a Scalar5, whose ABI is determined by the swapper's implementation.
    ///
    /// ## Errors
    ///     * **BadAddress**: The mapping does not exist
    ///     * **AccessDenied**: Called by a PID that does not belong to the swapper
    ///     ...
    #[cfg(feature = "swap")]
    SwapOp(usize, usize, usize, usize, usize, usize, usize),
```
Requires `xous = { features = ["swap"] }` to even name the variant. Syscall number `SwapOp = 44`.

`SwapAbi` is deliberately **not in `xous-rs`**. It is duplicated in two places
(`/tmp/xous-scratch/xous-core/services/xous-swapper/src/lib.rs:14-29` and `kernel/src/swap.rs:39`):
```rust
/// userspace swapper -> kernel ABI
/// This ABI is copy-paste synchronized with what's in the kernel. It's left out of
/// xous-rs so that we can change it without having to push crates to crates.io.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SwapAbi {
    Invalid = 0,
    ClearMemoryNow = 1,
    GetFreePages = 2,
    // RetrievePage = 3, // meant to be initiated within the kernel to itself
    // HardOom = 4, // meant to be initiated within the kernel to itself
    StealPage = 5,
    ReleaseMemory = 6,
    WritePage = 7,
    BlockErase = 8,
    DebugProcesses = 9,
    DebugServers = 10,
    DebugFree = 11,
    DebugInterrupts = 12,
}
```

The call site, `/tmp/xous-scratch/xous-core/services/xous-swapper/src/main.rs:274-279` (verbatim):
```rust
/// Convenience wrapper for GetFreePages syscall
fn get_free_pages() -> usize {
    match xous::rsyscall(xous::SysCall::SwapOp(SwapAbi::GetFreePages as usize, 0, 0, 0, 0, 0, 0)) {
        Ok(Result::Scalar5(free_pages, _total_memory, _, _, _)) => free_pages,
        _ => panic!("GetFreeMem syscall failed"),
    }
}
```

Kernel gate, `/tmp/xous-scratch/xous-core/kernel/src/syscall.rs:1093-1098` (verbatim):
```rust
                SwapAbi::GetFreePages => {
                    if pid.get() != xous_kernel::SWAPPER_PID {
                        return Err(xous_kernel::Error::AccessDenied);
                    }
                    Swap::with(|swap| swap.get_free_mem())
                }
```

Return shape, `/tmp/xous-scratch/xous-core/kernel/src/swap.rs:357-389`:
```rust
    /// This is a non-divergent syscall (handled entirely within the kernel)
    pub fn get_free_mem(&self) -> SysCallResult {
        // ... prints a per-PID RAM usage table to the kernel console ...
        let ram_size = crate::mem::MemoryManager::with(|mm| mm.memory_size());
        Ok(xous_kernel::Result::Scalar5(
            ram_size / PAGE_SIZE - self.used_pages,
            ram_size / PAGE_SIZE,
            0,
            0,
            0,
        ))
    }
```
So: `Scalar5(free_pages, total_pages, 0, 0, 0)`. Note it *unconditionally* prints a RAM usage table to
the kernel console (the `#[cfg(feature = "debug-swap")]` above it is commented out).

**What an app CAN call instead** — `xous_swapper::Swapper`, an IPC client, not a syscall.
`/tmp/xous-scratch/xous-core/services/xous-swapper/src/lib.rs:85-146`:
```rust
pub const SWAPPER_PUBLIC_NAME: &'static str = "_swapper server_";

pub struct Swapper {
    conn: xous::CID,
}
impl Swapper {
    pub fn new() -> Result<Self, xous::Error> { ... }

    /// Attempts to free `page_count` pages of RAM.
    pub fn garbage_collect_pages(&self, page_count: usize) -> usize {
        match xous::send_message(
            self.conn,
            xous::Message::new_blocking_scalar(Opcode::GarbageCollect as usize, page_count, 0, 0, 0),
        ) {
            Ok(xous::Result::Scalar5(free_pages, _, _, _, _)) => free_pages,
            _e => { log::warn!(...); 0 }
        }
    }

    pub fn write_page(&self, offset: usize, page: &FlashPage) -> Result<xous::Result, xous::Error>
    pub fn block_erase(&self, offset: usize, len: usize) -> Result<xous::Result, xous::Error>
}
```
`garbage_collect_pages(0)` is the closest thing to a free-page query available to an app: it returns the
free page count. (I did not find any other app-reachable free/total memory API — see the NOT-FOUND list.)

---

## 3. USB serial from an app

The type is **`usb_bao1x::UsbHid`**, defined in
`/tmp/xous-scratch/xous-core/services/usb-bao1x/src/lib.rs:16-19`. The name is acknowledged as wrong in a
comment right above it: `// TODO: this object is misnamed, it also includes a serial handler`.

### How to obtain it (`lib.rs:21-28`, verbatim)
```rust
    pub fn new() -> Self {
        let xns = xous_names::XousNames::new().expect("couldn't connect to XousNames");
        REFCOUNT.fetch_add(1, Ordering::Relaxed);
        let conn = xns
            .request_connection_blocking(api::SERVER_NAME_USB_DEVICE)
            .expect("Can't connect to USB device server");
        UsbHid { conn }
    }
```
Note `new()` returns `Self` (panics on failure), not a `Result`. `impl Drop` disconnects at refcount 0.
Cargo dep for an out-of-tree badge app:
`usb-bao1x = { git = "...xous-core", rev = "...", features = ["bao1x", "board-baosec", "oem-baosec-lite"] }`.

### Exact signatures (all from `services/usb-bao1x/src/lib.rs`)

```rust
// lib.rs:225 — Blocks until an ASCII string terminated by `delimiter` is received on serial; if `None`,
//              it will return as soon as a character (or series of characters) have been received (thus
//              the return `String` will be piecemeal)
pub fn serial_wait_ascii(&self, delimiter: Option<char>) -> String

// lib.rs:238 — Blocks until enough binary data has been received to fill the buffer.
//              Another thread can be used to call serial_flush() if we don't want to block forever
//              and we're receiving small amounts of binary data.
pub fn serial_wait_binary(&self) -> Vec<u8>

// lib.rs:249 — Non-blocking call that issues a serial flush command to the USB stack
pub fn serial_flush(&self) -> Result<(), xous::Error>

// lib.rs:256 — Inject serial input over USB to the debug console. Dangerous!
//              This will also override/discard any existing hooked listeners.
pub fn serial_console_input_injection(&self)          // NOTE: no return value; errors are `.ok()`d away

// lib.rs:264
pub fn serial_clear_input_hooks(&self)                // NOTE: no return value; `.unwrap()`s internally

// lib.rs:281
pub fn serial_send_nb(&self, data: &[u8]) -> Result<usize, xous::Error>

// lib.rs:311
pub fn serial_send(&self, data: &[u8]) -> Result<usize, xous::Error>

// lib.rs:409 — also useful for §7
pub fn set_log_level(&self, level: LogLevel)          // LogLevel { Trace=0, Debug=1, Info=2, Warn=3, Err=4 }
                                                      //   -> sets log level *inside the usb-bao1x server*
pub fn cid(&self) -> xous::CID
```

Size cap for both send paths: `api.rs:168` — `pub const SERIAL_BINARY_BUFLEN: usize = 3840; // save 256
bytes on the page for Rkyv overhead`. Both `serial_send*` silently truncate to `data.len().min(SERIAL_BINARY_BUFLEN)`
and return the truncated length; **you must loop yourself for longer payloads.**

Semantics of the two send calls, verbatim from their doc comments:
- `serial_send_nb`: *"The returned value is the number of bytes submitted in the IPC request. It does not
  indicate how many bytes were accepted by the USB CDC transmit buffer or delivered to the host."*
- `serial_send`: *"The returned value is the length of the contiguous prefix accepted by
  `serial_port.write()`. A value smaller than the requested length indicates a short write or write error;
  the caller may retry the remaining suffix. Returns `Ok(0)` when the USB device is not configured."*

### How an app takes serial input away from the log server

`serial_console_input_injection()` does **not** take input away from the log server — it does the
opposite: it asks the log server to *also mirror its output* onto USB, and routes USB serial RX into the
**keyboard** server as synthetic keystrokes. The full sequence, verbatim from
`/tmp/xous-scratch/xous-core/services/usb-bao1x/src/main.rs:687-712`:

```rust
            Opcode::SerialHookConsole => msg_scalar_unpack!(msg, _, _, _, _, {
                let log_conn = xous::connect(xous::SID::from_bytes(b"xous-log-server ").unwrap()).unwrap();
                match xous::send_message(
                    log_conn,
                    xous::Message::new_blocking_scalar(
                        log_server::api::Opcode::TryHookUsbMirror.to_usize().unwrap(),
                        0,
                        0,
                        0,
                        0,
                    ),
                ) {
                    Ok(xous::Result::Scalar1(result)) => {
                        if result == 1 {
                            serial_listen_mode = SerialListenMode::ConsoleListener;
                            // unhook any previous pending listener
                            serial_listener.take();
                        } else {
                            log::error!("Error trying to connect USB console.");
                        }
                    }
                    _ => {
                        log::error!("Could not connect USB console");
                    }
                }
            }),
```

RX side, `main.rs:606-618` (verbatim) — where the characters actually go:
```rust
                        SerialListenMode::ConsoleListener => {
                            match std::str::from_utf8(&serial_buf) {
                                Ok(s) => {
                                    for c in s.chars() {
                                        native_kbd.inject_key(c);
                                    }
                                }
                                Err(_) => {
                                    log::info!("Non UTF-8 received on console: {:x?}", &serial_buf);
                                }
                            }
                            serial_buf.clear();
                        }
```
`native_kbd` is created at `main.rs:91`:
`let native_kbd = bao1x_api::keyboard::Keyboard::new(&xns).expect("couldn't connect to keyboard service");`

And the un-hook, `main.rs:713-729` (verbatim):
```rust
            Opcode::SerialClearHooks => {
                let log_conn = xous::connect(xous::SID::from_bytes(b"xous-log-server ").unwrap()).unwrap();
                // it is never harmful to double-unhook this
                xous::send_message(
                    log_conn,
                    xous::Message::new_blocking_scalar(
                        log_server::api::Opcode::UnhookUsbMirror.to_usize().unwrap(),
                        0,
                        0,
                        0,
                        0,
                    ),
                )
                .ok();

                serial_listen_mode = SerialListenMode::NoListener;
                serial_listener.take();
            }
```

Listen modes, `main.rs:17-32` (verbatim — read the "buffer indefinitely" warnings):
```rust
enum SerialListenMode {
    // this just causes data incoming to be printed to the debug log; it is the default
    NoListener,
    // this assumes there will be a CR/LF character to delimit lines (the `char` arg), and
    // will buffer data until two conditions are met: 1) a listener is hooked and 2) a CR/LF is received.
    // This will "infinitely" buffer incoming characters if no listener is hooked.
    AsciiListener(Option<char>),
    // this will simply buffer the data until the `usize` argument is met and passes it back to
    // hooked listener. If this mode is set and there is no listener, it will buffer data "indefinitely"
    // (e.g. until local heap is exhausted and the system panics)
    BinaryListener,
    // this will take any serial input and pass it on as if one was typing at the console
    ConsoleListener,
}
```

**So there are two distinct architectures, and dc34-console uses the first:**

**(A) Console-injection (what `dc34-console` does).** `usb.serial_console_input_injection()` once at
startup; then the app receives characters as *keyboard events*, and writes output with plain `println!`
(which the log server mirrors to USB). The actual receive path in dc34-console
(`/tmp/xous-scratch/xous-core/services/dc34-console/src/shell.rs:25-47`, verbatim excerpt):

```rust
fn shell() {
    let xns = xous_names::XousNames::new().unwrap();
    // unlimited connections allowed, this is a user app and it's up to the app to decide its policy
    let shch_sid = xns.register_name(SERVER_NAME_SHELLCHAT, None).expect("can't register server");

    let kbd = keyboard::Keyboard::new(&xns).unwrap();

    let mut repl = crate::repl::Repl::new(&xns);
    ...
    // register this late because the REPL can take a while to init as it depends on the PDDB.
    kbd.register_listener(SERVER_NAME_SHELLCHAT, ConsoleOp::Keypress.to_u32().unwrap() as usize);
    let mut input = String::new();
    loop {
        let msg = xous::receive_message(shch_sid).unwrap();
        let console_op: Option<ConsoleOp> = FromPrimitive::from_usize(msg.body.id());
        match console_op {
            Some(ConsoleOp::Keypress) => msg_scalar_unpack!(msg, k1, _k2, _k3, _k4, {
                let k = char::from_u32(k1 as u32).unwrap_or('\u{0000}');
                ...
```
Gotchas: `use bao1x_api::*;` is at the top of that file, so `keyboard::Keyboard` resolves to
**`bao1x_api::keyboard::Keyboard`**, not the `services/keyboard` crate. Its signatures
(`/tmp/xous-scratch/xous-core/libs/bao1x-api/src/keyboard.rs:126-207`):
```rust
    pub fn new(xns: &xous_names::XousNames) -> Result<Self, xous::Error>
    pub fn register_listener(&self, server_name: &str, action_opcode: usize)   // #[cfg(feature = "std")]
    pub fn register_observer(&self, server_name: &str, action_opcode: usize)   // #[cfg(feature = "std")]
    pub fn get_keys_blocking(&self) -> Vec<char>                                // #[cfg(feature = "std")]
    pub fn inject_key(&self, c: char)
    pub fn get_keymap(&self) -> Result<KeyMap, xous::Error>
    pub fn set_keymap(&self, map: KeyMap) -> Result<(), xous::Error>
```
Server name `pub const SERVER_NAME_KBD: &str = "_Matrix keyboard driver_";`
(`libs/bao1x-api/src/lib.rs:50`), registered by `services/bao1x-hal-service/src/servers/keyboard.rs:275`
on hardware and by `services/bao1x-emu/src/keyboard.rs:39` in hosted mode.
Note dc34-console's shell also handles the arrow chars `'↑'`/`'↓'` and `0x08` backspace itself.
Also `dabao-console` echoes typed characters back with `usb.serial_send_nb(&[k1 as u8]).ok();`
(`/tmp/xous-scratch/dabao-console/src/shell.rs:53`).

**(B) Exclusive ownership (what you probably actually want for a clean channel).**
`usb.serial_clear_input_hooks()` → this sends `UnhookUsbMirror` to the log server (so log traffic stops
going to USB) *and* sets `SerialListenMode::NoListener`. Then drive the port directly with
`serial_wait_ascii(Some('\n'))` / `serial_wait_binary()` for input and `serial_send()` for output. See §7.
I did **not** find an in-tree app that does (B) end-to-end; `services/bao-console/src/cmds/usb.rs`,
`services/shellchat/src/cmds/usb.rs` and `apps-dabao/dabao-console/src/cmds/usb.rs` only expose
`serial_console_input_injection` / `serial_clear_input_hooks` / `serial_send_nb` as REPL commands.

---

## 4. Graphics

Server name: `/tmp/xous-scratch/xous-core/libs/ux-api/src/service/api.rs:27` —
`pub const SERVER_NAME_GFX: &str = "_Graphics_";`. On baosec/badge this server is implemented by
**`services/bao-video`** (camera + display + QR in one process); on hosted mode by the same crate built
with `hosted-baosec`.

### 4a. `ux_api::service::gfx::Gfx` — `/tmp/xous-scratch/xous-core/libs/ux-api/src/service/gfx.rs`

```rust
// gfx.rs:10-13
#[derive(Debug)]
pub struct Gfx {
    conn: xous::CID,
}

// gfx.rs:23
pub fn new(xns: &xous_names::XousNames) -> Result<Self, xous::Error>
// gfx.rs:31
pub fn conn(&self) -> xous::CID
// gfx.rs:236
pub fn screen_size(&self) -> Result<Point, xous::Error>
// gfx.rs:261
pub fn glyph_height_hint(&self, glyph: GlyphStyle) -> Result<usize, xous::Error>
// gfx.rs:305
pub fn draw_textview(&self, tv: &mut TextView) -> Result<(), xous::Error>
// gfx.rs:324
pub fn bounds_compute_textview(&self, tv: &mut TextView) -> Result<(), xous::Error>
// gfx.rs:341
pub fn clear(&self) -> Result<(), xous::Error>
// gfx.rs:169
pub fn flush(&self) -> Result<(), xous::Error>
// gfx.rs:50 / :81 / :112 / :143
pub fn draw_line(&self, line: Line) -> Result<(), xous::Error>
pub fn draw_circle(&self, circ: Circle) -> Result<(), xous::Error>
pub fn draw_rectangle(&self, rect: Rectangle) -> Result<(), xous::Error>
pub fn draw_rounded_rectangle(&self, rr: RoundedRectangle) -> Result<(), xous::Error>
// gfx.rs:818 / :827 — both #[cfg(feature = "board-baosec")]
pub fn brightness(&self, level: u8) -> Result<(), xous::Error>
pub fn brightness_nonblocking(&self, level: u8)
// gfx.rs:192 / :219 — both blit the 128x128 baochip bitmap on baosec
pub fn draw_sleepscreen(&self) -> Result<(), xous::Error>
pub fn draw_boot_logo(&self) -> Result<(), xous::Error>
```
Construction, `gfx.rs:23-28` (verbatim):
```rust
    pub fn new(xns: &xous_names::XousNames) -> Result<Self, xous::Error> {
        REFCOUNT.fetch_add(1, Ordering::Relaxed);
        let conn = xns
            .request_connection_blocking(crate::service::api::SERVER_NAME_GFX)
            .expect("Can't connect to GFX");
        Ok(Gfx { conn })
    }
```
i.e. `let gfx = ux_api::service::gfx::Gfx::new(&xns).unwrap();` — exactly as dc34-console does at
`services/dc34-console/src/main.rs:49`.

`draw_textview` body, `gfx.rs:305-320` — note the silent truncation, and which fields come back:
```rust
    pub fn draw_textview(&self, tv: &mut TextView) -> Result<(), xous::Error> {
        if tv.text.len() > TEXTVIEW_LEN {
            tv.text.truncate(TEXTVIEW_LEN);
        }
        let mut buf = Buffer::into_buf(tv.clone()).or(Err(xous::Error::InternalError))?;
        buf.lend_mut(self.conn, GfxOpcode::DrawTextView.to_u32().unwrap())
            .or(Err(xous::Error::InternalError))?;

        let tvr = buf.to_original::<TextView, _>().unwrap();
        tv.bounds_computed = tvr.bounds_computed;
        tv.cursor = tvr.cursor;
        tv.overflow = tvr.overflow;
        tv.busy_animation_state = tvr.busy_animation_state;
        Ok(())
    }
```
`bounds_compute_textview`, `gfx.rs:324-339` — it's the same opcode with `set_dry_run(true)`:
```rust
    /// Bounds computation does no checks on security since it's a non-drawing operation. While normal drawing
    /// always takes the bounds from the canvas, the caller can specify a clip_rect in this tv, instead of
    /// drawing the clip_rect from the Canvas associated with the tv.
    pub fn bounds_compute_textview(&self, tv: &mut TextView) -> Result<(), xous::Error> {
        let mut tv_query = tv.clone();
        tv_query.set_dry_run(true);
        let mut buf = Buffer::into_buf(tv_query).or(Err(xous::Error::InternalError))?;
        buf.lend_mut(self.conn, GfxOpcode::DrawTextView.to_u32().unwrap())
            .or(Err(xous::Error::InternalError))?;
        let tvr = buf.to_original::<TextView, _>().unwrap();

        tv.cursor = tvr.cursor;
        tv.bounds_computed = tvr.bounds_computed;
        tv.overflow = tvr.overflow;
        // don't update the animation state when just computing the textview bounds
        // tv.busy_animation_state = tvr.busy_animation_state;
        Ok(())
    }
```
`clear` is a **non-blocking** scalar (`gfx.rs:341-344`) — it will not have taken effect when it returns:
```rust
    /// Clear the screen in a device-optimized fashion. The exact background color depends on the device.
    pub fn clear(&self) -> Result<(), xous::Error> {
        send_message(self.conn, Message::new_scalar(GfxOpcode::Clear.to_usize().unwrap(), 0, 0, 0, 0))
            .map(|_| ())
    }
```

### 4b. `TextView` — `/tmp/xous-scratch/xous-core/libs/ux-api/src/minigfx/textview.rs`

Constructor (`textview.rs:126-151`, verbatim):
```rust
    pub fn new(canvas: Gid, bounds_hint: TextBounds) -> Self {
        TextView {
            canvas,
            operation: TextOp::Nop,
            untrusted: true,
            token: None,
            invert: false,
            clip_rect: None,
            bounds_hint,
            bounds_computed: None,
            style: TEXTVIEW_DEFAULT_STYLE,
            text: String::new(),
            cursor: Cursor::new(0, 0, 0),
            insertion: None,
            ellipsis: false,
            draw_border: true,
            border_width: 1,
            rounded_border: None,
            margin: Point { x: 4, y: 4 },
            selected: None,
            clear_area: true,
            overflow: None,
            dry_run: false,
            busy_animation_state: None,
        }
    }
```
Constants (`textview.rs:78-79`):
```rust
pub const TEXTVIEW_LEN: usize = 3072;
pub const TEXTVIEW_DEFAULT_STYLE: GlyphStyle = GlyphStyle::Regular;
```

Bounds are expressed with `TextBounds` (`textview.rs:9-26`, verbatim, all local to the canvas not the screen):
```rust
/// coordinates are local to the canvas, not absolute to the screen
pub enum TextBounds {
    // fixed width and height in a rectangle
    BoundingBox(Rectangle),
    // fixed width, grows up from bottom right
    GrowableFromBr(Point, u16),
    // fixed width, grows down from top left
    GrowableFromTl(Point, u16),
    // fixed width, grows up from bottom left
    GrowableFromBl(Point, u16),
    // fixed width, grows down from top right
    GrowableFromTr(Point, u16),
    // fixed width, centered aligned top
    CenteredTop(Rectangle),
    // fixed width, centered aligned bottom
    CenteredBot(Rectangle),
}
```
Public fields you'll actually set: `clip_rect: Option<Rectangle>`, `style: GlyphStyle`,
`draw_border: bool`, `border_width: u16`, `rounded_border: Option<u16>`, `clear_area: bool`,
`margin: Point`, `invert: bool`, `ellipsis: bool`, `insertion: Option<i32>`, `cursor: Cursor`,
`bounds_hint: TextBounds`, `bounds_computed: Option<Rectangle>`, `overflow: Option<bool>`,
`selected: Option<[u32;2]>`, `busy_animation_state: Option<u32>`, `text: String`.
Private (use accessors): `operation` (`set_op`/`get_op`), `canvas` (`get_canvas_gid`),
`dry_run` (`set_dry_run`/`dry_run()`).

Getting text in — two idioms, both used in-tree. `impl core::fmt::Write for TextView` at `textview.rs:217-222`:
```rust
// allow `write!()` macro on a` &TextView`
impl core::fmt::Write for TextView {
    fn write_str(&mut self, s: &str) -> core::result::Result<(), core::fmt::Error> {
        write!(self.text, "{}", s)
    }
}
```
so either `write!(tv, "...")` (needs `use core::fmt::Write;` in scope) or `write!(tv.text, "...")`.

`Gid`: `/tmp/xous-scratch/xous-core/libs/ux-api/src/service/api.rs:7-17`, with `Gid::new([u32;4])` and
`Gid::dummy()`. For direct `Gfx` (non-GAM) use, in-tree code passes `Gid::new([0,0,0,0])` or `Gid::dummy()`.
`Cursor`: `/tmp/xous-scratch/xous-core/libs/ux-api/src/minigfx/cursor.rs:15-18` — `{ pt: Point, line_height: usize }`.

### 4c. `GlyphStyle` and the character grid

`/tmp/xous-scratch/xous-core/libs/blitstr2/src/glyphstyle.rs:1-12` (verbatim):
```rust
/// Style options for Latin script fonts
pub enum GlyphStyle {
    Small = 0,
    Regular = 1,
    Bold = 2,
    Monospace = 3,
    Cjk = 4,
    Large = 5,
    ExtraLarge = 6,
    Tall = 7,
}
```
There is also `impl From<usize> for GlyphStyle` (unknown values → `Regular`) and `impl From<GlyphStyle> for usize`.
`/tmp/xous-scratch/xous-core/libs/ux-api/src/lib.rs:15` — `pub const SYSTEM_STYLE: blitstr2::GlyphStyle = blitstr2::GlyphStyle::Tall;`

Heights, `/tmp/xous-scratch/xous-core/libs/blitstr2/src/glyphstyle.rs:54-65` (verbatim; the identical
function is *also* at `libs/blitstr2/src/lib.rs:52-63` as `glyph_height_hint`):
```rust
pub fn glyph_to_height_hint(g: GlyphStyle) -> usize {
    match g {
        GlyphStyle::Small => 12,      // crate::blitstr2::fonts::small::MAX_HEIGHT as usize,
        GlyphStyle::Regular => 15,    // crate::blitstr2::fonts::regular::MAX_HEIGHT as usize,
        GlyphStyle::Bold => 15,       // crate::blitstr2::fonts::regular::MAX_HEIGHT as usize,
        GlyphStyle::Monospace => 15,  // crate::blitstr2::fonts::mono::MAX_HEIGHT as usize,
        GlyphStyle::Cjk => 16,        // crate::blistr2::fonts::emoji::MAX_HEIGHT as usize,
        GlyphStyle::Large => 24,      // 2x of small
        GlyphStyle::ExtraLarge => 30, // 2x of regular
        GlyphStyle::Tall => 19,
    }
}
```

**Width: there is no `glyph_to_width_hint` and the IPC call returns height only.** Handler, verbatim,
`/tmp/xous-scratch/xous-core/libs/ux-api/src/minigfx/handlers.rs:181-191`:
```rust
pub fn query_glyph_props(msg: &mut xous::envelope::Envelope) {
    if let Some(scalar) = msg.body.scalar_message_mut() {
        let style = scalar.arg1;
        let glyph = GlyphStyle::from(style);

        scalar.arg1 = glyph.into();
        scalar.arg2 = glyph_to_height_hint(glyph);
    } else {
        panic!("Incorrect message type");
    }
}
```
(`Gfx::glyph_height_hint` reads `Scalar5(_, _, h, _, _)`, i.e. `arg2`.)

Width comes from the per-glyph sprite, `/tmp/xous-scratch/xous-core/libs/blitstr2/src/lib.rs:28-45`:
```rust
pub struct GlyphSprite {
    pub glyph: &'static [u32],
    pub wide: u8,
    pub high: u8,
    pub kern: u8,
    pub ch: char,
    pub invert: bool,
    pub insert: bool,
    pub double: bool,
    pub large: bool,
}
```
looked up by `libs/blitstr2/src/lib.rs:66` —
`pub fn style_glyph(locale: &'static str, ch: char, base_style: &GlyphStyle) -> GlyphSprite`,
and advanced in `libs/ux-api/src/minigfx/cursor.rs:31-34` via `self.pt.x += glyph.wide as isize;`.

**For a fixed character grid, use `GlyphStyle::Monospace`: every ASCII entry of
`/tmp/xous-scratch/xous-core/libs/blitstr2/src/fonts/mono.rs:463` `pub const WIDTHS: [u8; 207]` is `7`,
and `MAX_HEIGHT` is `15` (`fonts/mono.rs:15`).** Other fonts are proportional (`regular::WIDTHS[n]`,
`small::WIDTHS[n]`; `Large` = `small::WIDTHS[n]*2`, `ExtraLarge` = `regular::WIDTHS[n]*2`).
`MAX_HEIGHT` per font: small 12, regular 15, bold 15, mono 15, tall 19, emoji 16.
(Only three of mono's 207 entries are not 7: `©`, `®` and the replacement glyph `U+FFFD` are 8.
No ASCII character is.)

#### Correction: the mono grid is **16** columns, not 18 (Task 7)

> An earlier revision of this section concluded `128/7 = 18` columns. **That is wrong**, and it is
> worth being precise about why, because `7` is a real number read out of the real source — it is
> just not the number that governs layout. `WIDTHS[n]` is the width of the **ink**. The **advance**
> is `wide + kern`, and both the layout pass and the blit say so verbatim:
>
> * `libs/ux-api/src/wordwrap.rs:76` — `self.width += (gs.wide + gs.kern) as isize;`
> * `libs/ux-api/src/wordwrap.rs:154` — `point.x += (glyph.wide + glyph.kern) as isize;`
>
> and `libs/blitstr2/src/fonts.rs:21` is `const DEFAULT_KERN: u8 = 1;`, which `mono_glyph`
> (`fonts.rs:143-164`) applies unconditionally. So a mono cell advances **8** pixels and the badge
> grid is **`128/8 = 16` columns x `128/15 = 8` rows**.
>
> (The `cursor.rs:31-34` line quoted just above — `self.pt.x += glyph.wide as isize;` — is
> `Cursor::update_glyph`, which the `TextView` path does not use for placement. `ComposedType::render`
> at `wordwrap.rs:154` is what actually positions glyphs. Two different advances exist in the tree
> and only the kerned one is on the drawing path.)
>
> Nothing in xous-core computes a column count, so there is no in-tree counter-example, and it cannot
> be queried at runtime either: `QueryGlyphProps` returns the **height only**
> (`minigfx/handlers.rs:181-191`, `scalar.arg2 = glyph_to_height_hint(glyph)`) and there is no
> `glyph_to_width_hint` at all.
>
> **Measured, not inferred.** `ux-api`'s typesetter and `blitstr2`'s blitter build for the host, so
> the real `Typesetter::typeset` + `ComposedType::render` were run into a 128x128 bit buffer:
>
> ```text
> glyph 'A': wide=7 kern=1 high=15 => advance=8    (same for '0', ' ', '/', '-')
>
> 18 chars, box 128 wide:  y0..14 x 1..=117 ; y15..29 x 1..=22   <-- wrapped, 3 spilled
> 16 chars, box 128 wide:  y0..14 x 1..=117 ; y15..29 x 1..=4    <-- wrapped, 1 spilled
> 16 chars, box 256 wide:  y0..14 x 1..=124                      <-- fits whole
> 8 rows of 16, box 256 wide x 127 high:
>     bands at y 0,15,30,45,60,75,90,105, each x 1..=124, overflow=false
> ```
>
> Two consequences for anyone laying out text here:
>
> 1. **A row that exactly fills the screen still wraps against a screen-sized bounding box.** The
>    predicates are `does_word_fit_on_line: width + cursor.x < bb.max.x` and
>    `is_word_longer_than_line: width >= bb.max.x - bb.min.x` (`wordwrap.rs:526-528`), and a full row
>    is `16 * 8 = 128`, which trips both against `bb.max.x == 128`. Give `TextBounds::BoundingBox` a
>    rectangle **wider than the screen** and let `clip_rect` bound the drawing instead;
>    `wordwrap.rs:146` drops glyphs past `clip_rect.br().x` and `op::rectangle`'s iterator
>    (`op.rs:177`) filters the fill per pixel, so the overhang is clipped, not overrun.
> 2. **Never hand the typesetter an empty first line.** `move_candidate_to_newline`
>    (`wordwrap.rs:567-575`) advances by `cursor.line_height`, which starts at **0**, so a leading
>    `\n` does not advance `y` and the whole block shifts up one row. Pad every row to at least one
>    space.
>
> `badge/app/src/oled.rs` is the working caller; its module docs carry the same derivation.

### 4d. Screen size constants for the badge

`/tmp/xous-scratch/xous-core/libs/ux-api/src/platform/baosec.rs` (complete file, verbatim):
```rust
pub const LINES: isize = 128;
pub const HEIGHT: usize = LINES as usize;
pub const WIDTH: isize = 128;
pub const FB_SIZE: usize = LINES as usize * WIDTH as usize / core::mem::size_of::<u32>();
pub const FB_WIDTH_WORDS: usize = WIDTH as usize / core::mem::size_of::<u32>();

// For passing frame buffer references
pub type FbRaw = [u32];
```
Gated at `libs/ux-api/src/platform/mod.rs:1-4` on `board-baosec | hosted-baosec | loader-baosec`.
A second, differing set at `/tmp/xous-scratch/xous-core/libs/blitstr2/src/platform/baosec.rs`:
```rust
pub const LINES: isize = 128;
pub const WIDTH: isize = 128;
pub const WORDS_PER_LINE: usize = WIDTH as usize / (core::mem::size_of::<u32>() * 8);
pub type FrBuf = [u32];
pub const FB_SIZE: usize = WORDS_PER_LINE * LINES as usize;
```
Surprise: the two `FB_SIZE` definitions disagree (the ux-api one divides by 4 instead of 32). The SH1107
driver uses the bit-correct form. Display driver:
`/tmp/xous-scratch/xous-core/libs/bao1x-hal/src/sh1107.rs:10-18`:
```rust
pub const COLUMN: isize = WIDTH;
pub const ROW: isize = LINES;
pub const PAGE: u8 = ROW as u8 / 8;
...
// 0x4f is a bit too bright
pub const DEFAULT_BRIGHTNESS: u8 = 0x3f;
```
and `sh1107.rs:423` — `pub fn screen_size(&self) -> Point { Point::new(WIDTH, LINES) }`.
At runtime, `bao-video` answers `GfxOpcode::ScreenSize` from `display.screen_size()`
(`services/bao-video/src/main.rs:925-931`), so `gfx.screen_size()` will return `Point{128,128}`.

### 4e. Real usage example

**`services/dc34-console` never constructs a `TextView`** — it only calls `Gfx::new`,
`brightness_nonblocking`, `flush` and friends. Likewise `services/bao-console` never constructs one.
The closest real, complete, 128x128-sized `Gfx` + `TextView` example in-tree is
`/tmp/xous-scratch/xous-core/services/bao-video/src/testing.rs`. Verbatim setup (lines 1-7, 22-28) and
the drawing block (lines 45-47, 58-73):

```rust
use core::fmt::Write;

use blitstr2::GlyphStyle;
use num_traits::*;
use ticktimer::Ticktimer;
use ux_api::minigfx::*;
use ux_api::service::api::*;
```
```rust
const TEST_STYLE: GlyphStyle = GlyphStyle::Tall;
pub fn tests() {
    let _ = std::thread::spawn({
        move || {
            let xns = xous_names::XousNames::new().unwrap();
            let gfx = bao_video::Gfx::new(&xns).unwrap();
            let ticktimer = Ticktimer::new().expect("Couldn't connect to Ticktimer");
```
```rust
                let screensize = gfx.screen_size().expect("Couldn't get screen size");
                let blackout = Rectangle::new_with_style(
                    Point::new(0, 0),
                    screensize,
                    DrawStyle::new(PixelColor::Dark, PixelColor::Dark, 1),
                );
                gfx.draw_rectangle(blackout).unwrap();
                gfx.flush().unwrap();

                let clipping_area = Rectangle::new_coords(5, 5, 120, 120);
                let text_bounds = Rectangle::new_coords(10, 10, 110, 110);
```
```rust
                    Some(TestType::BasicTest) => {
                        let mut tv =
                            TextView::new(Gid::new([0, 0, 0, 0]), TextBounds::BoundingBox(text_bounds));
                        tv.clip_rect = Some(clipping_area);
                        tv.style = TEST_STYLE;
                        tv.ellipsis = true;
                        tv.rounded_border = Some(4);
                        write!(tv, "This is a test of basic word wrapping ...").unwrap();
                        tv.insertion = None;
                        tv.invert = true;
                        gfx.draw_textview(&mut tv).unwrap();
                        gfx.flush().unwrap();
                    }
```
A real *app* example (baosec vault) is `/tmp/xous-scratch/xous-core/apps-baosec/vault2/src/ux.rs:289-313`,
which uses `TextBounds::CenteredTop(...)`, `tv.margin = Point::new(0,0)`, `tv.draw_border = false`,
`write!(tv, "{}", code)`, `self.gfx.draw_textview(&mut tv)`.

**Our own working caller is `badge/app/src/oled.rs` (`GfxScreen::draw`)**, and it is the one to copy
for a full-screen character grid: `TextView::new(Gid::dummy(), TextBounds::BoundingBox(...))` with the
box deliberately wider than the screen, `clip_rect` set to the screen, `style = GlyphStyle::Monospace`,
`draw_border = false`, `border_width = 0`, `margin = Point::new(0, 0)`, `invert = true` (which makes the
fill `PixelColor::Dark`, the OLED's unlit state — glyph polarity is not the caller's to choose, it is
forced on this board at `wordwrap.rs:163-169`), `clear_area = true`, `ellipsis = false`. `Gid::dummy()`
is fine: nothing on the direct-`Gfx` path validates it. See the correction in **4c** for why the box is
over-wide.

---

## 5. Build and packaging

### 5a. `cargo build` for a baosec-lite app

Verbatim from `/tmp/xous-scratch/xous-core/services/dc34-console/build.sh` (and identically from the
standalone `bunnie/dc34-console`):
```sh
cargo build --release --target riscv32imac-unknown-xous-elf --features board-baosec --features bao1x \
  --features oem-baosec-lite --features utralib/bao1x
```
`--release` is required for code density (per the dabao-console README). Output ELF lands at
`target/riscv32imac-unknown-xous-elf/release/<crate-name>`.

The in-tree equivalent for building a whole image is
`cargo xtask baosec-lite [cratespecs...]` (see §5d), where `baosec-lite` is `baosec_common()` plus
`add_feature("oem-baosec-lite")` and `add_loader_feature("oem-baosec-lite")`
(`/tmp/xous-scratch/xous-core/xtask/src/main.rs:815-818`).

### 5b. `xous-app-uf2`

Install: `cargo install xous-tools` (crate `xous-tools`,
`/tmp/xous-scratch/xous-core/tools/Cargo.toml`, `default-run = "xous-app-uf2"`, binary source
`/tmp/xous-scratch/xous-core/tools/src/bin/xous-app-uf2.rs`).

Its own self-description (`xous-app-uf2.rs:37-40`):
```
App::new("Xous Detached App UF2 Creator for Developer Images")
    .about("Create a detached app image for Xous, signed for developer images, using the latest defaults")
```

Arguments, verbatim from `xous-app-uf2.rs:41-90`:

| flag | takes value | required | help text (verbatim) |
|---|---|---|---|
| `-f`, `--elf` | yes, repeatable (`number_of_values(1)`) | **yes** | "List of ELF files to incorporate in the detached app" |
| `--antirollback` | yes, default `"1"` | no | "Anti-rollback number. Must be greater than or equal to the current anti-rollback number on the target system." (panics if > 500) |
| `--swap` | **no** (a bare flag) | no | "When specified, creates a swap image" |
| `--git-rev` | yes | no | "Explicit git commit hash for swap nonce ... If not specified, uses git rev-parse HEAD." |
| `--git-describe` | yes | no | "Explicit git describe version for swap signing ... If not specified, uses git describe." |
| `--empty-loader` | no | no | "creates a short loader file with all 0's as data. Used for resetting device state in CI testing." (emits `empty.uf2`) |

**What `--swap` does**, from the source itself:

*Without* `--swap` (`xous-app-uf2.rs:126-132, 147-150, 219-231`) — RRAM detached app, dabao layout:
```rust
        // There is no kernel in this image, so the RAM section has no meaning. Set to 0.
        let mut args = XousArguments::new(0, 0, 0);
        args.set_detached_offset(
            (bao1x_api::offsets::dabao::APP_RRAM_START - bao1x_api::offsets::KERNEL_START) as u32 - 0x1000,
        );
```
```rust
                args.add(IniF::new(init.entry_point, init.sections, init.program, init.alignment_offset));
```
```rust
        let app_uf2 = "apps.uf2";
        let uf2_blob =
            bin_to_uf2(&result, bao1x_api::BAOCHIP_1X_UF2_FAMILY, bao1x_api::dabao::APP_RRAM_START as _)?;
        ...
        println!("Created app UF2 at {}", app_uf2);
```

*With* `--swap` (`xous-app-uf2.rs:120-125, 147-149, 169-187`) — swap image, baosec layout:
```rust
    let mut args = if matches.is_present("swap") {
        XousArguments::new(
            0,
            bao1x_api::offsets::baosec::SWAP_RAM_LEN as _,
            u32::from_le_bytes(*b"Swap") as XousArgumentCode,
        )
```
```rust
                args.add(IniS::new(init.entry_point, init.sections, init.program));
```
```rust
    if matches.is_present("swap") {
        let mut swap_buffer = SwapWriter::new();
        args.write(&mut swap_buffer)?;

        // Create the swap target image and encrypt swap_buffer to it
        let mut swap = Cursor::new(Vec::new());
        swap_buffer.encrypt_to(&mut swap, &private_key, Some(anti_rollback as usize), git_rev, semver)?;

        // generate a uf2 file
        let swap_uf2 = "swap.uf2";
        let uf2_blob =
            bin_to_uf2(&swap.into_inner(), bao1x_api::BAOCHIP_1X_UF2_FAMILY, bao1x_api::SWAP_START_UF2 as _)?;
        ...
        println!("Created swap UF2 at {}", swap_uf2);
```

Summary of `--swap`, precisely:
1. Switches the section tag from `IniF` (execute-in-place from FLASH/RRAM) to **`IniS`** (swap-resident).
2. Sets the RAM section to `bao1x_api::offsets::baosec::SWAP_RAM_LEN` (`8192 * 1024`) and the argument
   code to the literal `"Swap"`.
3. Runs the blob through `SwapWriter::encrypt_to(...)` — the swap image is **encrypted**, keyed with the
   dev key and stamped with a git-rev nonce.
4. Emits **`swap.uf2`** (not `apps.uf2`) at `bao1x_api::offsets::common::SWAP_START_UF2 = 0x7000_0000`,
   UF2 family `BAOCHIP_1X_UF2_FAMILY = 0xa7d7_6373`.

Both paths sign with the built-in developer key (`DEV_KEY_PEM` at `xous-app-uf2.rs:26`, and a SLH-DSA
`DEV_KEY_PQ`). Non-swap images get a **dummy `v0.0.0` semver** deliberately (see the long comment at
`xous-app-uf2.rs:193-210`).

**For the badge you must use `--swap`.** `bao1x_api::offsets::baosec` has
`APP_RRAM_OFFSET = 0` and **`APP_RRAM_LEN = 0`** (`libs/bao1x-api/src/offsets/baosec.rs:36-38`) — there is
no on-chip app region on baosec, so `apps.uf2` has nowhere to land. dabao by contrast has
`APP_RRAM_OFFSET = 0x30_0000`, `APP_RRAM_LEN = 0xD_A000 + SIGBLOCK_LEN`
(`libs/bao1x-api/src/offsets/dabao.rs:25-27`).

**Caveat I want to flag rather than paper over:** `--swap` writes the *whole* swap image. On a stock
`baosec-lite` build that is safe, because `baosec_common()` puts every system service in FLASH
(`bao_swap_pkgs = [].to_vec();`, `xtask/src/main.rs:1280`) and leaves swap for apps only. I did not find
an out-of-tree README or CI job that actually documents the `xous-app-uf2 --swap` step for the badge —
both `bunnie/dc34-console` and `services/dc34-console` READMEs stop at `build.sh`, and
`dc34-console/.gitignore` still lists `apps.uf2` (copied boilerplate from dabao-console).

The documented dabao invocation, verbatim from `/tmp/xous-scratch/dabao-console/README.md`:
```
cargo install xous-tools
xous-app-uf2 --elf target/riscv32imac-unknown-xous-elf/release/dabao-console
```
> "This will create a file called `apps.uf2` which you can then copy into a Dabao."

### 5c. How the file gets onto the device

From `/tmp/xous-scratch/xous-core/README-baochip.md` (verbatim excerpts):

> The Baochip bootloader assumes an environment where USB is available. The device will enumerate as a
> USB mass storage device. Firmware is delivered by copying UF2 files into the drive (it is *not* usable
> as a generic storage device). Be sure to cleanly unmount or `sync` the drive before booting the device.

> Holding down the `PROG` button while plugging the device into USB will cause it to enter a bootloader
> that enumerates a mass storage device. The build artifacts can then be copied onto the device. Pressing
> `PROG` again will cause the device to run the program.

> You will need to copy all three artifacts generated (loader.uf2, xous.uf2, and apps.uf2) initially to
> ensure that the loader, kernel, and applications are at the same revision. After that point if the
> loader and kernel are not updated, one can just update apps.uf2.

Volume label is `BAOCHIP` (`ALTCHIP` when running `bao1x-alt-boot1`). Signed production images are
produced by `/tmp/xous-scratch/xous-core/baosign.ps1`; its `baosec-lite` config expects, verbatim:
```
    "baosec-lite" = @(
        @{ Image = "loader.bin"; FunctionCode = "loader" ; TargetDir = "target\riscv32imac-unknown-xous-elf\release" }
        @{ Image = "xous.img"; FunctionCode = "kernel" ; TargetDir = "target\riscv32imac-unknown-xous-elf\release" }
        @{ Image = "swap.img"; FunctionCode = "swap" ; TargetDir = "target\riscv32imac-unknown-xous-elf\release" }
        @{ Image = "bao1x-boot1.img"; FunctionCode = "boot1" ; TargetDir = "target\riscv32imac-unknown-none-elf\release" },
        @{ Image = "bao1x-alt-boot1.img"; FunctionCode = "loader" ; TargetDir = "target\riscv32imac-unknown-none-elf\release" }
    )
```
Confirming: for baosec-lite the app payload rides in **`swap.img` / `swap.uf2`**, not `apps.*`.

Serial console for interacting once running: USB CDC-ACM at **1,000,000 baud, 8N1**
(`README-consoles.md`); on Linux `screen /dev/ttyACM0 1000000`. Physical backup UART on PB13 (Rx) /
PB14 (Tx) at the same rate.

### 5d. In-tree image build (`baosec-lite`)

`xtask/src/main.rs:810-818`:
```rust
        Some("baosec") => {
            baosec_common(&mut builder)?;
        }

        Some("baosec-lite") => {
            baosec_common(&mut builder)?;
            builder.add_feature("oem-baosec-lite");
            builder.add_loader_feature("oem-baosec-lite");
        }
```
`baosec_common` (`xtask/src/main.rs:1240-1325`) sets board `board-baosec`, swap size
`bao1x_api::offsets::baosec::SWAP_RAM_LEN`, kernel features `print-panics`, `swap`, `v2p`, loader
features `swap`, `debug-print`, target `bao1x_soc`, and loads, **in this exact order**:
```rust
    // It is important that this is the first service added, because the swapper *must* be in PID 2
    builder.add_service("xous-swapper", LoaderRegion::Flash);
    // It is important that this is the second service added, as keystore *must* be in PID 3
    builder.add_service("keystore", LoaderRegion::Flash);
```
then `bao_rram_pkgs = ["xous-ticktimer", "xous-log", "xous-names", "usb-bao1x", "bao1x-hal-service",
"modals", "pddb", "bao-video"]` into Flash, then:
```rust
    for app in get_cratespecs() {
        let (name, region) = crate::builder::region_from_name(&app, LoaderRegion::Swap);
        builder.add_service(name, region);
    }
```
i.e. **your app, given as a positional cratespec, defaults to `LoaderRegion::Swap`.**
So: `cargo xtask baosec-lite dc34-console`. Notably `dc34-console` / `bao-console` are **not** in the
default package list — you must name them.

---

## 6. `cargo xtask baosec-emu`

Help text, `/tmp/xous-scratch/xous-core/xtask/src/main.rs:1078`:
```
 baosec-emu              Run user image in hosted mode but for the baosec target
```

Full definition, `/tmp/xous-scratch/xous-core/xtask/src/main.rs:362-388` (verbatim):
```rust
        Some("baosec-emu") => {
            let bao_pkgs = [
                "xous-ticktimer",
                "keystore",
                "xous-log",
                "xous-names",
                "usb-bao1x",
                "bao1x-emu",
                "bao-console",
                "modals",
                "pddb",
                "bao-video",
                "vault2",
            ];
            builder.add_feature("pddbtest");
            builder
                // hosted-baosec feature added below
                .target_hosted_baosec()
                .add_services(&bao_pkgs)
                .add_apps(&get_cratespecs());

            // safe because xtask is single-threaded - the build to setup the emulation run is strictly
            // single-threaded the read of the variable will be multi-threaded, but it will be set
            // by that point in time.
            unsafe {
                std::env::set_var("UUID", "1234567812345678123456781234567812345678123456781234567812345678");
            }
            // builder.add_feature("modal-testing");
        }
```
Differences vs. hardware `baosec-lite`: `bao1x-emu` **replaces** `bao1x-hal-service`; `bao-console`
(not `dc34-console`) is the console; `vault2` is added; no swapper, no loader, no kernel image.

`target_hosted_baosec()`, `xtask/src/builder.rs:387-395` (verbatim):
```rust
    pub fn target_hosted_baosec(&mut self) -> &mut Builder {
        self.loader = CrateSpec::None;
        self.target = None;
        self.target_kernel = None;
        self.stream = BuildStream::Release;
        self.utra_target = "hosted-baosec".to_string();
        self.run_svd2repl = false;
        self
    }
```
**No cross-compilation target** — everything builds for the host triple. Feature set comes from
`utra_target = "hosted-baosec"`.

What it runs at the end (`xtask/src/builder.rs:926-983`, condensed but the code is verbatim):
```rust
        } else if self.target.is_none() {
            // hosted mode doesn't specify a cross-compilation target!
            // throw a warning if prebuilt files are specified for hosted mode
            for item in [&self.services[..], &self.apps[..]].concat() {
                if let CrateSpec::Prebuilt(name, _, _region) = item {
                    println!("Warning! Pre-built binaries not supported for hosted mode ({})", name);
                }
            }
            ...
            let mut hosted_args = vec!["run"];
            if let BuildStream::Release = self.stream {
                hosted_args.push("--release");
            }
            hosted_args.push("--");
            for service in services_path.iter() {
                hosted_args.push(service);
            }
            // jam in any pre-built local binary files that were specified
            let binary_files_string = self.enumerate_binary_files()?;
            ...
            for f in canonicalized_paths { ... binary_files_storage.push(windows_clean_path); }
            let mut binary_files: Vec<&str> = binary_files_storage.iter().map(|s| s.as_ref()).collect();
            hosted_args.append(&mut binary_files);

            if !self.dry_run {
                let mut dir = project_root();
                dir.push("kernel");
                println!("Starting hosted mode...");
                ...
                let status = cargo(&self.cargo_configs).current_dir(dir).args(&hosted_args).status()?;
```
So it ends up running `cargo run --release -- <path-to-each-built-service-exe>...` from the `kernel/`
directory. The hosted kernel is itself a host binary that spawns each listed executable as a "process".

### Can an out-of-tree app be run under it?

**Yes, but only as a pre-built host binary path, not as a crate to build.** Argument parsing,
`xtask/src/main.rs:1114-1126` (verbatim):
```rust
fn get_cratespecs() -> Vec<String> {
    let mut cratespecs = Vec::<String>::new();
    let mut args = env::args();
    args.nth(1); // skip the verb
    for arg in args {
        if arg.starts_with('-') {
            // stop processing the list as soon as first named argument is found
            break;
        }
        cratespecs.push(arg)
    }
    cratespecs
}
```
and the cratespec grammar, `xtask/src/builder.rs:107-141` (verbatim):
```rust
impl From<&str> for CrateSpec {
    fn from(spec: &str) -> CrateSpec {
        // remote crates are specified as "name^version", i.e. "xous-names^0.9.9"
        if spec.contains('^') { ... CrateSpec::CratesIo(...) }
        // prebuilt crates are specified as "name#url"
        else if spec.contains('#') { ... CrateSpec::Prebuilt(...) }
        // local files are specified as paths, which, at a minimum include one directory separator "/" or "\"
        // i.e. "./local_file"
        else if spec.contains('/') || spec.contains('\\') {
            //optionally a BinaryFile can have a name associated with it as "name:path"
            if spec.find(':').is_some() { ... CrateSpec::BinaryFile(Some(name), path, region) }
            else { ... CrateSpec::BinaryFile(None, name, region) }
        } else { ... CrateSpec::Local(name, region) }
    }
}
```
So:
- `cargo xtask baosec-emu myapp` → `CrateSpec::Local("myapp")` → must be a **workspace member** of
  `xous-core` (the builder runs `cargo build -p myapp` in-tree). Out-of-tree crates cannot be named this way.
- `cargo xtask baosec-emu ./target/release/myapp` → `CrateSpec::BinaryFile` → **works out-of-tree**, but
  the path must be an already-built **native host executable** (there's an explicit
  `panic!("FATAL ERROR: App '{}' does not exist or is not executable.", f)` check), and `Prebuilt`
  (`name#url`) is explicitly warned against for hosted mode.

The full cratespec help text (`xtask/src/main.rs:1018-1027`, verbatim):
```
[cratespecs] is a list of 0 or more items of the following syntax:
   [name]                crate 'name' to be built from local source
   [name@version]        crate 'name' to be fetched from crates.io at the specified version
   [name#URL]            pre-built binary crate of 'name' downloaded from a server at 'URL'
   [path-to-binary]      file path to a prebuilt binary image on local machine.
                         Files in '.' must be specified as './file' to avoid confusion with local source
   [name:path-to-binary] file path to a prebuilt binary image on local machine which will be renamed.
                         This is useful if the binary image is an app since the name will be required
                         for registration with the gam.
```
(Doc/code mismatch: the help says `name@version`, the parser at `builder.rs:110` actually splits on `^`.)
`--app <cratespec>` and `--service <cratespec>` are also accepted (`xtask/src/main.rs:194-197`).

**Manifest:** `/tmp/xous-scratch/xous-core/apps/manifest.json` exists; its keys are exactly
`app-loader, ball, chat-test, hello, hidv2, mtxchat, mtxcli, repl, sigchat, transientdisk, vault`.
**`apps-baosec/manifest.json` does not exist** (a repo-wide `find -maxdepth 3 -name manifest.json` returns
only `./apps/manifest.json`; `apps-baosec/` holds only `dc34-vault/`, `vault2/`, `README.md`).
`generate_app_menus` *is* called for `baosec-emu` (`self.board == ""`, so the `board-dabao` bypass at
`builder.rs:905` doesn't apply), but unknown app names are **silently ignored**
(`xtask/src/app_manifest.rs:51-57` only inserts names it finds in the map), and the files it generates
(`services/gam/src/apps.rs`, `services/status/src/app_autogen.rs`,
`services/cram-console/src/app_autogen.rs`) belong to crates `baosec-emu` doesn't build. **So being absent
from the manifest costs you nothing under `baosec-emu`.**

Also worth knowing: `hosted-bao1x-ci` (`xtask/src/main.rs:430-448`) is the build-only twin of
`baosec-emu` with the **same package list minus `usb-bao1x`** — useful if you just want a compile check.
There is no xtask verb named `hosted` and none named `bao-emu`.

### Which syscalls are stubbed in hosted mode — the correction

**`Error::NotImplemented` does not exist.** The full `xous::Error` enum
(`/tmp/xous-scratch/xous-core/xous-rs/src/definitions.rs:117-174`) has 40 variants:
`NoError, BadAlignment, BadAddress, OutOfMemory, MemoryInUse, InterruptNotFound, InterruptInUse,
InvalidString, ServerExists, ServerNotFound, ProcessNotFound, ProcessNotChild, ProcessTerminated,
Timeout, InternalError, ServerQueueFull, ThreadNotAvailable, UnhandledSyscall, InvalidSyscall,
ShareViolation, InvalidThread, InvalidPID, UnknownError, AccessDenied, UseBeforeInit, DoubleFree,
DebugInProgress, InvalidLimit, NotFound, InvalidCoding, HardwareError, SerializationError,
InvalidArgument, NetworkError, StorageError, Unavailable, ParseError, InvalidCore, VerificationError,
SecurityError`. `grep -rn "NotImplemented" xous-rs/ kernel/src/` returns **zero hits**.

What actually happens to memory mapping in hosted mode: **`map_memory` succeeds**, but the returned
range is host-allocator memory, not kernel memory. `/tmp/xous-scratch/xous-core/xous-rs/src/arch/hosted/mem.rs:43-76`
(verbatim, complete):
```rust
pub fn map_memory_pre(
    _phys: &Option<MemoryAddress>,
    _virt: &Option<MemoryAddress>,
    _size: usize,
    _flags: MemoryFlags,
) -> core::result::Result<(), Error> {
    Ok(())
}

pub fn map_memory_post(
    _phys: Option<MemoryAddress>,
    _virt: Option<MemoryAddress>,
    size: usize,
    _flags: MemoryFlags,
    _range: MemoryRange,
) -> core::result::Result<MemoryRange, Error> {
    // let rounded_size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let layout = Layout::from_size_align(size, PAGE_SIZE).unwrap().pad_to_align();
    let mem = unsafe { alloc(layout) } as usize;

    // println!("Allocated {} bytes (requested {}) @ {:016x}", rounded_size, size, mem);
    unsafe { MemoryRange::new(mem, size) }
}

pub fn unmap_memory_pre(_range: &MemoryRange) -> core::result::Result<(), Error> { Ok(()) }

pub fn unmap_memory_post(range: MemoryRange) -> core::result::Result<(), Error> {
    // println!("Request to free {} bytes @ {:016x}", range.len(), range.as_ptr() as usize);
    let layout = Layout::from_size_align(range.len(), PAGE_SIZE).unwrap().pad_to_align();
    let ptr = range.as_mut_ptr();
    unsafe { dealloc(ptr, layout) };
    Ok(())
}
```
Note `map_memory_post` **discards the kernel's returned `range`** entirely. So a `map_memory` of a
physical peripheral address in hosted mode silently hands you zeroed heap, not MMIO — a much more
insidious failure mode than an error return.

**Per-syscall verdict for hosted mode.** The kernel dispatcher is `kernel/src/syscall.rs:732 handle_inner`,
terminating at line 1401 with `_ => Err(xous_kernel::Error::UnhandledSyscall),`. The hosted kernel is
launched as `cargo run --release --` from `kernel/`, i.e. with kernel **default features only** —
`kernel/Cargo.toml:81`: `default = ["debug-proc", "print-panics"]`. That single fact decides most rows:

| syscall | hosted behaviour | evidence |
|---|---|---|
| `MapMemory` with `phys == None` | **works** (kernel reserves a virtual range via the no-op `reserve_address`; client then discards the result and `alloc`s from the host heap) | `kernel/src/syscall.rs:770`, `kernel/src/arch/hosted/mem.rs:36-43`, `xous-rs/src/arch/hosted/mem.rs:52-65` |
| `MapMemory` with `phys == Some(addr)` (MMIO) | **panics the kernel** — `kernel/src/mem.rs:729` calls `crate::arch::mem::map_page_inner(...)` which is `unimplemented!()`; `kernel/src/syscall.rs:785` then calls `hand_page_to_user`, also `unimplemented!()` | `kernel/src/arch/hosted/mem.rs:57`, `:96` |
| `MapMemory` with `MemoryFlags::DEV`, `phys == 0` | same panic (the `device_ram` phys-resolution branch is `#[cfg(baremetal)]`) | `kernel/src/mem.rs:618, 704-745` |
| `UnmapMemory` | works, no-op (`unmap_page_inner` returns `Ok(virt)`); alignment check is `cfg!(baremetal)`-gated | `kernel/src/syscall.rs:793-808`, `arch/hosted/mem.rs:94` |
| `IncreaseHeap` / `DecreaseHeap` | work | `kernel/src/syscall.rs:809-874` |
| `AdjustProcessLimit` | **works** — pure bookkeeping, no arch dependency | `kernel/src/syscall.rs:1043-1057` |
| `UpdateMemoryFlags` | works, no-op | `kernel/src/syscall.rs:1034-1042`, `arch/hosted/mem.rs:102` |
| `ClaimInterrupt` / `FreeInterrupt` | **panic the kernel** via `arch::irq::enable_irq` / `disable_irq` | `kernel/src/arch/hosted/irq.rs` (entire file is three `unimplemented!()`s) |
| `VirtToPhys`, `VirtToPhysPid` | `Err(UnhandledSyscall)` — arms are `#[cfg(feature = "v2p")]`, not enabled | `kernel/src/syscall.rs:1058-1073` |
| `RegisterSwapper` | `Err(UnhandledSyscall)` — `#[cfg(feature = "swap")]`, not enabled | `kernel/src/syscall.rs:1074-1077` |
| **`SwapOp` (all `SwapAbi` ops incl. `GetFreePages`)** | `Err(UnhandledSyscall)` — whole arm is `#[cfg(feature = "swap")]`; it also uses `riscv::register::sstatus`, so it could not compile on x86 regardless | `kernel/src/syscall.rs:1078-1215` |
| `RawTrng` | `Err(UnhandledSyscall)` — `#[cfg(feature = "raw-trng")]` | `kernel/src/syscall.rs:1216-1227` |
| `PlatformSpecific` | **panics** — `#[cfg(not(feature = "bao1x"))] => unimplemented!("No platform specific calls for this platform")` | `kernel/src/syscall.rs:1396-1399` |
| `SetExceptionHandler` | `Err(UnhandledSyscall)` — implementation is commented out, ref. xous-core issue #90 | `kernel/src/syscall.rs:1401-1408` |
| `CreateProcess` from a client | client-side `create_process_pre` is `unimplemented!()`; hosted processes are instead spawned by the kernel as OS subprocesses with `XOUS_SERVER`/`XOUS_PID`/`XOUS_PROCESS_KEY` env vars | `xous-rs/src/arch/hosted/process.rs:113-118, 120+` |

Hosted syscalls are not traps at all: they are length-prefixed messages over a TCP socket to the kernel
process, with a 5 ms sleep-retry loop on `Result::RetryCall` (`xous-rs/src/arch/hosted/mod.rs:132-184`).

What *is* genuinely `unimplemented!()` in hosted mode (kernel side,
`/tmp/xous-scratch/xous-core/kernel/src/arch/hosted/mem.rs`) — these panic if reached:
- `MemoryMapping::get_pid` (line 21)
- `map_page_inner` (line 57)
- `move_page_inner` (line 68)
- `lend_page_inner` (line 80)
- `return_page_inner` (line 91)
- `hand_page_to_user` (line 96)

and these are no-ops / trivially true:
- `MemoryMapping::activate` → `Ok(())`, `allocate` → `Ok(())`, `reserve_address` → `Ok(())`
- `address_available(_virt) -> bool { true }`
- `unmap_page_inner(_mm, virt) -> Ok(virt)`
- `virt_to_phys(virt) -> Ok(virt)` (identity!)
- `page_flags(_virt) -> None`
- `update_page_flags(..) -> Ok(())`

Client side, the only `unimplemented!()` in all of `xous-rs/src/arch/hosted/` is at
`xous-rs/src/arch/hosted/process.rs:117`.

Practical consequence for the badge app: **USB serial is completely absent under `baosec-emu`.**
`services/usb-bao1x/src/main_hosted.rs` (163 lines) handles only FIDO/U2F, `SendKeyCode`, `GetLedState`,
`SetBlockDevice*`, `IsSocCompatible`, core switching, and `Quit`. **None of `SerialHookAscii`,
`SerialHookBinary`, `SerialHookConsole`, `SerialClearHooks`, `SerialFlush`, `SerialSendData`,
`SerialSendDataBlocking`, `LogString` are handled.** They fall through to
`_ => log::warn!("Opcode not supported: {:?}", msg),` (`main_hosted.rs:149`). So
`serial_console_input_injection()` is a silent no-op and `serial_send()` will come back with
`sent == None` → `Err(xous::Error::InternalError)`.

Keyboard *is* emulated: `services/bao1x-emu/src/keyboard.rs:39` registers `SERVER_NAME_KBD`, so a
keyboard-listener architecture (§3A) still works under `baosec-emu` — driven by the emulator window, not
by serial. Graphics also work: `bao-video` is in the `baosec-emu` package list and provides `_Graphics_`.

Swap is **not** in the `baosec-emu` package list (`xous-swapper` absent), and the hosted kernel has no
`swap` feature, so `SysCall::SwapOp` returns `Err(UnhandledSyscall)` there.

### What `services/bao1x-emu` actually is

It is **both a service binary and a library**, and it is a drop-in replacement for
`bao1x-hal` + `bao1x-hal-service` on the host. `services/bao1x-emu/src/lib.rs` in full:
```rust
pub mod camera;
pub mod display;
pub mod i2c;
pub mod keyboard;
pub mod trng;
pub mod udma;
```
As a service (`services/bao1x-emu/src/main.rs:11-18`) it registers `bao1x_api::SERVER_NAME_BAO1X_HAL`
— impersonating `bao1x-hal-service` — plus starts the keyboard and susres services. Its `MapIfram`
handler returns a **fake address `0xDEAD_BEEF`** (`main.rs:29-37`), which is the direct workaround for
hosted `map_memory` being unable to map MMIO. `SetPreemptionState` is `todo!()` and everything else is
`unimplemented!("Not available in hosted mode")` (`main.rs:97-106`).

- **Graphics: yes** — `src/display.rs` (410 lines) is a `minifb` desktop window implementing
  `Oled128x128` + `impl FrameBuffer`, with `MainThreadToken` / `claim_main_thread` (Cocoa needs the GUI
  loop on TID 1). `DARK_COLOUR = 0x161616`, `LIGHT_COLOUR = 0xC5C5BD`, `MAX_FPS = 60`.
  Dep: `[target.'cfg(any(windows,unix))'.dependencies] minifb = "0.26.0"`.
- **Keyboard: yes** — registers `SERVER_NAME_KBD` plus a private `b"keyboard_bouncer"` server.
- **TRNG: yes** — `rand::thread_rng().gen()`.
- **Camera / I2C / UDMA: stubs** — I2C always ACKs, UDMA IRQ status is always 0, camera init/poke/peek
  are empty.
- **USB: no.** `bao1x-emu` contains no USB emulation at all; hosted USB lives in
  `usb-bao1x/src/main_hosted.rs`, gated on `cfg(not(target_os = "xous"))` rather than a feature.

The swap idiom, verbatim from `/tmp/xous-scratch/xous-core/services/bao-video/src/gfx.rs:1-4`:
```rust
#[cfg(feature = "hosted-baosec")]
use bao1x_emu::display::Mono;
#[cfg(feature = "board-baosec")]
use bao1x_hal::sh1107::Mono;
use ux_api::minigfx::*;
```
Maintainers' own note, `/tmp/xous-scratch/xous-core/README-baochip.md`:
> `baosec-emu`: xtask target for hosted mode emulation for `baosec`. `bao1x-emu` contains hosted mode
> shims for the `baosec` target. `bao1x-emu` mis-named and should probably be renamed to `baosec-emu`.

Your app's `hosted-baosec` feature therefore needs to look like dc34-console's
(`Cargo.toml`, verbatim):
```toml
hosted-baosec = [
    "modals/hosted-baosec",
    "usb-bao1x/hosted-baosec",
    "bao1x-emu",
    "pddb/hosted-baosec",
    "bao1x-hal/hosted-baosec",
]
```

---

## 7. Quieting the log server

First, the thing that makes this non-obvious: **`println!` and `log::` share the same wire.** The log
server mirrors *both* to USB when the mirror is hooked. From
`/tmp/xous-scratch/xous-core/services/xous-log/src/main.rs`:

- `api::Opcode::LogRecord` (log:: traffic) → formatted, then at lines 120-146
  `#[cfg(feature = "usb")] if let Some(conn) = usb_serial { ... usb_send_str(conn, &usb_str); }`
- `api::Opcode::StandardOutput | api::Opcode::StandardError` (`println!`) → at lines 178-185
  `#[cfg(feature = "usb")] if let Some(conn) = usb_serial { ... usb_send_str(conn, &s.replace("\n", "\r\n")); }`

`usb_send_str` (`xous-log/src/main.rs:35-44`, verbatim) — note the raw opcode `8192`:
```rust
#[cfg(feature = "usb")]
fn usb_send_str(conn: xous::CID, s: &str) {
    let serializer = UsbString { s: String::from(s), sent: None };
    match xous_ipc::Buffer::into_buf(serializer) {
        Ok(buf) => {
            // failures to send are silent & ignored; also, this API doesn't block.
            buf.send(conn, 8192 /* LogString */).ok();
        }
        _ => {} // dont block on errors
    }
}
```
matching `services/usb-bao1x/src/api.rs:107` — `LogString = 8192`.

The log server's own opcodes (`/tmp/xous-scratch/xous-core/api/xous-api-log/src/api.rs:29-45`):
```rust
pub enum Opcode {
    /// A `LogRecord` message, delivering structured log output
    LogRecord = 0,
    /// A `&[u8]` destined for stdout
    StandardOutput = 1,
    /// A `&[u8]` destined for stderr
    StandardError = 2,
    /// A `xous::StringBuffer` containing this program's name
    ProgramName = 3,
    /// Try to log console output to a USB serial port. Best-effort only; failures will not crash, will not
    /// be noted
    TryHookUsbMirror = 4,
    UnhookUsbMirror = 5,
    ...
```
There is **no** "set log level" opcode on the log server. It does no filtering of its own.

### Four mechanisms, in order of how well they solve your problem

**(1) Take the port away from the log server entirely — the clean answer.**
```rust
let usb = usb_bao1x::UsbHid::new();
usb.serial_clear_input_hooks();          // sends UnhookUsbMirror to the log server AND sets NoListener
// output: usb.serial_send(b"...")       // bypasses the log server completely
// input:  usb.serial_wait_ascii(Some('\n'))  or  usb.serial_wait_binary()
```
`serial_clear_input_hooks` → `Opcode::SerialClearHooks` → `UnhookUsbMirror` to `xous-log-server `
(source quoted in full in §3). After this, `log::` output still goes to the *hardware* UART (PB13/PB14 at
1 Mbaud) but no longer to USB CDC. Cost: you also give up console key-injection, so you must poll
`serial_wait_ascii` yourself. Warning from `main.rs:23-28`: with `NoListener` the RX buffer just grows —
`AsciiListener`/`BinaryListener` "will 'infinitely' buffer incoming characters if no listener is hooked".

**(2) Lower your own process's level.** `log::set_max_level(log::LevelFilter::Error)` (or `Off`) right
after `log_server::init_wait().unwrap()`. `log::set_max_level` is a per-process static, so it silences
only your process. Every other service does its own `log::set_max_level(log::LevelFilter::Info)` in its
own `main()` (dozens of call sites, e.g. `services/xous-swapper`, `services/pddb`, `services/bao-video`).
Note `xous_api_log::init()` and `init_wait()` both **hard-code `log::set_max_level(log::LevelFilter::Info)`**
(`api/xous-api-log/src/lib.rs:86` and `:98`) — so you must set your level *after* calling them, which is
exactly what every in-tree `main()` does.

**(3) Quiet the noisiest neighbour at runtime.** `usb_bao1x::UsbHid::set_log_level(LogLevel)`
(`services/usb-bao1x/src/lib.rs:409`, handled at `services/usb-bao1x/src/main.rs:582-590`) sets the level
*inside the usb-bao1x server process*:
```rust
            Opcode::SetLogLevel => msg_scalar_unpack!(msg, level_code, _, _, _, {
                let level = LogLevel::try_from(level_code).unwrap_or(LogLevel::Info);
                match level {
                    LogLevel::Trace => log::set_max_level(log::LevelFilter::Trace),
                    LogLevel::Info => log::set_max_level(log::LevelFilter::Info),
                    LogLevel::Debug => log::set_max_level(log::LevelFilter::Debug),
                    LogLevel::Warn => log::set_max_level(log::LevelFilter::Warn),
                    LogLevel::Err => log::set_max_level(log::LevelFilter::Error),
                }
            }),
```
`LogLevel { Trace=0, Debug=1, Info=2, Warn=3, Err=4 }` (`services/usb-bao1x/src/api.rs:224-230`).
Equivalent per-server APIs exist for `net` (`services/net/src/lib.rs:66 set_debug_level`) and
`gam` (`services/gam/src/lib.rs:497 set_debug_level`) but I found **none** for the swapper, pddb,
bao-video, ticktimer, or names.

**(4) Build-time.** `services/xous-log/Cargo.toml` features (verbatim):
```toml
lcd-console = []
debugprint = []  # adding this allocates the UART for debugging the logger
logging = []     # adding this allocates the hardware UART for console interactions
usb = ["rkyv"]
#default = []
default = ["logging", "usb"]
# default = ["debugprint", "logging"]
# when activated, disables the console so the kernel GDB stub can claim it.
gdb-stub = []
```
Dropping the `usb` feature removes the USB mirror path from the log server entirely (all the
`#[cfg(feature = "usb")]` blocks above vanish) — but that also kills `println!`-to-USB, so only do this in
combination with (1). `gdb-stub` disables the console outright.
Kernel-side noise is controlled by xtask kernel features: `baosec_common` enables `print-panics`, and
`debug-swap`, `debug-print`, `debug-swap-verbose` are present but **commented out**
(`xtask/src/main.rs:1300-1302`). Watch out for `SwapAbi::GetFreePages`, which unconditionally prints a
per-PID RAM table to the kernel console (§2e).

---

## What I could NOT find

Listed because absence is information.

1. **`Error::NotImplemented` — does not exist anywhere in xous-core.** The claim that hosted-mode memory
   mapping "returns `NotImplemented`" is not supported by the source at this revision. Hosted `map_memory`
   *succeeds* and hands back host-allocator memory (§6). I could not find any syscall in hosted mode that
   returns a distinguishable "not implemented" error; the failures are `unimplemented!()` panics
   (kernel-side page primitives) or silent semantic no-ops.

2. **`GetFreePages` as an app-callable API — does not exist.** It is `SysCall::SwapOp(SwapAbi::GetFreePages …)`,
   gated to `SWAPPER_PID` with `Error::AccessDenied` for anyone else, requires `xous/swap` feature, and
   `SwapAbi` is deliberately not published in `xous-rs`. The nearest app-reachable substitute is
   `xous_swapper::Swapper::garbage_collect_pages(n) -> usize` (free page count). I found **no** app-facing
   API that returns *total* pages.

3. **No width counterpart to `glyph_to_height_hint`.** `GfxOpcode::QueryGlyphProps` returns height only.
   There is no `glyph_to_width_hint`, no `Gfx::glyph_width_hint`, and no per-style width constant. You must
   either use `GlyphStyle::Monospace` (all widths are 7) or reach into `blitstr2::style_glyph(...).wide`
   per character. I did not find a public re-export of `style_glyph` through `ux-api`.

4. **No `TextView` usage in `services/dc34-console`.** I grepped the whole crate — it constructs `Gfx` at
   `src/main.rs:49`, `src/power.rs:169`, `src/cmds/test.rs:178`, `src/cmds/test.rs:699` but never a
   `TextView`. Same for `services/bao-console`. My §4e example is from `services/bao-video/src/testing.rs`
   (128x128, correct platform, but it is test code) and `apps-baosec/vault2/src/ux.rs` (a real app).

5. **No documented out-of-tree packaging step for the badge.** Neither `bunnie/dc34-console/README.md`
   (which is an archival notice) nor `services/dc34-console/README.md` mentions `xous-app-uf2` at all. My
   §5b conclusion that you need `--swap` is derived from source (`baosec::APP_RRAM_LEN == 0`,
   `baosign.ps1`'s `baosec-lite` config listing `swap.img`), **not** from documentation. Verify before
   relying on it.

6. **No `Gfx` string-drawing convenience helper.** There is no `draw_str`, no `msg`, no
   `print_at` — every string goes through `TextView` + `draw_textview`.

7. **No `bunnie/xous-core` branch relevant to the badge**, and no `board-dc34` feature anywhere in
   `betrusted-io/xous-core@dev`. Confirmed by full branch enumeration and by
   `grep -rn "dc34" --include='*.toml'` (only `Cargo.toml` workspace members, `libs/dc34-api`,
   `services/dc34-console`, `apps-baosec/dc34-vault`).

8. **No runtime log-level control for the swapper, pddb, bao-video, ticktimer, or xous-names.** Only
   `usb-bao1x`, `net`, and `gam` expose one. And the log server itself has no filtering opcode at all.

9. **No `apps-baosec/manifest.json`, and no xtask verb `hosted` or `bao-emu`.** Only `apps/manifest.json`
   exists, and it governs Precursor GAM menu codegen only.

10. **`Gfx::screen_size()` is not const-checkable at compile time from an app** — the constants live in
   `ux_api::platform::*` behind the `board-baosec` feature (which the badge app does enable transitively via
   `ux-api/board-baosec`), so `ux_api::platform::WIDTH`/`LINES` should be usable, but I did not find an app
   in-tree that imports them directly rather than calling `screen_size()`.
