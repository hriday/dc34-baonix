{ pkgs }:

let
  # The riscv-tests Makefiles invoke the cross toolchain as
  # `$(RISCV_PREFIX)gcc` etc. In nixpkgs the baremetal riscv64 cross
  # toolchain is `pkgsCross.riscv64-embedded`, whose binaries are named
  # `riscv64-none-elf-*` — hence RISCV_PREFIX below.
  cross = pkgs.pkgsCross.riscv64-embedded;
in
pkgs.stdenv.mkDerivation {
  pname = "riscv-tests";
  version = "unstable-2026-08-17";

  # Pinned to a real commit, not `master`: a floating rev would make this
  # build unreproducible and would turn a conformance regression into an
  # unattributable one. Obtained with
  #   nix run nixpkgs#nix-prefetch-github -- \
  #     riscv-software-src riscv-tests --fetch-submodules
  # `fetchSubmodules` matters: riscv-tests vendors riscv-test-env (the `p`
  # and `v` environments, and the `riscv_test.h` the ISA tests include) as
  # a git submodule, and without it `./configure` finds no environment.
  src = pkgs.fetchFromGitHub {
    owner = "riscv-software-src";
    repo = "riscv-tests";
    rev = "2ebecad997fa58cd9e5724340ba75aa4b59bd1d0";
    fetchSubmodules = true;
    hash = "sha256-gp9X37Ymai9E5JsatU/HFaDaY7pi4Z5QaM2rUMv9jqg=";
  };

  nativeBuildInputs = [
    pkgs.autoconf
    pkgs.automake
    # `isa/Makefile` probes for the toolchain with `which $(RISCV_PREFIX)gcc`
    # and `$(error)`s out if that comes back empty, so `which` itself has to
    # be on PATH — it is not part of the bare stdenv.
    pkgs.which
    cross.buildPackages.gcc
    cross.buildPackages.binutils
  ];

  # The upstream tree ships configure.ac but no generated ./configure.
  configurePhase = ''
    runHook preConfigure
    autoconf
    ./configure --prefix=$out --with-xlen=64
    runHook postConfigure
  '';

  # `RISCV_PREFIX` must be *exported*, not passed on the make command line:
  # the top-level `isa` target re-invokes make in a subdirectory, and a
  # command-line variable does not propagate into a sub-make while an
  # environment variable does (`isa/Makefile` assigns it with `?=`).
  RISCV_PREFIX = "riscv64-none-elf-";

  # The stock `all` target builds every suite in both the `p` (physical) and
  # `v` (virtual-memory, syscall-proxy) environments. The `v` environment is
  # out of scope for this emulator, and its recipe additionally shells out to
  # `md5sum`, which does not exist on darwin. So rather than build `all`,
  # ask make to print the exact `-p-` targets for the suites whose extensions
  # this emulator implements — I, M, A, C (`ui`/`um`/`ua`/`uc`) plus the
  # machine- and supervisor-mode privileged suites (`mi`/`si`) — and build
  # only those. `$(tests)` is the makefile's own list, already filtered down
  # to the suites the toolchain can actually target, so this stays correct if
  # upstream adds or renames a test.
  buildPhase = ''
    runHook preBuild

    # printf rather than a heredoc: the recipe line needs a literal tab,
    # which Nix's indented-string form does not let us write reliably.
    printf 'print-p-tests:\n\t@echo $(filter %s,$(tests))\n' \
      'rv64ui-p-% rv64uc-p-% rv64um-p-% rv64ua-p-% rv64mi-p-% rv64si-p-%' \
      > print.mk

    mkdir -p isa
    # --no-print-directory: without it make's "Entering directory" chatter
    # lands in $targets and becomes a bogus make goal.
    targets=$(make --no-print-directory -C isa \
      -f "$PWD/isa/Makefile" -f "$PWD/print.mk" \
      src_dir="$PWD/isa" XLEN=64 print-p-tests)
    echo "building: $targets"
    make -C isa -f "$PWD/isa/Makefile" src_dir="$PWD/isa" XLEN=64 $targets

    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p $out/share/riscv-tests/isa
    cp isa/rv64*-p-* $out/share/riscv-tests/isa/
    # `.dump` disassemblies are not ELFs; the harness reads every file in the
    # directory, so they must not be installed alongside the binaries.
    find $out/share/riscv-tests/isa -name '*.dump' -delete
    runHook postInstall
  '';

  meta = {
    description = "RISC-V official per-instruction ISA test suite (rv64, p environment)";
  };
}
