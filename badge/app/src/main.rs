//! `rv64-badge`: the badge end of the tethered emulator.
//!
//! This file is a **platform leaf**. Everything it does is a syscall or a
//! constructor for something that makes syscalls; every decision — the slice
//! length, the frame count, the DTB derivation, what happens on a link fault,
//! what goes on the screen — is in [`badge_app::run`], above the `cfg`, where
//! `tests/dry_run.rs` boots the real guest through it on a laptop.
//!
//! That split is not tidiness. Every hardware-only defect in Task 6 was a
//! policy decision written below a `#[cfg(target_os = "xous")]`, and each one
//! cost a flash-and-photograph cycle. If you find yourself adding an `if` here,
//! it belongs in `run.rs`.
//!
//! # The startup order, which is load-bearing
//!
//! **`ticktimer` → `names` → `gfx` + paint → `log` → `usb` → `mirror` → run.**
//!
//! The full reasoning, with the hardware run that paid for each constraint, is
//! the comment block at the top of [`main`]. It is the most valuable thing in
//! this file: three cycles have been lost to this ordering, one per run, and
//! all three were the same shape — a dependency that is invisible because it
//! lives inside somebody else's library.
//!
//! In brief: the ticktimer is first because **`std` itself needs it**
//! (`std::thread::sleep` and `Instant::now()` connect to it and panic if it is
//! absent, so any library reached before it can die inside std); the screen is
//! as early as it can be after that, because until it exists a failure is a
//! badge sitting on the loader's graphic saying nothing anywhere; and every
//! step after it writes its stage line *before* the call that could hang.
//!
//! **No path here returns.** Every failure ends in `park_forever`, because a
//! process that exits takes its screen with it and becomes indistinguishable
//! from one that never started. What each failure says is
//! [`badge_app::startup::Halt`], above the `cfg`, where a test asserts that no
//! variant can halt without saying anything.
//!
//!
//! **It does not call `serial_console_input_injection()`.** That call reaches
//! `usb-bao1x`'s `SerialHookConsole` handler, which forwards `TryHookUsbMirror`
//! to the log server *and* sets `serial_listen_mode = ConsoleListener` — a mode
//! in which arriving bytes are injected into the keyboard server as keystrokes
//! and then `serial_buf.clear()`ed. That destroys page traffic. Its name
//! suggests it takes console input; what it does is take the port away from
//! this app.
//!
//! **It does not call `serial_clear_input_hooks()` either**, which the plan's
//! Task 8 text proposed. That call *unhooks the mirror*, which is the one
//! channel that carries a panic off this badge. The probe's docs record the
//! same thing: "The probe must never call `serial_clear_input_hooks()`, which
//! unhooks the mirror; an earlier revision did, which is why every failure used
//! to look like silence." Nothing needs to be cleared: the listen mode starts
//! at `NoListener` and `UsbTransport::new` primes it to `BinaryListener` by
//! parking once before it returns.
//!
//! The mirror and this transport coexist — proven on hardware by the probe, and
//! argued from the sources in `usbhost.rs`'s module docs. The one real
//! interaction is that mirrored text shares the CDC *transmit* endpoint with
//! request frames and occasionally splits one; the host's decoder drops the
//! split frame on its CRC and the transport re-sends it. The cost is counted in
//! `Report::retries`.

#[cfg(not(target_os = "xous"))]
fn main() {
    // Built for the badge; there is nothing to run here. The laptop's entry
    // point into the same code is the dry run, which boots the real guest
    // against `rv64-host serve` with only the two platform leaves swapped.
    eprintln!(
        "rv64-badge targets riscv32imac-unknown-xous-elf.\n\
         On a laptop, run the integrated boot instead:\n\
         \n    cargo test --release --test dry_run -- --nocapture\n\n\
         (inside `nix develop`, which sets GUEST_KERNEL/GUEST_DTB/GUEST_INITRAMFS)"
    );
    std::process::exit(1);
}

#[cfg(target_os = "xous")]
fn main() {
    badge::main()
}

#[cfg(target_os = "xous")]
mod badge {
    use badge_app::oled::OledSink;
    use badge_app::run::{self, Config, Console, Mirrored, FRAMES};
    use badge_app::startup;
    use badge_app::usbhost::UsbTransport;

    /// `log_server::api::Opcode::TryHookUsbMirror`, log server opcode 4
    /// (`services/xous-log/src/main.rs:250-288`). Named through the crate
    /// rather than as a literal so a renumbering upstream is a compile error.
    fn hook_panic_mirror() -> Option<usize> {
        let sid = xous::SID::from_bytes(b"xous-log-server ")?;
        let conn = xous::connect(sid).ok()?;
        match xous::send_message(
            conn,
            xous::Message::new_blocking_scalar(
                log_server::api::Opcode::TryHookUsbMirror as usize,
                0,
                0,
                0,
                0,
            ),
        ) {
            // 1 when the mirror is established, 0 when the log server could not
            // reach the USB driver. The probe's `try_hook_panic_mirror` checks
            // the same value for the same reason: an unchecked hook is a
            // transcript that is silent for a reason nobody can see.
            Ok(xous::Result::Scalar1(v)) => Some(v),
            _ => None,
        }
    }

    /// Is a server registered under this well-known SID yet?
    ///
    /// `xous::try_connect`, deliberately, and **not** `xous::connect`. The
    /// kernel's `SysCall::Connect` handler retries on `ServerNotFound`
    /// (`kernel/src/syscall.rs:990-999`), so a blocking connect to a server
    /// that never registers is a process wedged inside a syscall with nothing
    /// on the screen and nothing on the wire. `TryConnect` returns the error
    /// and lets [`startup::wait_for`] own the waiting, the bound and the
    /// report.
    ///
    /// This is a bare kernel syscall against a well-known SID: it touches no
    /// other server, allocates nothing, and has no side effects. That is what
    /// makes it safe to poll, and it is the distinction from the *named*-server
    /// probe this file used to have and no longer does — see [`main`].
    fn sid_is_up(name: &[u8; 16]) -> bool {
        match xous::SID::from_bytes(name) {
            Some(sid) => xous::try_connect(sid).is_ok(),
            None => false,
        }
    }

    /// Stops, without exiting, so whatever is on the screen stays there and the
    /// process stays visible.
    ///
    /// **Every failure path in this file ends here.** A `return` from `main`
    /// takes the screen with it and leaves a badge indistinguishable from one
    /// that never started — which has now cost two hardware cycles, the second
    /// time as a literal silent `return` on the path where the screen was the
    /// thing that was missing. If there is nothing to say it on, that *is* the
    /// finding, and a finding is only observable if the process is still there
    /// to be observed.
    ///
    /// `std::thread::sleep` needs the ticktimer, which is one of the things
    /// that can be missing here, hence the yield fallback.
    fn park_forever(tt_up: bool) -> ! {
        loop {
            if tt_up {
                std::thread::sleep(std::time::Duration::from_secs(60));
            } else {
                xous::yield_slice();
            }
        }
    }

    /// Reports a [`startup::Halt`] on every channel that exists, then parks.
    ///
    /// `screen` is `None` when there is no screen — which is itself the thing
    /// worth reporting, and the case the previous cut handled by returning
    /// silently. *What* to say is `Halt`'s decision, above the `cfg`, where a
    /// test asserts no variant can halt without saying anything.
    fn stop<C: Console>(screen: Option<&mut C>, halt: startup::Halt, tt_up: bool) -> ! {
        log::error!("rv64: {}", halt.long());
        let short = halt.short();
        if let (Some(s), false) = (screen, short.is_empty()) {
            run::note(s, &short);
        }
        park_forever(tt_up)
    }

    pub fn main() {
        // ===============================================================
        // THE STARTUP ORDER. Read this before moving anything.
        // ===============================================================
        //
        // Three hardware cycles have been lost here, one per run, and all three
        // were the same shape: **a dependency that is invisible because it is
        // inside somebody else's library.** The order below is not a preference;
        // each step is where it is because it cannot be anywhere earlier, and
        // every constraint below was paid for.
        //
        //   1. ticktimer   `std` itself needs it. `std::thread::sleep` and
        //                  `Instant::now()` connect to `ticktimer-server`
        //                  internally and panic if it is absent, so *any* std
        //                  timing call made before this dies inside std, in a
        //                  file we did not write. It is uniquely cheap to wait
        //                  for: a well-known SID, so it needs no name server,
        //                  and `try_connect` is a bare kernel syscall that
        //                  touches nothing else. Nothing can precede it, and
        //                  nothing needs to.
        //                  [run 3: died in std's `xous.rs` because the display
        //                  came first and the libraries reaching it are
        //                  ordinary std code]
        //
        //   2. names       Needed to reach any server registered by name, which
        //                  the display is. `XousNames::new()` is `xous::connect`
        //                  on a well-known SID, which the kernel retries until
        //                  the server registers, so it waits rather than
        //                  failing.
        //
        //   3. gfx + paint The first channel that can report anything. Until it
        //                  exists a failure is a badge sitting on the loader's
        //                  graphic with nothing on screen and nothing on the
        //                  wire. It is third because it cannot be second, and
        //                  second because it must not be later.
        //                  [run 2: five bounded waits ran ahead of it and one
        //                  did not resolve; the badge said nothing anywhere]
        //
        //   4. log         Bounded, because `init_wait()` and the mirror hook
        //                  both do a *blocking* connect to its SID. Not fatal:
        //                  the screen is already a channel.
        //
        //   5. usb         Via the call the real client makes. See below.
        //
        //   6. mirror      Only after names *and* usb, because the log server's
        //                  `TryHookUsbMirror` handler opens with a blocking
        //                  `xous::connect(b"xous-name-server")` — hooking it
        //                  earlier wedges the log server inside its own message
        //                  loop, and every process that logs blocks behind it.
        //
        //   7. transport, machine, run.
        //                  [run 1: `Ticktimer::new().expect()` inside
        //                  `UsbTransport::new` fired because nothing had waited
        //                  for step 1]
        //
        // Two standing rules that fall out of the above:
        //
        // * **Every step writes its stage line before the call that could
        //   hang**, so a badge stuck showing `usb...` names the call it is
        //   stuck in.
        // * **Nothing returns.** Every failure path parks, because a process
        //   that exits takes its screen with it and is indistinguishable from
        //   one that never started. What each failure says is
        //   `startup::Halt`, above the `cfg`, where a test asserts that no
        //   variant can halt without saying something.

        // ---- 1. ticktimer -------------------------------------------------
        //
        // Before this line, nothing may allocate a `Duration`, sleep, or read a
        // clock — and nothing here does. The wait is `wait_for` (pure integer
        // logic and two closures), `sid_is_up` (`SID::from_bytes` plus
        // `xous::try_connect`, both bare syscalls) and `xous::yield_slice`
        // (one more). No `std::time`, no `std::thread::sleep`, no allocation:
        // `Startup::record`, which does allocate, runs only after the wait has
        // returned. If a helper is ever added to this loop that reaches for std
        // timing, the panic moves here rather than disappearing.
        //
        // `Clock::None` is exactly the case `startup` was built for, and this
        // is now the only wait that runs without a clock.
        let tt_wait = startup::wait_for(
            || sid_is_up(b"ticktimer-server"),
            &mut startup::Clock::None,
            xous::yield_slice,
            startup::WaitLimits::without_a_clock(),
        );
        if tt_wait.is_err() {
            // No screen, no log server, no clock. `park_forever(false)` yields
            // rather than sleeping for precisely that reason, and `log::error!`
            // with no logger installed is a harmless no-op that costs one
            // `format_args`.
            stop::<Mirrored<badge_app::oled::BadgeOled>>(
                None,
                startup::Halt::NoTicktimer,
                false,
            );
        }

        // ---- 2. names -----------------------------------------------------
        //
        // Blocking and unbounded, deliberately: this is the pair that reached
        // the screen on real hardware, and a bounded probe in front of it would
        // be a second code path that never has. Its internal `.expect` is on a
        // kernel connect the kernel retries, so it is unreachable in practice.
        let xns = xous_names::XousNames::new().expect("no name server");

        // ---- 3. gfx, and the first paint ----------------------------------
        //
        // `OledSink::new` → `Gfx::new` → `request_connection_blocking`, which
        // parks the request inside the name server until `_Graphics_` registers
        // (`services/xous-names/src/main.rs:444-455`). It paints the banner
        // before it returns, so this is the first paint and everything after it
        // is diagnosable from a photograph.
        //
        // The residual risk, stated rather than hidden: if `_Graphics_` never
        // registers this waits forever with the loader's graphic still up and
        // no mirror hooked, so there is nothing on the wire either. That risk
        // was present in the build that worked, and the alternative — polling
        // `Opcode::Lookup` — is what replaced a working screen with silence.
        let mut oled = match OledSink::new(&xns) {
            Ok(o) => o,
            Err(e) => stop::<Mirrored<badge_app::oled::BadgeOled>>(
                None,
                startup::Halt::NoScreen(format!("{e:?}")),
                true,
            ),
        };

        // ---- 4. log -------------------------------------------------------
        //
        // A clock exists now, so this and everything after it get a real
        // deadline and a sleeping backoff that does not take CPU from the
        // services being waited for.
        let tt = ticktimer::Ticktimer::new().ok();
        let mut clock_ms = || tt.as_ref().map(|t| t.elapsed_ms()).unwrap_or(0);
        let mut clock = startup::Clock::Ms(&mut clock_ms);
        let limits = startup::WaitLimits::with_a_clock();
        let backoff = || std::thread::sleep(std::time::Duration::from_millis(startup::POLL_MS));

        let mut startup = startup::Startup::new();
        // Recorded now rather than when it happened: `Startup` allocates, and
        // nothing allocates before the ticktimer is confirmed.
        let _ = startup.record("tt", tt_wait);
        run::note(&mut oled, &startup.deps()[0].line());

        run::note(&mut oled, "log...");
        let log_up = startup
            .record(
                "log",
                startup::wait_for(
                    || sid_is_up(b"xous-log-server "),
                    &mut clock,
                    backoff,
                    limits,
                ),
            )
            .is_ok();
        if log_up {
            log_server::init_wait().ok();
            log::set_max_level(log::LevelFilter::Info);
        }
        run::note(&mut oled, &startup.deps()[1].line());
        log::info!("rv64: startup {} (tt confirmed before names/gfx)", startup.summary());

        // ---- 5. usb, via the call the real client makes --------------------
        //
        // Run 2's probe used `xns.request_connection(name)` where the real
        // client uses `request_connection_blocking`: `Opcode::Lookup`
        // (`api/xous-api-names/src/lib.rs:127`) against `Opcode::BlockingConnect`
        // (`:148`), two different name-server code paths that can disagree.
        // `Lookup` also calls `xous::create_server_id()` on *every miss*
        // (`services/xous-names/src/main.rs:489`) — a TRNG draw per poll —
        // while `BlockingConnect` parks the request in `waiting_connections`
        // and answers it the moment the server registers.
        //
        // The general rule is worth more than the instance: **a probe is only
        // worth having if it predicts the real call.** A lighter-weight one
        // that "should" agree is a second code path, and avoiding an untested
        // code path was the entire point of probing.
        //
        // So there is no probe. `UsbHid::new()` blocks, and the `usb...` line
        // above it is the report.
        run::note(&mut oled, "usb...");
        let usb = usb_bao1x::UsbHid::new();
        // An optimisation, not a safety measure: every line `usb-bao1x` does
        // not log is one that cannot split a request frame on the shared
        // transmit endpoint. It cannot quiet any other process, and the
        // noisiest is the swapper.
        usb.set_log_level(usb_bao1x::LogLevel::Err);
        run::note(&mut oled, "usb ok");

        // Now, and not before. The log server's `TryHookUsbMirror` handler
        // opens with a **blocking** `xous::connect(b"xous-name-server")`
        // (`services/xous-log/src/main.rs:254`), so hooking the mirror before
        // the name server exists wedges the log server inside its own message
        // loop — and every process that logs blocks behind it. The previous cut
        // hooked it first thing, before waiting for anything at all. Names and
        // USB are both up here, and the handler's own USB lookup is a
        // *TryConnect* (opcode 7, `:271`), so nothing in it can block now.
        run::note(&mut oled, "mirror...");
        match hook_panic_mirror() {
            Some(1) => run::note(&mut oled, "mirror ok"),
            other => {
                // Not fatal: the run is still worth attempting, and the screen
                // is still a channel. But say so, because from here a panic is
                // invisible and only the corner spinner separates a panic from
                // a wedge.
                log::error!("rv64: panic mirror NOT hooked ({other:?})");
                run::note(&mut oled, "mirror BAD");
            }
        }

        // Blocks until its reader thread has parked once, which is what flips
        // the listen mode to `BinaryListener`. Until that flip, arriving bytes
        // are cleared rather than queued. Its own `Ticktimer::new()` is safe
        // now: `tt` above proved the server is registered.
        run::note(&mut oled, "link...");
        let transport = UsbTransport::new(usb);
        run::note(&mut oled, "link ok");

        run::note(&mut oled, "boot...");
        // `Mirrored` so the guest console reaches the serial transcript as
        // `ConOut` frames as well as the screen. Eight rows of sixteen is the
        // photograph; it is not a log, and a kernel oops is longer than it.
        let mut machine = match run::assemble(transport, Mirrored::new(oled), FRAMES) {
            Ok(m) => m,
            Err(e) => {
                // `assemble` took the sink, so there is no screen left to write
                // to. The mirror is the only channel — which is exactly what it
                // was hooked for — and this parks rather than returning, so the
                // badge does not simply vanish.
                stop::<Mirrored<badge_app::oled::BadgeOled>>(
                    None,
                    startup::Halt::CannotStartGuest(e.describe()),
                    true,
                );
            }
        };

        // `max_insns` is unbounded on purpose. A real boot ends at a shell
        // prompt and stays there; a cap would turn the deliverable — a
        // photograph of a live prompt — into a run that stops looking at it.
        let cfg = Config::default();
        let report = machine.run(&cfg, |_, _| true);
        log::info!("rv64: {}", report.summary());

        // Park rather than exit. The run is over either way, and a process that
        // terminates takes its screen with it — the photograph is the
        // deliverable, and the ending line `run` just wrote is on it.
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            // Keep the spinner moving so a photograph can still distinguish
            // "finished and parked" from "wedged".
            machine.bus.uart.sink.tick();
            machine.bus.uart.sink.flush();
        }
    }
}
