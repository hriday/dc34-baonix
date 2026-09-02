# The three artifacts a boot needs, under one output.
#
# `flake.nix` already exposes `packages.{kernel,dtb,initramfs}` individually,
# and this adds nothing to them — no rebuild, no repackaging, just symlinks.
# What it buys is a single name for "the guest": `nix build .#guest` gets you
# a bootable set in one command, and the devShell can export three
# environment variables that are guaranteed to describe the *same* build
# rather than three independently-resolved store paths that a partial rebuild
# could leave inconsistent.
#
# The three artifacts are not independently versioned but they are
# independently breakable: the DTB describes the memory map the kernel is
# compiled to expect, and the initramfs must be built for the ISA the kernel
# is configured for (rv64imac soft-float — see initramfs.nix's assertions).
# Booting a kernel against a mismatched pair of the others is the failure
# mode this project has spent the most time on, so they are named together.
#
# Building this from a macOS checkout needs the Linux remote builder for the
# kernel and the initramfs; see the headers of kernel.nix and initramfs.nix.
{
  pkgs,
  kernel,
  dtb,
  initramfs,
}:
pkgs.runCommand "guest"
  {
    # Kept reachable so `nix build .#guest` also gets you the kernel's
    # System.map, and so a consumer can find each piece without re-deriving
    # the flake outputs.
    passthru = { inherit kernel dtb initramfs; };
    meta.description = "riscv64 Linux Image, device tree and initramfs for the rv64 emulator";
  }
  ''
    mkdir -p $out
    ln -s ${kernel}/Image $out/Image
    ln -s ${kernel}/System.map $out/System.map
    ln -s ${dtb} $out/guest.dtb
    ln -s ${initramfs} $out/initramfs.cpio.gz
  ''
