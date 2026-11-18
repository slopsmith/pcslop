# Toolchain and third-party downloads for the XP benchmark CD (bench/xp,
# scripts/build-bench). Each download is pinned to the file its official
# source served; see bench/README.md for sources and licences.
{ pkgs }:
let
  deps = pkgs.linkFarm "xpbench-deps" [
    {
      name = "7za920.zip";
      path = pkgs.fetchurl {
        url = "https://www.7-zip.org/a/7za920.zip";
        hash = "sha256-Kjr+GcGA+Dc/oC/wAlTVOU/sA0n1gE4K0vYGeFT/KKw=";
      };
    }
    {
      name = "coremark.tar.gz";
      path = pkgs.fetchurl {
        url = "https://github.com/eembc/coremark/archive/1f483d5b8316753a742cbf5590caf5bd0a4e4777.tar.gz";
        hash = "sha256-QGfn8mAhjfE/KHXY+CGo/zKoQVPG4HWdfIn5bAqL+Yc=";
      };
    }
    {
      name = "scimark2_1c.zip";
      path = pkgs.fetchurl {
        url = "https://math.nist.gov/scimark2/scimark2_1c.zip";
        hash = "sha256-IjRkzX6QtMIuKvCNuub2+qM+ZbAeHFjloXaDe8Z5WL4=";
      };
    }
    {
      name = "whetstone.c";
      path = pkgs.fetchurl {
        url = "https://www.netlib.org/benchmark/whetstone.c";
        hash = "sha256-Mz5M7KBCwUb2PuxgVXPRauiwcWbLxEoXvsHql8bx778=";
      };
    }
    {
      name = "linpackc.new";
      path = pkgs.fetchurl {
        url = "https://www.netlib.org/benchmark/linpackc.new";
        hash = "sha256-pj8uyGUSlZ8f2Sa/r7hZBbLXt0ApQv+uOvN01IdF6X4=";
      };
    }
  ];
in
pkgs.mkShellNoCC {
  packages = [
    pkgs.pkgsCross.mingw32.buildPackages.gcc
    pkgs.xorriso
    pkgs.unzip
    pkgs.gnupatch
  ];
  XPBENCH_DEPS = deps;
}
