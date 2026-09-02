#!/usr/bin/env bash
# One command that runs everything. Run it before every flash.
#
# It exists because `cargo test --workspace` does not cover this project.
# `badge/app` is a standalone workspace on purpose -- it builds for
# riscv32imac-unknown-xous-elf against a custom sysroot and must not be pulled
# into the host workspace, or Cargo would resolve xous-core's target-specific
# dependencies for every platform when it writes the root lock file. The
# consequence is that `cargo test --workspace` never *compiles*
# `badge/app/tests/dry_run.rs` or `badge/app/tests/oled_boot.rs`, let alone runs
# them -- and those two are the entire argument that the badge port works.
#
# There is no CI here, so "automated" means "one command", and this is it.
#
# Run it inside `nix develop`: that is what sets GUEST_KERNEL, GUEST_DTB and
# GUEST_INITRAMFS, and RV64_REQUIRE_SUITES=1 so a suite that skips for a missing
# prerequisite fails instead of reporting green having run nothing.
set -euo pipefail

cd "$(dirname "$0")"

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

if [ -z "${GUEST_KERNEL:-}" ]; then
    echo "warning: GUEST_KERNEL is unset, so the boot suites will skip." >&2
    echo "         Run this inside \`nix develop\`, which sets it and also sets" >&2
    echo "         RV64_REQUIRE_SUITES=1 to turn a skip into a failure." >&2
fi

say "host workspace: tests"
cargo test --workspace --release

# No `-D warnings`: `crates/rv64` is frozen for this phase (the port was designed
# so the core crate is untouched) and a newer clippy has since grown lints it
# trips. Turning those into errors here would mean either editing a frozen crate
# or sprinkling `allow`s through it, and neither is worth a red check.
say "host workspace: clippy"
cargo clippy --workspace --all-targets

# `--release` is not advice. Booting Linux is 173.5 million emulated
# instructions: ~19 s optimized, minutes not.
say "badge/app: tests (includes the dry run -- the real guest, to a shell)"
(cd badge/app && cargo test --release)

# This one *is* `-D warnings`: badge/app is the code this task owns, it is clean
# today, and it is small enough that keeping it clean is free.
say "badge/app: clippy"
(cd badge/app && cargo clippy --all-targets -- -D warnings)

# The only thing standing between a typo in a syscall and a flash. Needs the
# xous sysroot; see badge/README.md's "Toolchain" section. Skipped with a loud
# note rather than silently, because a skip here is the one that costs a
# hardware cycle.
say "badge/app: type-check the hardware path"
if [ -d "$(rustc --print sysroot)/lib/rustlib/riscv32imac-unknown-xous-elf" ]; then
    (cd badge/app && cargo check --target riscv32imac-unknown-xous-elf)
else
    echo "SKIPPED: no riscv32imac-unknown-xous-elf sysroot for $(rustc --version)." >&2
    echo "         Install it before flashing -- see badge/README.md, ### Toolchain." >&2
    echo "         Nothing else checks the usb-bao1x and Gfx call sites." >&2
fi

say "all green"
