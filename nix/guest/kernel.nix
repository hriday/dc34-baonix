# The guest kernel: a raw riscv64 `Image`, loaded by the emulator at
# 0x8020_0000 with a1 pointing at nix/guest/guest.dtb.
#
# `crossPkgs` must be a riscv64-linux cross package set whose *build*
# platform is Linux (see flake.nix). A Linux kernel does not build on
# Darwin, so from a macOS checkout this needs the remote builder:
#
#     nix build .#kernel --builders \
#       'ssh-ng://linux-builder aarch64-linux /etc/nix/builder_ed25519 4 1 kvm,benchmark,big-parallel - -'
#
# `pkgs` is the native package set, used only for the final copy so that
# `result/Image` lands on the machine you ran `nix build` from.
{ pkgs, crossPkgs }:
let
  inherit (pkgs) lib;

  # The generator in kernel-config.nix verifies its own output, but the
  # kernel below does not read the generator's output — it reads the
  # checked-in ./kernel.config, and `packages.kernel-config` is not one of
  # its inputs. So that verification proves nothing about the file this
  # build actually compiles: hand-editing ./kernel.config to drop
  # CONFIG_RISCV_SBI_V01 would produce a kernel that builds perfectly and
  # boots to a blank screen, which is the exact failure the generator's
  # check exists to prevent.
  #
  # This re-runs the same check at evaluation time, over the file that is
  # really used. It costs one `readFile` and no build, and it puts the
  # guarantee in the build graph rather than in the path a careful person
  # happens to take.
  #
  # The semantics deliberately match kernel-config.nix's shell loop: an
  # `X=v` line must be present verbatim, and a `# X is not set` line is
  # satisfied by the symbol being disabled *or* absent (disabling a parent
  # menu removes the child from the file entirely).
  configOf =
    text:
    builtins.listToAttrs (
      lib.concatMap (
        line:
        let
          m = builtins.match "(CONFIG_[A-Za-z0-9_]+)=(.*)" line;
        in
        lib.optional (m != null) {
          name = lib.elemAt m 0;
          value = lib.elemAt m 1;
        }
      ) (lib.splitString "\n" text)
    );

  actual = configOf (builtins.readFile ./kernel.config);

  violations = lib.concatMap (
    line:
    let
      set = builtins.match "(CONFIG_[A-Za-z0-9_]+)=(.*)" line;
      unset = builtins.match "# (CONFIG_[A-Za-z0-9_]+) is not set" line;
    in
    if set != null then
      let
        sym = lib.elemAt set 0;
        want = lib.elemAt set 1;
      in
      lib.optional (actual.${sym} or null != want) "  ${sym}: want ${want}, kernel.config has ${
        if actual ? ${sym} then actual.${sym} else "it unset or absent"
      }"
    else if unset != null then
      let
        sym = lib.elemAt unset 0;
      in
      lib.optional (actual ? ${sym}) "  ${sym}: want it unset, kernel.config has ${actual.${sym}}"
    else
      [ ]
  ) (lib.splitString "\n" (builtins.readFile ./kernel.fragment));
in
assert lib.assertMsg (violations == [ ]) ''
  nix/guest/kernel.config does not honour nix/guest/kernel.fragment:
  ${lib.concatStringsSep "\n" violations}

  kernel.config is generated, not hand-maintained. Edit kernel.fragment and
  regenerate:

      nix build .#kernel-config && cp result nix/guest/kernel.config

  (needs the Linux builder; see the header of this file.)'';
let
  # 6.12 is the oldest LTS still carried in the pinned nixpkgs that is new
  # enough to matter here; 6.1/6.6 are also present but older. Notably, in
  # 6.12 `earlycon-riscv-sbi.c` falls back to the legacy v0.1 console
  # whenever DBCN is absent and CONFIG_RISCV_SBI_V01 is set, which is what
  # makes `earlycon=sbi` work against our SBI stub.
  upstream = crossPkgs.linuxKernel.kernels.linux_6_12;

  kernel = crossPkgs.linuxKernel.manualConfig {
    inherit (upstream) src version;

    # A checked-in path, not a derivation, so `manualConfig` can parse it
    # during evaluation without import-from-derivation. Regenerate it with
    # `nix build .#kernel-config` (nix/guest/kernel-config.nix).
    configfile = ./kernel.config;

    # `manualConfig` builds `target = "Image"` for RISC-V but the arch's
    # `install` rule copies `$(KBUILD_IMAGE)`, which arch/riscv/Makefile sets
    # to `$(boot)/Image.gz` unless EFI_ZBOOT is on. Nothing ever builds that,
    # so the install phase dies with "Missing file: arch/riscv/boot/Image.gz"
    # *after* a complete, successful compile. A make command-line assignment
    # overrides the Makefile's `:=`, which points install at the artifact
    # that was actually built. (`Image.gz` would be no use to us regardless:
    # crates/rv64-host writes the file verbatim to 0x8020_0000 and has no
    # decompressor.)
    extraMakeFlags = [ "KBUILD_IMAGE=arch/riscv/boot/Image" ];

    # The guest's device tree is nix/guest/dtb.nix, built from a .dts we
    # wrote; none of the in-tree riscv DTBs describe this machine, and with
    # every CONFIG_ARCH_* SoC disabled there is nothing left for `make
    # dtbs` to compile anyway.
    buildDTBs = false;
  };
in
pkgs.runCommand "guest-kernel-image-${upstream.version}"
  {
    passthru = { inherit (upstream) version; };
    meta.description = "Trimmed riscv64 Linux ${upstream.version} Image for the rv64 emulator";
  }
  ''
    mkdir -p $out
    cp ${kernel}/Image $out/Image
    # Kept alongside the Image because the first thing anyone will want when
    # this kernel oopses is to turn a PC into a symbol name.
    cp ${kernel}/System.map $out/System.map
  ''
