{
  description = "riscv64 emulator and guest image for the DC34 badge";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.flake-utils.url = "github:numtide/flake-utils";

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        # nixpkgs declares spike's `meta.platforms` as Linux-only, and a
        # single unavailable package fails the whole `mkShell` — which used
        # to leave the devShell with no cargo/rustc/dtc either, not just no
        # spike. spike does in fact build on aarch64-darwin; the gate is
        # stale, not a real incompatibility. Override just this package's
        # platform list rather than setting `allowUnsupportedSystem`, which
        # would silence the same class of error for every other package.
        spike = pkgs.spike.overrideAttrs (old: {
          meta = old.meta // {
            platforms = old.meta.platforms ++ pkgs.lib.platforms.darwin;
          };
        });

        riscv-tests = pkgs.callPackage ./nix/riscv-tests.nix { inherit pkgs; };
        dtb = pkgs.callPackage ./nix/guest/dtb.nix { inherit pkgs; };

        # A Linux kernel cannot be built on Darwin: kbuild bootstraps its
        # own host tools (fixdep, conf, ...) and the tree assumes a Linux
        # host. So the guest kernel's *build* platform is pinned to Linux on
        # the same CPU, which on aarch64-darwin means the aarch64-linux
        # remote builder. `pkgsCross.riscv64` is nixpkgs' own
        # riscv64-unknown-linux-gnu cross set; the kernel's own Makefile
        # forces -march=rv64imac -mabi=lp64 from CONFIG_* regardless of that
        # toolchain's rv64gc/lp64d defaults, so the emitted code stays
        # inside what the emulator implements.
        buildSystem =
          if pkgs.stdenv.hostPlatform.isLinux then
            system
          else
            "${pkgs.stdenv.hostPlatform.parsed.cpu.name}-linux";

        crossPkgs = (import nixpkgs { system = buildSystem; }).pkgsCross.riscv64;

        # Userland needs a *different* cross set from the kernel's, and the
        # reason is not cosmetic. `pkgsCross.riscv64` targets rv64gc/lp64d:
        # the D extension is in the ISA string and doubles are passed in FP
        # registers. The kernel escapes that because its own Makefile forces
        # `-march=rv64imac -mabi=lp64` from CONFIG_*, but nothing forces it
        # on ordinary packages, so a busybox from that set contains FP
        # instructions.
        #
        # This machine has no FPU (`riscv,isa = "rv64imac"` in guest.dts, and
        # `# CONFIG_FPU is not set` in kernel.config), so such an instruction
        # is illegal. Measured, not assumed: an lp64d busybox traps at its
        # eighth instruction of user code with mcause=2 and mtval=0xb920,
        # which decodes as `c.fsd` — a double-precision store. Worse, the
        # emulator's default `medeleg` does not delegate cause 2 to S-mode,
        # so the trap goes to M-mode, mtvec is 0, and the machine spins at
        # address 0 forever with no diagnostic at all. See the Task 20 report.
        #
        # Pinning `gcc.arch`/`gcc.abi` builds the whole toolchain — gcc,
        # libgcc and musl — for rv64imac/lp64 soft float, so no FP
        # instruction is emitted anywhere in the closure. musl is used rather
        # than glibc for two independent reasons: glibc's riscv64 port
        # requires the D extension, and a static glibc busybox still drags
        # glibc and libgcc into its runtime closure (measured: a 12.9 MiB
        # cpio, which does not fit in 32 MiB of guest RAM).
        guestPkgs =
          (import nixpkgs {
            system = buildSystem;
            crossSystem = {
              config = "riscv64-unknown-linux-musl";
              gcc = {
                arch = "rv64imac";
                abi = "lp64";
              };
            };
          }).pkgsStatic;

        kernel = pkgs.callPackage ./nix/guest/kernel.nix { inherit pkgs crossPkgs; };
        kernel-config = pkgs.callPackage ./nix/guest/kernel-config.nix { inherit crossPkgs; };
        initramfs = pkgs.callPackage ./nix/guest/initramfs.nix {
          inherit pkgs guestPkgs;
          # Recorded in the guest's /etc/os-release, so the provenance on
          # screen is checkable against flake.lock rather than asserted.
          nixpkgsRev = nixpkgs.rev or "unknown";
        };
        # The three of them under one name, so "the guest" is a single thing
        # to build and a single thing for the devShell to point the boot test
        # at. See nix/guest/default.nix — it repackages nothing.
        guest = pkgs.callPackage ./nix/guest/default.nix {
          inherit
            pkgs
            kernel
            dtb
            initramfs
            ;
        };
      in {
        packages.riscv-tests = riscv-tests;
        packages.dtb = dtb;
        packages.kernel = kernel;
        packages.initramfs = initramfs;
        packages.guest = guest;
        # Not a build input of `kernel` — it regenerates the checked-in
        # nix/guest/kernel.config, which `kernel` reads as a plain path so
        # that evaluating the flake needs no import-from-derivation.
        packages.kernel-config = kernel-config;

        # `nix flake check` with no `checks` attribute passes while checking
        # nothing — a green check that means nothing is worse than no check.
        # `guest` already builds `dtb` as one of its three inputs (see
        # nix/guest/default.nix), so building `dtb` here is redundant as a
        # build, but names it as an independently checkable output rather
        # than leaving it reachable only by way of `guest`. Deliberately
        # *not* a `checks.boot` running the Rust boot test under Nix: that
        # needs a nix-built `rv64-host` binary, which this flake does not
        # produce, and was left to the phase-3 plan rather than bolted on
        # here as a pipeline that builds a different binary than the one
        # developers actually run.
        #
        # These check the *guest images*, and nothing else. In particular they
        # do not run the Rust suites, and `cargo test --workspace` at the repo
        # root does not run `badge/app`'s either -- that crate is a standalone
        # workspace on purpose, so its `tests/dry_run.rs` and
        # `tests/oled_boot.rs` are never even compiled from here. Those two are
        # the whole argument that the badge port works. `./check.sh` is the one
        # command that runs everything; use it before a flash.
        checks = { inherit guest dtb; };

        devShells.default = pkgs.mkShell {
          # `spike` here is the let-bound override above, not `pkgs.spike`
          # (a `let` binding outranks `with`, but spelling the rest out
          # avoids relying on that).
          packages = [
            pkgs.cargo pkgs.rustc pkgs.rustfmt pkgs.clippy
            pkgs.dtc pkgs.qemu
            pkgs.cpio pkgs.gzip
            spike
          ];

          # Consumed by crates/rv64-host/tests/riscv_tests.rs.
          RISCV_TESTS = "${riscv-tests}/share/riscv-tests/isa";

          # Consumed by crates/rv64-host/tests/boot.rs, the plan's
          # deliverable: it boots these three and asserts the guest reaches a
          # shell prompt. All three come out of the same `guest` output, so
          # they cannot drift apart from each other.
          #
          # This is what makes entering the devShell depend on the guest
          # kernel and initramfs, which on a macOS checkout means the Linux
          # remote builder the first time. That is the price of the boot test
          # not silently skipping (`RV64_REQUIRE_SUITES` below), and it is
          # the right trade: a green boot test that ran nothing is worse than
          # no boot test.
          GUEST_KERNEL = "${guest}/Image";
          GUEST_DTB = "${guest}/guest.dtb";
          GUEST_INITRAMFS = "${guest}/initramfs.cpio.gz";

          # All four integration suites (riscv-tests, the Spike differential
          # harness, the device-tree check, and the boot test) skip
          # themselves when their external prerequisite is missing, and
          # libtest captures stderr for passing tests — so outside this
          # shell `cargo test --workspace` reports four green tests that
          # ran nothing. Inside this shell every prerequisite is present by
          # construction, so a skip means the environment is broken. This
          # makes it say so. See `rv64_host::suite_prerequisite_missing`.
          RV64_REQUIRE_SUITES = "1";
        };
      });
}
