# The guest userland: a gzipped cpio initramfs whose /nix/store holds a real
# Nix closure, copied path for path. The store paths on screen are the whole
# point of this project, so they are produced by copying the closure of an
# actual derivation — never by creating directories named to look like one.
#
# `guestPkgs` must be a *static, soft-float* riscv64 package set — flake.nix
# builds one by pinning `gcc.arch = "rv64imac"` and `gcc.abi = "lp64"`, and
# explains at length why the kernel's `pkgsCross.riscv64` (rv64gc/lp64d) is
# not usable here. Its build platform is Linux, so from a macOS checkout this
# needs the remote builder:
#
#     nix build .#initramfs --builders \
#       'ssh-ng://linux-builder aarch64-linux /etc/nix/builder_ed25519 4 1 kvm,benchmark,big-parallel - -'
#
# `pkgs` is the native set; it only assembles the archive, so `result` lands
# on the machine you ran `nix build` from.
{
  pkgs,
  guestPkgs,
  # Provenance for /etc/os-release. Locked flake inputs always have `rev`;
  # the fallback is for `--override-input` against a dirty checkout.
  nixpkgsRev ? "unknown",
}:
let
  inherit (pkgs) lib;

  # Static matters for a reason beyond size: nothing in this initramfs
  # provides a dynamic loader at the absolute path a normal riscv64 ELF's
  # PT_INTERP names, so a dynamically linked busybox would fail to exec with
  # ENOENT — exactly the `Failed to execute /init (error -2)` Task 19 saw with
  # its test cpio. A static build has no PT_INTERP at all, and with musl it is
  # also the only path in the closure.
  busybox = guestPkgs.busybox;

  plat = guestPkgs.stdenv.hostPlatform;
  isaExtensions = lib.removePrefix "rv64" (plat.gcc.arch or "");

  # Only the applets /init actually invokes, plus `busybox` itself so that
  # every other applet is still one `busybox <name>` away from the shell
  # prompt. `busybox --install` would generate the full set, but it has to run
  # on the target and this archive is assembled on the host.
  applets = [
    "busybox"
    "sh"
    "mount"
    "ls"
    "cat"
    "tr"
    "uname"
    # Added when nix/guest/init.sh's banner was reshaped for the badge's
    # 16-column display: the store listing now truncates each entry's name
    # (`cut -c1-16`), truncates the device-tree model (`cut -c1-16`), and
    # derives each entry's basename (`basename "$p"`) itself rather than
    # relying on `ls` to have already split it.
    "cut"
    "basename"
  ];

  # Tier 1 of the spec's honesty ladder (see `WRITEUP.md`):
  # every byte of this system came out of /nix/store, and that is all it
  # claims. It is not a NixOS evaluation, so it does not say NixOS — writing
  # `NAME=NixOS` here would make the brag false, which defeats the brag.
  # Nothing about where it runs is asserted either; /init reads the machine
  # name out of the device tree instead.
  #
  # No HOME_URL. The honest candidate is nixos.org — it is nixpkgs' home too
  # — but it is the one field a skeptic could read back as the NixOS claim the
  # line above explicitly disowns, and it carries no information that
  # BUILD_ID's exact revision does not already carry better.
  osRelease = pkgs.writeText "os-release" ''
    NAME="nixpkgs-built Linux"
    ID=nixpkgs-rv64
    PRETTY_NAME="riscv64 Linux and busybox, built by nixpkgs (not NixOS)"
    BUILD_ID="nixpkgs ${nixpkgsRev}"
  '';
in
# Both of these have the same symptom when violated — a guest that prints
# nothing and never comes back — so they are checked at evaluation time
# rather than discovered after a twenty-minute boot.
assert lib.assertMsg plat.isStatic
  "guest userland must come from a static package set; a dynamic riscv64 binary has no loader here";
assert lib.assertMsg (plat.gcc.abi or null == "lp64")
  "guest userland must use the soft-float lp64 ABI, not lp64d: this machine has no FPU";
assert lib.assertMsg (!lib.hasInfix "f" isaExtensions && !lib.hasInfix "d" isaExtensions)
  "guest userland ISA ${plat.gcc.arch or "(unset)"} advertises F or D; the emulator implements only rv64imac";
pkgs.runCommand "guest-initramfs.cpio.gz"
  {
    nativeBuildInputs = [
      pkgs.cpio
      pkgs.gzip
    ];
    meta.description = "riscv64 busybox initramfs carrying a real Nix closure";
    passthru = { inherit busybox; };
  }
  ''
    root=$PWD/root
    mkdir -p $root/{bin,dev,proc,sys,etc,nix/store}

    # The genuine article: every path busybox transitively needs, at the same
    # /nix/store/<hash>-<name> it has on the build machine, so the symlinks
    # below resolve inside the guest and `ls /nix/store` is a real listing.
    # Deliberately not padded — this closure is what it is.
    for p in $(cat ${pkgs.writeClosure [ busybox ]}); do
      cp -a "$p" $root/nix/store/
    done
    chmod -R u+w $root/nix/store

    ${lib.concatMapStrings (a: ''
      ln -s ${busybox}/bin/busybox $root/bin/${a}
    '') applets}

    cp ${osRelease} $root/etc/os-release
    cp ${./init.sh} $root/init
    chmod +x $root/init

    # -depth so a directory's own timestamp is set after its children's,
    # rather than being bumped again by them. Together with cpio's fixed
    # ownership and gzip -n this makes the archive byte-reproducible.
    find $root -depth -exec touch -h -d @1 {} +

    (cd $root && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 --reproducible) \
      | gzip -9n > $out
  ''
