//! Waiting for the services this app needs, instead of assuming them.
//!
//! # The failure this exists to prevent
//!
//! The first hardware run got as far as painting the OLED and then died on
//! `Ticktimer::new().expect("no ticktimer")`, inside `UsbTransport::new`. We
//! ship as a swap-resident `IniS` init process, so we start very early — early
//! enough to reach a well-known-SID connect before the server behind it has
//! registered.
//!
//! `badge/probe` calls ticktimer too and never hit this, because it opens with
//! `std::thread::sleep(Duration::from_secs(5))`, nominally "let a console
//! attach". That sleep was also, accidentally, letting every service finish
//! registering. This crate inherited the probe's code and not its accident.
//!
//! **No laptop test could have found this.** There are no Xous services on a
//! laptop to be un-started, so `tests/dry_run.rs` — which covers the whole run
//! loop against the real host server — has nothing to say about it. That is the
//! honest boundary of the dry run, and it is why this module is written the way
//! the rest of this crate is: the *policy* ([`wait_for`], [`Startup`]) is plain
//! Rust with unit tests, and only the syscall that answers "is it up yet?"
//! lives in `main.rs` below the `cfg`.
//!
//! # Why not a sleep
//!
//! A fixed sleep is either too short on a slow boot or wasted time on a fast
//! one, and it produces the same dead process when it is too short — with no
//! more information than before. Waiting per dependency, with a bound, means a
//! failure names *which* service never came up rather than dying on whichever
//! `expect` happened to be first in the source.
//!
//! # The clock problem, stated rather than hidden
//!
//! The obvious bound is a deadline in milliseconds. But the clock **is** the
//! ticktimer, and the ticktimer is one of the things being waited for: on the
//! badge, `std::thread::sleep` is implemented as a blocking call to the
//! ticktimer server, so using it to wait *for* the ticktimer would fail exactly
//! where it is needed.
//!
//! So [`wait_for`] takes a clock that is allowed to say "there is no clock yet"
//! ([`Clock::None`]). Before the ticktimer, the bound is an attempt count and
//! the backoff is `xous::yield_slice()`, which is a bare syscall and needs no
//! server. After it, the bound is a real deadline and the backoff is a sleep.
//! The attempt budget is generous rather than calibrated, and says so at
//! [`NO_CLOCK_ATTEMPTS`].
//!
//! # Reporting
//!
//! [`Startup`] accumulates one line per dependency, terse enough for a
//! 16-column screen, so a photograph of a failed boot reads
//!
//! ```text
//! log ok 0ms
//! nms ok 0ms
//! tt ok 214ms
//! gfx MISSING
//! ```
//!
//! rather than showing whichever `expect` fired. `Startup::missing` is what the
//! caller stops on, and it names the dependency.

use std::string::String;
use std::vec::Vec;

/// Attempts allowed while there is no clock — i.e. while waiting for the
/// ticktimer itself, and for anything probed before it.
///
/// This is a **budget, not a duration**, and it cannot be otherwise: there is
/// no time source yet, which is the whole problem. Each attempt is one
/// `try_connect` plus one `yield_slice`, so the wall-clock length depends on
/// how much other work the scheduler has to hand out — which at boot, when
/// every service is starting, is a lot, and that is the case where the wait
/// matters.
///
/// Two million is deliberately far past anything plausible. The cost of it
/// being too large is a slow failure on a badge that was never going to boot;
/// the cost of it being too small is this exact bug again, and one more
/// flash-and-photograph cycle to rediscover it.
pub const NO_CLOCK_ATTEMPTS: usize = 2_000_000;

/// Deadline for a dependency probed once a clock exists.
///
/// Thirty seconds is a long time to stare at a badge and a very long time for a
/// service to register. It is set for the failing case rather than the healthy
/// one: a human waits half a minute once and gets a named dependency, instead
/// of waiting forever and getting nothing.
pub const DEADLINE_MS: u64 = 30_000;

/// Milliseconds between probes once there is a clock to sleep against.
///
/// Before that the backoff is a yield, which is as fast as the scheduler
/// allows. Afterwards a sleep is better: nothing here is latency-sensitive, and
/// spinning would take CPU away from the very services being waited for.
pub const POLL_MS: u64 = 10;

/// What [`wait_for`] can bound itself against.
///
/// `None` is not a degenerate case to be tidied away; it is the state the
/// process is genuinely in before the ticktimer answers, and making it a
/// variant is what keeps that fact in the type rather than in a comment.
pub enum Clock<'a> {
    /// No time source yet. Only [`WaitLimits::max_attempts`] applies.
    None,
    /// Milliseconds since some fixed point.
    Ms(&'a mut dyn FnMut() -> u64),
}

/// Bounds on one wait. Both apply; whichever is reached first ends the wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitLimits {
    pub deadline_ms: u64,
    pub max_attempts: usize,
}

impl WaitLimits {
    /// The bound to use before there is a clock.
    pub fn without_a_clock() -> Self {
        Self { deadline_ms: u64::MAX, max_attempts: NO_CLOCK_ATTEMPTS }
    }

    /// The bound to use once there is one. The attempt cap stays as a
    /// backstop for a clock that has stopped advancing, which would otherwise
    /// turn a deadline into an infinite loop.
    pub fn with_a_clock() -> Self {
        Self { deadline_ms: DEADLINE_MS, max_attempts: NO_CLOCK_ATTEMPTS }
    }
}

/// What one wait cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Waited {
    pub attempts: usize,
    /// Zero when the wait ran without a clock.
    pub elapsed_ms: u64,
}

/// Polls `ready` until it says yes or a bound is reached.
///
/// `Ok(waited)` means the dependency is up and how long it took to say so.
/// `Err(waited)` means it never answered — which is a different thing from an
/// error, and the caller reports it differently.
///
/// `backoff` is called between attempts and never before the first, so a
/// dependency that is already up costs exactly one probe and no sleep. That
/// matters more than it looks: this runs once per dependency at every boot, and
/// the healthy path is the one that should be free.
pub fn wait_for(
    mut ready: impl FnMut() -> bool,
    clock: &mut Clock<'_>,
    mut backoff: impl FnMut(),
    limits: WaitLimits,
) -> Result<Waited, Waited> {
    let started = match clock {
        Clock::None => 0,
        Clock::Ms(now) => now(),
    };
    let mut w = Waited::default();
    loop {
        w.attempts += 1;
        if let Clock::Ms(now) = clock {
            w.elapsed_ms = now().saturating_sub(started);
        }
        if ready() {
            return Ok(w);
        }
        if w.attempts >= limits.max_attempts || w.elapsed_ms >= limits.deadline_ms {
            return Err(w);
        }
        backoff();
    }
}

/// One dependency's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dep {
    /// Short enough for a 16-column screen: `log`, `nms`, `tt`, `gfx`, `usb`.
    pub name: &'static str,
    pub outcome: Result<Waited, Waited>,
}

impl Dep {
    /// One screen line. At most 16 characters for every name this app uses.
    ///
    /// The three states read differently on purpose, because they send a reader
    /// to three different places:
    ///
    /// * `tt ok` — connected on the first probe. Nothing to look at.
    /// * `tt ok 214ms` — it was not up and then it was. Startup ordering, not a
    ///   fault; worth seeing, because a number that grows across boots is a
    ///   service getting slower.
    /// * `tt MISSING` — the bound was reached. **This** is the failure, and it
    ///   names itself instead of surfacing as whichever `expect` came first.
    pub fn line(&self) -> String {
        match self.outcome {
            // A single successful probe is the overwhelmingly common case, and
            // "0ms" next to it is noise on a screen with 128 cells.
            Ok(w) if w.attempts <= 1 => format!("{} ok", self.name),
            Ok(w) if w.elapsed_ms > 0 => format!("{} ok {}ms", self.name, w.elapsed_ms),
            // No clock, so the only honest figure is the probe count.
            Ok(w) => format!("{} ok {}p", self.name, w.attempts),
            Err(_) => format!("{} MISSING", self.name),
        }
    }

    pub fn is_missing(&self) -> bool {
        self.outcome.is_err()
    }
}

/// Every dependency's outcome, in the order they were waited for.
#[derive(Debug, Default, Clone)]
pub struct Startup {
    deps: Vec<Dep>,
}

impl Startup {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an outcome and hands it back, so a call site can be
    /// `if startup.record("tt", wait_for(..)).is_err() { ... }`.
    pub fn record(&mut self, name: &'static str, outcome: Result<Waited, Waited>) -> Result<Waited, Waited> {
        self.deps.push(Dep { name, outcome });
        outcome
    }

    /// One line per dependency, for the screen.
    pub fn lines(&self) -> Vec<String> {
        self.deps.iter().map(Dep::line).collect()
    }

    /// The first dependency that never came up.
    ///
    /// This is what a caller stops on, and what its message names. The
    /// *first* rather than any, because later probes may have been skipped or
    /// may have failed as a consequence of this one.
    pub fn missing(&self) -> Option<&'static str> {
        self.deps.iter().find(|d| d.is_missing()).map(|d| d.name)
    }

    /// A single line for `log::`, where width does not matter.
    pub fn summary(&self) -> String {
        let parts: Vec<String> = self
            .deps
            .iter()
            .map(|d| match d.outcome {
                Ok(w) => format!("{}=ok({}p,{}ms)", d.name, w.attempts, w.elapsed_ms),
                Err(w) => format!("{}=MISSING({}p,{}ms)", d.name, w.attempts, w.elapsed_ms),
            })
            .collect();
        parts.join(" ")
    }

    pub fn deps(&self) -> &[Dep] {
        &self.deps
    }
}

/// Why the app stopped before it could run a guest.
///
/// # Why this is a type and not a `log::error!` at each site
///
/// Because "report it and park" has to be impossible to get wrong. The second
/// hardware run was lost to a `return` on the one path where the screen was the
/// missing thing — a badge that vanishes is indistinguishable from one that
/// never started, and that ambiguity has now cost two cycles. Every halt
/// carries a screen line *and* a wire line, both non-empty, and
/// [`Halt::is_reportable`] is asserted over every variant so a new one cannot be
/// added without them.
///
/// `main.rs` renders these and parks; it never decides what to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Halt {
    /// `ticktimer-server` never registered. `UsbTransport` cannot run without
    /// it — this is what killed the first hardware run.
    NoTicktimer,
    /// The graphics server answered but `Gfx::new` failed, so there is no
    /// screen. Reportable only on the wire, which is the point: a missing
    /// screen must still say so somewhere.
    NoScreen(String),
    /// The guest could not be started — a bad image, or a dead link. By this
    /// point the sink has been moved into the machine, so the wire is the only
    /// channel left.
    CannotStartGuest(String),
}

impl Halt {
    /// The screen line, at most [`crate::oled::COLS`] characters.
    ///
    /// Every variant has one even though, with the current startup order, none
    /// of them fires while a screen exists: `NoTicktimer` is waited for before
    /// anything else, `NoScreen` is the screen's own absence, and
    /// `CannotStartGuest` fires after `assemble` has taken the sink. Writing a
    /// line for each anyway is what keeps the mechanism correct rather than
    /// coincidentally unused -- the order has changed three times in three
    /// hardware runs, and the next halt added after the paint will want one.
    /// `main.rs`'s `stop` prints it wherever there is a screen.
    pub fn short(&self) -> String {
        match self {
            Halt::NoTicktimer => "STOP: no tt".into(),
            Halt::NoScreen(_) => "STOP: no gfx".into(),
            Halt::CannotStartGuest(_) => "STOP: no guest".into(),
        }
    }

    /// The wire line. **Never empty**, for any variant — see the type's docs.
    pub fn long(&self) -> String {
        match self {
            Halt::NoTicktimer => {
                "`ticktimer-server` never registered. It is waited for before anything \
                 else because `std` itself needs it: `std::thread::sleep` and \
                 `Instant::now()` connect to it internally and panic if it is absent, so \
                 any library reached before it can die inside std. Nothing ran after this \
                 point, and there was no screen or log server yet to say so on."
                    .into()
            }
            Halt::NoScreen(why) => {
                format!("no console: Gfx::new failed ({why}). Parking rather than exiting, so \
                         this is visible as a live process with a dark screen rather than as a \
                         badge that never started")
            }
            Halt::CannotStartGuest(why) => format!("cannot start the guest: {why}"),
        }
    }

    /// Whether this halt says something on at least one channel. Always true,
    /// and asserted over every variant below.
    pub fn is_reportable(&self) -> bool {
        !self.long().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn limits(attempts: usize, deadline: u64) -> WaitLimits {
        WaitLimits { deadline_ms: deadline, max_attempts: attempts }
    }

    /// The healthy path costs one probe and **no backoff**. A boot pays this
    /// once per dependency; it should be free.
    #[test]
    fn something_already_up_costs_one_probe_and_no_backoff() {
        let backoffs = Cell::new(0);
        let w = wait_for(
            || true,
            &mut Clock::None,
            || backoffs.set(backoffs.get() + 1),
            limits(10, 1000),
        )
        .unwrap();
        assert_eq!(w.attempts, 1);
        assert_eq!(backoffs.get(), 0, "a dependency that is already up must not sleep");
    }

    #[test]
    fn a_dependency_that_appears_late_is_waited_for_and_the_cost_reported() {
        let n = Cell::new(0);
        let w = wait_for(
            || {
                n.set(n.get() + 1);
                n.get() >= 5
            },
            &mut Clock::None,
            || {},
            limits(100, u64::MAX),
        )
        .unwrap();
        assert_eq!(w.attempts, 5);
    }

    /// The bug this module exists for: without a clock, the attempt count is
    /// the only bound, and it must actually terminate.
    #[test]
    fn without_a_clock_the_attempt_budget_terminates_the_wait() {
        let w = wait_for(|| false, &mut Clock::None, || {}, limits(7, u64::MAX)).unwrap_err();
        assert_eq!(w.attempts, 7);
        assert_eq!(w.elapsed_ms, 0, "there was no clock; reporting a duration would be a lie");
    }

    #[test]
    fn with_a_clock_the_deadline_terminates_the_wait() {
        let t = Cell::new(0u64);
        let mut now = || {
            t.set(t.get() + 5);
            t.get()
        };
        let w = wait_for(
            || false,
            &mut Clock::Ms(&mut now),
            || {},
            limits(usize::MAX, 40),
        )
        .unwrap_err();
        assert!(w.elapsed_ms >= 40, "gave up at {}ms", w.elapsed_ms);
    }

    /// A clock that stops advancing must not turn a deadline into an infinite
    /// loop. `WaitLimits::with_a_clock` keeps the attempt cap for exactly this.
    #[test]
    fn a_stopped_clock_still_terminates_on_the_attempt_budget() {
        let mut now = || 0u64;
        let w = wait_for(
            || false,
            &mut Clock::Ms(&mut now),
            || {},
            limits(9, u64::MAX),
        )
        .unwrap_err();
        assert_eq!(w.attempts, 9);
    }

    /// Elapsed time is measured from the first probe, not from some earlier
    /// point, so the number on the screen is this dependency's wait and not the
    /// whole boot's.
    #[test]
    fn elapsed_is_measured_from_this_waits_own_start() {
        let t = Cell::new(1_000u64);
        let mut now = || {
            t.set(t.get() + 10);
            t.get()
        };
        let n = Cell::new(0);
        let w = wait_for(
            || {
                n.set(n.get() + 1);
                n.get() >= 3
            },
            &mut Clock::Ms(&mut now),
            || {},
            limits(100, u64::MAX),
        )
        .unwrap();
        assert!(w.elapsed_ms < 100, "elapsed leaked the clock's absolute value: {}", w.elapsed_ms);
    }

    /// Every line this app can produce fits the badge's 16-column grid. A line
    /// that wrapped would push the summary off an 8-row screen, which is the
    /// one place it has to be readable.
    #[test]
    fn every_startup_line_fits_the_display() {
        let mut s = Startup::new();
        let _ = s.record("log", Ok(Waited { attempts: 1, elapsed_ms: 0 }));
        let _ = s.record("nms", Ok(Waited { attempts: 400, elapsed_ms: 0 }));
        let _ = s.record("tt", Ok(Waited { attempts: 90, elapsed_ms: 21437 }));
        let _ = s.record("gfx", Err(Waited { attempts: 2_000_000, elapsed_ms: 30_000 }));
        let _ = s.record("usb", Ok(Waited { attempts: 2, elapsed_ms: 12 }));
        for line in s.lines() {
            assert!(line.len() <= crate::oled::COLS, "{line:?} is {} wide", line.len());
        }
    }

    /// The three states read differently, because they send a reader to three
    /// different places.
    #[test]
    fn the_three_outcomes_are_distinguishable_on_the_screen() {
        let up = Dep { name: "tt", outcome: Ok(Waited { attempts: 1, elapsed_ms: 0 }) };
        let late = Dep { name: "tt", outcome: Ok(Waited { attempts: 30, elapsed_ms: 214 }) };
        let gone = Dep { name: "tt", outcome: Err(Waited { attempts: 9, elapsed_ms: 30_000 }) };
        assert_eq!(up.line(), "tt ok");
        assert_eq!(late.line(), "tt ok 214ms");
        assert_eq!(gone.line(), "tt MISSING");
        assert!(!up.is_missing() && !late.is_missing() && gone.is_missing());
    }

    /// A dependency that came up after a wait but with no clock reports probes
    /// rather than a fabricated duration.
    #[test]
    fn a_clockless_wait_reports_probes_not_milliseconds() {
        let d = Dep { name: "tt", outcome: Ok(Waited { attempts: 812, elapsed_ms: 0 }) };
        assert_eq!(d.line(), "tt ok 812p");
    }

    #[test]
    fn missing_names_the_first_dependency_that_never_came() {
        let mut s = Startup::new();
        let _ = s.record("log", Ok(Waited::default()));
        let _ = s.record("tt", Err(Waited::default()));
        let _ = s.record("gfx", Err(Waited::default()));
        assert_eq!(s.missing(), Some("tt"));
    }

    fn every_halt() -> Vec<Halt> {
        vec![
            Halt::NoTicktimer,
            Halt::NoScreen("AccessDenied".into()),
            Halt::CannotStartGuest("read 0x80200038 failed".into()),
        ]
    }

    /// **No halt is silent.** The second hardware run was lost to a `return` on
    /// the one path where the screen was the thing that was missing, and a
    /// process that vanishes is indistinguishable from one that never started.
    /// Every variant must say something on the wire.
    #[test]
    fn every_halt_says_something_somewhere() {
        for h in every_halt() {
            assert!(h.is_reportable(), "{h:?} halts without saying anything");
            assert!(h.long().len() > 20, "{h:?}'s wire line is too terse to act on");
        }
    }

    /// Every halt has a screen line and it fits the screen.
    ///
    /// None of them currently fires while a screen exists -- see `Halt::short`
    /// -- but the startup order has changed three times in three hardware runs,
    /// and a variant added after the paint must not be the one that discovers
    /// there was nowhere to say it.
    #[test]
    fn every_halt_has_a_screen_line_that_fits() {
        for h in every_halt() {
            let short = h.short();
            assert!(!short.is_empty(), "{h:?} has no screen line");
            assert!(
                short.len() <= crate::oled::COLS,
                "{h:?} would wrap: {short:?} is {} wide",
                short.len()
            );
        }
    }

    /// The cause travels with the halt rather than being dropped on the way.
    #[test]
    fn a_halt_carries_its_cause_to_the_wire() {
        let h = Halt::CannotStartGuest("read 0x80200038 failed: no answer".into());
        assert!(h.long().contains("0x80200038"), "the cause was lost: {}", h.long());
    }

    #[test]
    fn a_healthy_startup_reports_nothing_missing() {
        let mut s = Startup::new();
        let _ = s.record("log", Ok(Waited::default()));
        let _ = s.record("tt", Ok(Waited::default()));
        assert_eq!(s.missing(), None);
        assert_eq!(s.summary(), "log=ok(0p,0ms) tt=ok(0p,0ms)");
    }
}
