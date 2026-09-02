{ pkgs }:
pkgs.runCommand "guest.dtb" { nativeBuildInputs = [ pkgs.dtc ]; } ''
  dtc -I dts -O dtb -o $out ${./guest.dts}
''
