# rutter — a marine navigation app: charts, weather and tides. Flake-free Nix.
#
# Named for the rutter, the written sailing directions a pilot kept before
# charts were printed: bearings, soundings, landmarks and tides, in a book.
#
# Why this project exists: OpenCPN is the obvious open-source answer and its
# Android build cannot be reproduced from public sources — see ../../opencpn,
# where the toolchain works, an APK installs and starts, and 37 of the Java
# methods its C++ calls exist nowhere in the repository. Rather than keep
# reverse-engineering someone else's unpublished tree, this builds the three
# features that were actually wanted, on a structure meant to be ported.
#
# ── Structure ────────────────────────────────────────────────────────────────
#
# Laid out like firmware, because the goal is the same: one body of
# application code landing on several boards.
#
#   core/              charts, weather, tides. Pure computation. No GPU, no
#                      windowing, no filesystem, no clock. Portable by
#                      construction, and the only crate with real tests.
#   render/            drawing, via wgpu — which is the whole reason this is
#                      Rust. wgpu is the HAL: one renderer over Vulkan on
#                      Android and Linux, Metal on iOS and macOS, DX12 on
#                      Windows.
#   platform/linux/    board support: winit window, event loop, paths.
#   platform/android/  board support: cdylib + NativeActivity glue.
#
# Dependencies point down that list and never up. The moment the core needs
# to know what it is running on, the split has failed.
#
# ── Targets ──────────────────────────────────────────────────────────────────
#
#   Android   primary. Cross-compiled from here; `rutter android`.
#   Linux     primary, and the development target; `rutter run`.
#   Windows   reachable from here via pkgsCross.mingw32, not yet wired up.
#   macOS     reachable on a mac with this same default.nix, untested.
#   iOS       NOT buildable from NixOS, and no amount of Nix will fix it.
#             Apple requires Xcode on macOS to compile and sign. The core and
#             render crates will cross-compile to aarch64-apple-ios from a
#             mac; the shell around them has to be built there. Treat iOS as
#             "the code is ready, the build is not".
#
# Inputs (nixpkgs, rust-overlay) are pinned by nixtamal in ./nix/tamal; bump
# them with
#   nix-shell -p nixtamal --run 'nixtamal refresh'
#
# Enter the dev shell:
#   nix-shell                      # or: nix-shell --attr shells.default
# Then everything is driven by one command:
#   rutter doctor | test | run | android | charts | fmt | lint | clean
#
# Status: the core builds and its tests pass; `rutter run` opens a window and
# draws the chart grid; `rutter android` cross-compiles the shared library.
# There is no APK yet — `rutter doctor` says so, and so does README.md.

{ system ? builtins.currentSystem }:

let
  inputs = import ./nix/tamal { inherit system; };

  # The Android SDK is unfree and carries a click-through licence; both have
  # to be accepted here rather than in the caller's ambient config, so that
  # `nix-shell` in this directory works on a stock machine.
  pkgs = import inputs.nixpkgs {
    inherit system;
    overlays = [ (import inputs.rust-overlay) ];
    config = {
      allowUnfree = true;
      android_sdk.accept_license = true;
    };
  };

  inherit (pkgs) lib;

  # ── Toolchain pins ─────────────────────────────────────────────────────────

  # API 24 is the floor. Chosen deliberately rather than inherited: 21 and 22
  # make `fseeko`/`ftello` unavailable under _FILE_OFFSET_BITS=64 and 23 and
  # below hide `nl_langinfo`, both of which cost a day each in ../../opencpn.
  # 24 is also where Vulkan became reliable, which matters because wgpu wants
  # it. Nothing this app targets runs anything older.
  minSdk = "24";

  # compileSdk 35 and targetSdk 35: Android 14+ refuses to install an app
  # targeting below 23, and Play requires recent targets. 16 KB page
  # alignment (Android 15) is handled by the linker flag below, not by the SDK
  # version.
  compileSdk = "35";
  buildTools = "35.0.0";

  # r27 is the first NDK with 16 KB-page-aligned defaults and a clang new
  # enough that Rust's `aarch64-linux-android` target needs no coaxing.
  ndkVersion = "27.0.12077973";

  androidComposition = pkgs.androidenv.composeAndroidPackages {
    platformVersions = [ compileSdk ];
    buildToolsVersions = [ buildTools ];
    includeNDK = true;
    ndkVersions = [ ndkVersion ];
    # No emulator: a chart renderer on a GPU-less AVD tells you nothing, and
    # the test device is a real phone over adb.
    includeEmulator = false;
    includeSystemImages = false;
    includeSources = false;
    includeCmake = false;
  };

  sdkRoot = "${androidComposition.androidsdk}/libexec/android-sdk";
  ndkRoot = "${sdkRoot}/ndk/${ndkVersion}";
  ndkBin = "${ndkRoot}/toolchains/llvm/prebuilt/linux-x86_64/bin";

  # ── Android ABIs ───────────────────────────────────────────────────────────
  #
  # `rust` is the Rust target triple; `clang` is the NDK's wrapper prefix,
  # which spells armv7 differently; `env` is the triple as it appears in the
  # `CC_<triple>` environment variables cc-rs reads, which uses underscores.
  #
  # arm64-v8a first and alone by default: every Android device sold since
  # 2017 has it, and a one-ABI APK is half the size. The others are listed so
  # adding them is a config change rather than an archaeology exercise.
  androidAbis = {
    "arm64-v8a" = {
      rust = "aarch64-linux-android";
      clang = "aarch64-linux-android";
    };
    "armeabi-v7a" = {
      rust = "armv7-linux-androideabi";
      clang = "armv7a-linux-androideabi";
    };
    "x86_64" = {
      rust = "x86_64-linux-android";
      clang = "x86_64-linux-android";
    };
  };

  defaultAbi = "arm64-v8a";

  rustTargets = lib.mapAttrsToList (_: a: a.rust) androidAbis;

  # Rust from rust-overlay rather than nixpkgs, for one reason: the targets
  # override. nixpkgs' rustc ships std for the build host only, so a
  # `cargo build --target aarch64-linux-android` against it fails on a
  # missing core, and the workarounds all involve pkgsCross contortions that
  # fight cargo rather than help it.
  rustToolchain = pkgs.rust-bin.stable.latest.default.override {
    targets = rustTargets;
    extensions = [
      "rust-src"   # for rust-analyzer and for -Zbuild-std later, if needed
      "rust-analyzer"
      "clippy"
      "rustfmt"
    ];
  };

  # Environment telling cargo and cc-rs how to reach the NDK, per ABI.
  #
  # The alternative is a checked-in .cargo/config.toml, which cannot work
  # here: the linker path contains a Nix store hash, so the file would be
  # either wrong or untrackable. Environment variables keep the store paths
  # out of the repository.
  #
  # The API level is baked into the clang wrapper's name — there is one
  # wrapper per (triple, API) pair — which is why minSdk appears here.
  androidCargoEnv = lib.listToAttrs (lib.concatMap
    (abi:
      let
        a = androidAbis.${abi};
        upper = lib.toUpper (builtins.replaceStrings [ "-" ] [ "_" ] a.rust);
        under = builtins.replaceStrings [ "-" ] [ "_" ] a.rust;
        cc = "${ndkBin}/${a.clang}${minSdk}-clang";
      in
      [
        # cargo reads the linker from the SCREAMING_SNAKE form of the triple.
        { name = "CARGO_TARGET_${upper}_LINKER"; value = cc; }
        # 16 KB pages, required by Android 15+ and free to set earlier.
        {
          name = "CARGO_TARGET_${upper}_RUSTFLAGS";
          value = "-C link-arg=-Wl,-z,max-page-size=16384";
        }
        # cc-rs reads the lowercase-with-underscores form, for any crate in
        # the tree with a build.rs that compiles C.
        { name = "CC_${under}"; value = cc; }
        { name = "CXX_${under}"; value = "${cc}++"; }
        { name = "AR_${under}"; value = "${ndkBin}/llvm-ar"; }
      ])
    (builtins.attrNames androidAbis));

  # ── Runtime libraries for the Linux target ─────────────────────────────────
  #
  # wgpu dlopen()s the Vulkan loader and winit dlopen()s the Wayland and X11
  # client libraries, so neither appears as a DT_NEEDED and neither is found
  # by nix-shell's ordinary library path. They have to be on
  # LD_LIBRARY_PATH explicitly or `rutter run` fails at surface creation with
  # an error that reads like a driver problem.
  linuxRuntimeLibs = with pkgs; [
    vulkan-loader
    libxkbcommon
    wayland
    # X11, for running under XWayland or a plain X session. The top-level
    # names, not the `xorg.*` set, which nixpkgs now warns on.
    libx11
    libxcursor
    libxi
    libxrandr
    # Deliberately NOT mesa. NixOS exposes the real driver at
    # /run/opengl-driver, which the Vulkan loader finds on its own; adding
    # nixpkgs' mesa here puts a second copy of every implicit layer on the
    # path, and the loader reports each one as a duplicate — a dozen warning
    # lines before the first frame, for no gain.
  ];

  # ── The driver ─────────────────────────────────────────────────────────────

  rutter = pkgs.writeShellScriptBin "rutter" ''
    set -euo pipefail

    root="''${RUTTER_ROOT:-$PWD}"
    cd "$root"

    sdk=${sdkRoot}
    ndk=${ndkRoot}
    adb=${androidComposition.androidsdk}/bin/adb
    abi="''${RUTTER_ABI:-${defaultAbi}}"

    abi_to_rust() {
      case "$1" in
        ${lib.concatStringsSep "\n        " (lib.mapAttrsToList
          (abi: a: "${abi}) echo ${a.rust} ;;") androidAbis)}
        *) echo "unknown ABI: $1" >&2; return 1 ;;
      esac
    }

    case "''${1:-help}" in

      doctor)
        echo "── rutter toolchain ────────────────────────────────────────────"
        printf '%-18s %s\n' 'project root' "$root"
        printf '%-18s %s\n' 'rustc' "$(rustc --version)"
        printf '%-18s %s\n' 'cargo' "$(cargo --version)"
        printf '%-18s %s\n' 'android SDK' "$sdk"
        printf '%-18s %s\n' 'android NDK' "${ndkVersion}"
        printf '%-18s %s\n' 'min SDK' "${minSdk}"
        printf '%-18s %s\n' 'default ABI' "$abi"
        echo

        echo "── Rust targets ───────────────────────────────────────────────"
        for t in ${lib.concatStringsSep " " rustTargets}; do
          if rustc --print target-libdir --target "$t" >/dev/null 2>&1; then
            printf '  %-28s ok\n' "$t"
          else
            printf '  %-28s MISSING\n' "$t"
          fi
        done
        echo

        echo "── NDK compilers ──────────────────────────────────────────────"
        for t in ${lib.concatStringsSep " " rustTargets}; do
          var="CC_$(echo "$t" | tr '-' '_')"
          cc="''${!var:-}"
          if [ -n "$cc" ] && [ -x "$cc" ]; then
            printf '  %-28s ok\n' "$t"
          else
            printf '  %-28s MISSING (%s)\n' "$t" "$var"
          fi
        done
        echo

        echo "── Chart pipeline ─────────────────────────────────────────────"
        for tool in ogr2ogr gdalinfo tippecanoe sqlite3; do
          if command -v "$tool" >/dev/null 2>&1; then
            printf '  %-28s ok\n' "$tool"
          else
            printf '  %-28s MISSING\n' "$tool"
          fi
        done
        echo

        echo "── Graphics (this machine) ────────────────────────────────────"
        if command -v vulkaninfo >/dev/null 2>&1; then
          if vulkaninfo --summary >/dev/null 2>&1; then
            printf '  %-28s ok\n' 'vulkan'
          else
            printf '  %-28s no device (will fall back to llvmpipe)\n' 'vulkan'
          fi
        else
          printf '  %-28s vulkaninfo not installed\n' 'vulkan'
        fi
        printf '  %-18s %s\n' 'session' "''${XDG_SESSION_TYPE:-unknown}"
        echo

        echo "── Device ─────────────────────────────────────────────────────"
        if devs=$("$adb" devices 2>/dev/null | tail -n +2 | grep -c 'device$'); then
          printf '  %-28s %s connected\n' 'adb' "$devs"
        else
          printf '  %-28s none connected\n' 'adb'
        fi
        echo

        echo "── Not done yet ───────────────────────────────────────────────"
        echo '  APK packaging        no gradle/aapt2 path wired up; `rutter'
        echo '                       android` builds the .so and stops there.'
        echo '  iOS                  needs Xcode on macOS; cannot be built'
        echo '                       from this machine at all.'
        echo '  Chart data           `rutter charts` is the pipeline; no'
        echo '                       charts are vendored.'
        ;;

      test)
        shift
        # The core is where correctness lives, so it is what `test` means.
        # --workspace would drag wgpu in, which takes minutes and tests
        # nothing that is not better judged by looking at the window.
        exec cargo test --package rutter-core "$@"
        ;;

      test-all)
        shift
        exec cargo test --workspace "$@"
        ;;

      run)
        shift
        exec cargo run --package rutter-linux "$@"
        ;;

      build)
        shift
        exec cargo build --workspace "$@"
        ;;

      android)
        shift
        target=$(abi_to_rust "$abi")
        echo "building rutter-android for $abi ($target)"
        cargo build --package rutter-android --target "$target" --release "$@"

        so=target/$target/release/librutter.so
        if [ ! -f "$so" ]; then
          echo "expected $so, which is not there" >&2
          exit 1
        fi

        out=build/android/$abi
        mkdir -p "$out"
        cp "$so" "$out/librutter.so"

        echo
        echo "  $out/librutter.so"
        ls -lh "$out/librutter.so" | awk '{print "  size:", $5}'
        # Prove it is for the ABI claimed rather than a host build that
        # happened to link. Getting this wrong is silent until install time.
        if command -v readelf >/dev/null 2>&1; then
          printf '  arch: %s\n' \
            "$(readelf -h "$out/librutter.so" | awk -F: '/Machine/{print $2}' | xargs)"
          printf '  pages: %s\n' \
            "$(readelf -lW "$out/librutter.so" | awk '/LOAD/{print $NF; exit}')"
        fi
        echo
        echo "No APK yet — see 'rutter doctor'. The .so is the deliverable"
        echo "for now, and it is what an APK would carry."
        ;;

      charts)
        shift
        if [ $# -lt 2 ]; then
          echo "usage: rutter charts <source> <out.mbtiles>" >&2
          echo >&2
          echo "<source> is anything OGR reads: an ENC .000 cell, a" >&2
          echo "directory of them, a Shapefile, GeoJSON." >&2
          exit 64
        fi
        src="$1"; out="$2"
        work=$(mktemp -d)
        trap 'rm -rf "$work"' EXIT

        echo "reading $src"
        gdalinfo --version
        # ENC cells carry many layers; DEPARE (depth areas), COALNE
        # (coastline) and SOUNDG (soundings) are the ones a chart is mostly
        # made of. Everything else can come later.
        ogr2ogr -f GeoJSONSeq "$work/chart.geojsons" "$src" \
          -t_srs EPSG:4326 -skipfailures

        echo "tiling"
        # -zg picks the zoom range from the data density; --drop-densest
        # keeps tiles under the 500 KB limit by thinning rather than failing.
        tippecanoe -o "$out" -f -zg \
          --drop-densest-as-needed \
          --extend-zooms-if-still-dropping \
          -l chart "$work/chart.geojsons"

        echo
        echo "  $out"
        ls -lh "$out" | awk '{print "  size:", $5}'
        ;;

      fmt)
        shift
        exec cargo fmt --all "$@"
        ;;

      lint)
        shift
        exec cargo clippy --workspace --all-targets "$@"
        ;;

      adb)
        shift
        exec "$adb" "$@"
        ;;

      clean)
        rm -rf target build
        echo "removed target/ and build/"
        ;;

      help | --help | -h)
        cat <<'EOF'
    rutter — charts, weather and tides

      doctor     print the toolchain and what is still missing  <- start here
      test       cargo test on the core (fast; this is the real test suite)
      test-all   cargo test on the whole workspace (slow; builds wgpu)
      run        open the Linux app
      build      cargo build, whole workspace
      android    cross-compile the shared library for $RUTTER_ABI
      charts     <source> <out.mbtiles> — turn chart data into tiles
      fmt        cargo fmt
      lint       cargo clippy
      adb        pass through to the SDK's adb
      clean      remove target/ and build/

    Environment:
      RUTTER_ABI   android ABI to build (default ${defaultAbi})
                   one of: ${lib.concatStringsSep ", " (builtins.attrNames androidAbis)}
      RUTTER_ROOT  project root (default: the current directory)
    EOF
        ;;

      *)
        echo "rutter: unknown command '$1'" >&2
        echo "try 'rutter help'" >&2
        exit 64
        ;;
    esac
  '';

  shell = pkgs.mkShell ({
    name = "rutter";

    packages = [
      rutter
      rustToolchain
      androidComposition.androidsdk
      androidComposition.platform-tools
      pkgs.git
      pkgs.pkg-config
      # Chart pipeline: GDAL reads ENC/S-57 and everything else, tippecanoe
      # turns it into tiles, sqlite3 inspects the resulting MBTiles.
      pkgs.gdal
      pkgs.tippecanoe
      pkgs.sqlite
      # Diagnostics, for when the window is black.
      pkgs.vulkan-tools
      pkgs.binutils
    ] ++ linuxRuntimeLibs;

    ANDROID_HOME = sdkRoot;
    ANDROID_SDK_ROOT = sdkRoot;
    ANDROID_NDK_ROOT = ndkRoot;
    ANDROID_NDK_HOME = ndkRoot;

    # rust-analyzer finds the standard library source through this.
    RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";

    LD_LIBRARY_PATH = lib.makeLibraryPath linuxRuntimeLibs;

    shellHook = ''
      export PS1="(rutter) $PS1"
      export RUTTER_ROOT="$PWD"
      echo "rutter dev shell — 'rutter doctor' for the toolchain, 'rutter help' for commands"
    '';
  } // androidCargoEnv);

in
{
  inherit pkgs androidComposition rustToolchain rutter sdkRoot ndkRoot;
  inherit minSdk compileSdk ndkVersion androidAbis defaultAbi;

  shells.default = shell;

  # `nix-shell` with no arguments picks this up via shell.nix.
  inherit shell;
}
