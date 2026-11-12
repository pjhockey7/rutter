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
#   core/              geography, weather, tides. Pure computation. No GPU, no
#                      windowing, no filesystem, no clock. Portable by
#                      construction, and no dependencies at all.
#   charts/            MBTiles and vector tile decoding: SQLite, gunzip,
#                      protobuf, geometry. Where the chart data comes from.
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
#   rutter doctor | test | run | apk | install | logcat | charts | fmt | clean
#
# And the Nix-built artifacts, which sit alongside the shell rather than
# replacing it — see the comment above rutterSrc for why:
#   nix-build --attr packages.linux | packages.androidLib | packages.apk
#
# Status: 113 tests pass across core and charts; `rutter run --charts
# <file.mbtiles>` opens a window and draws real chart linework; `rutter apk`
# builds a signed, installable APK with the chart bundled. Nothing has been
# run on real hardware yet — `rutter doctor` says so, and so does README.md.

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

  androidRustTargets = lib.mapAttrsToList (_: a: a.rust) androidAbis;

  # ── A portable Linux binary ────────────────────────────────────────────────
  #
  # The binary this shell builds normally is NixOS-only, and not in the
  # "missing a library" sense: its ELF interpreter is a /nix/store path, so on
  # Ubuntu it does not start at all. That makes the Linux target a developer
  # artifact rather than something anyone can be handed.
  #
  # Static musl was the obvious fix and it does not work, which is worth
  # recording so it is not retried. The binary builds, is 18 MB, has no ELF
  # interpreter at all, and reads charts and tide stations happily — and then
  # cannot open a window: "The wayland library could not be loaded". musl's
  # static dlopen is a stub that always fails, and wgpu reaches the GPU by
  # dlopen("libvulkan.so.1"). A graphics program cannot be fully static,
  # because the driver belongs to whoever is running it.
  #
  # So `rutter dist` links old glibc stubs through zig instead. The musl
  # target stays available because it is the right answer for a *headless*
  # tool — chart and tide fetching is HTTP plus SQLite and dlopens nothing,
  # so those can ship as one static file with no glibc floor at all.
  muslTarget = "x86_64-unknown-linux-musl";
  muslCc = pkgs.pkgsCross.musl64.stdenv.cc;

  rustTargets = androidRustTargets ++ [ muslTarget ];

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


  # ── Nix-built artifacts ────────────────────────────────────────────────────
  #
  # These exist alongside the dev shell, not instead of it. Developing through
  # nix-build would be miserable: a derivation is a pure function of its
  # inputs, so one line changed in core/ rebuilds all 297 crates. Cargo's
  # incremental build in the shell is the right tool for that.
  #
  # What these are for:
  #
  #   * Proving the build does not depend on anything in $HOME. If
  #     `nix-build` passes, the build is genuinely reproducible from the
  #     pinned inputs.
  #   * Producing the artifacts to ship. An APK installed on a real boat's
  #     phone should come from a hash-pinned build, not from whatever was in
  #     ~/.cargo that afternoon.
  #
  # Cargo and Nix do not fight here, because each owns a different layer:
  # Cargo.lock pins the crates and Nix *reads* it rather than duplicating it.
  # With no git dependencies in the lockfile — and there are none — this needs
  # no hand-maintained hashes at all, which is the usual reason people give up
  # on nixifying Rust.

  # Only the files that affect the build. Without this, `src = ./.` drags in
  # target/ and build/ — gigabytes — and every rebuild of either invalidates
  # the derivation.
  rutterSrc = lib.cleanSourceWith {
    name = "rutter-src";
    src = ./.;
    filter = path: _type:
      let
        relative = lib.removePrefix (toString ./. + "/") (toString path);
        top = builtins.head (lib.splitString "/" relative);
      in
      builtins.elem top [
        "Cargo.toml"
        "Cargo.lock"
        "core"
        "charts"
        "render"
        "platform"
        "android"
      ];
  };

  # The same toolchain the shell uses, rather than nixpkgs' rustc. Two
  # toolchains would mean the shell and the derivation could disagree about
  # which Rust compiled a given artifact, which is exactly the drift this
  # project pins inputs to avoid.
  rustPlatform = pkgs.makeRustPlatform {
    cargo = rustToolchain;
    rustc = rustToolchain;
  };

  # ── Linux binary ───────────────────────────────────────────────────────────

  rutterLinux = rustPlatform.buildRustPackage {
    pname = "rutter";
    version = "0.1.0";
    src = rutterSrc;

    # Reads Cargo.lock directly: no cargoHash to bump when a dependency
    # changes, and no outputHashes because nothing comes from git.
    cargoLock.lockFile = ./Cargo.lock;

    cargoBuildFlags = [ "--package" "rutter-linux" ];

    # core and charts only. render needs a GPU and a surface, and there is
    # neither in a build sandbox.
    cargoTestFlags = [ "--package" "rutter-core" "--package" "rutter-charts" ];

    nativeBuildInputs = [ pkgs.makeWrapper ];

    # wgpu dlopens the Vulkan loader and winit dlopens the Wayland and X11
    # client libraries. Because they are never DT_NEEDED, Nix's automatic
    # runtime-dependency scan cannot see them and the binary would start and
    # then fail at surface creation with an error that reads like a driver
    # problem. The wrapper is not optional.
    #
    # Mesa is deliberately absent: NixOS exposes the real driver at
    # /run/opengl-driver, which the loader finds on its own, and adding
    # nixpkgs' mesa here puts a second copy of every implicit layer on the
    # path.
    postInstall = ''
      wrapProgram $out/bin/rutter \
        --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath linuxRuntimeLibs}"
    '';

    meta = {
      description = "Marine navigation: charts, weather and tides";
      mainProgram = "rutter";
      platforms = lib.platforms.linux;
    };
  };

  # ── Android shared library ─────────────────────────────────────────────────

  rutterAndroidLib = rustPlatform.buildRustPackage ({
    pname = "rutter-android-lib";
    version = "0.1.0";
    src = rutterSrc;
    cargoLock.lockFile = ./Cargo.lock;

    # `--target` on the command line, not CARGO_BUILD_TARGET in the
    # environment: buildRustPackage's own setup hook exports
    # CARGO_BUILD_TARGET for the host *after* a derivation attribute of that
    # name is set, so setting it here is silently overridden and the build
    # quietly produces a host library instead. The tell is x11 and wayland
    # crates compiling for an Android build.
    cargoBuildFlags = [
      "--package"
      "rutter-android"
      "--target"
      androidAbis.${defaultAbi}.rust
    ];

    # The tests would be aarch64 binaries; there is nothing here to run them
    # on. They are covered by the Linux derivation's check phase, and the
    # code under test is the same.
    doCheck = false;

    # buildRustPackage's install phase looks for bin/; this produces a
    # cdylib, so place it where an APK build expects to find it.
    installPhase = ''
      runHook preInstall

      so=target/${androidAbis.${defaultAbi}.rust}/release/librutter.so
      if [ ! -f "$so" ]; then
        echo "expected $so, which is not there." >&2
        echo "Found instead:" >&2
        find target -name 'librutter.so' -type f >&2 || true
        exit 1
      fi

      # Confirm it is for the architecture claimed. A host build that landed
      # in the right directory would otherwise install silently and only fail
      # at `adb install`, which is a long way from the cause.
      if ! ${pkgs.file}/bin/file -b "$so" | grep -q 'ARM aarch64'; then
        echo "$so is not an aarch64 object:" >&2
        ${pkgs.file}/bin/file -b "$so" >&2
        exit 1
      fi

      mkdir -p "$out/lib/${defaultAbi}"
      cp "$so" "$out/lib/${defaultAbi}/librutter.so"
      chmod +w "$out/lib/${defaultAbi}/librutter.so"

      # Strip explicitly, with the NDK's own tool.
      #
      # Two reasons this is not left to anyone else. Nix's fixup strip is the
      # host binutils and has no business touching an aarch64 Android object,
      # which is why dontStrip is set. And Cargo's `strip = true` in
      # [profile.release] does not survive into this sandbox — the library
      # comes out "with debug_info, not stripped" at 13.6 MB against the
      # 7.1 MB the dev shell produces from the same profile. Doing it here is
      # deterministic and needs no theory about why.
      #
      # --strip-unneeded rather than --strip-all: it keeps .dynsym, and
      # android_main has to stay exported or the NDK's glue cannot find the
      # entry point by name.
      ${ndkBin}/llvm-strip --strip-unneeded "$out/lib/${defaultAbi}/librutter.so"

      if ! ${ndkBin}/llvm-nm --dynamic "$out/lib/${defaultAbi}/librutter.so" \
        | grep -q ' android_main$'; then
        echo "android_main is not exported after stripping" >&2
        exit 1
      fi

      runHook postInstall
    '';

    # Nothing here should be patched for the host: it is an aarch64 Android
    # object and both of these would corrupt it.
    dontPatchELF = true;
    dontStrip = true;

    meta = {
      description = "rutter's Android shared library (${defaultAbi})";
      platforms = lib.platforms.linux;
    };
  } // androidCargoEnv);

  # The APK's assets, assembled rather than checked in twice.
  #
  # The chart lives at charts/tests/data/coast.mbtiles, where it is the unit
  # tests' fixture. Copying it into android/assets/ as well would mean two
  # copies of the same bytes tracked in the repository, free to drift. This
  # builds the directory aapt2 wants from the one real copy.
  #
  # Only the fixture, and under the name the app looks for. The real charts
  # are fetched into build/, which is not a derivation input — a Nix build
  # has to be reproducible from the repository alone, so `nix-build` gives
  # the fixture and `rutter apk` gives whatever has actually been fetched.
  androidAssets = pkgs.runCommand "rutter-android-assets" { } ''
    mkdir -p "$out"
    cp ${./charts/tests/data/coast.mbtiles} "$out/noaa.mbtiles"
  '';

  # ── Unsigned APK ───────────────────────────────────────────────────────────
  #
  # Unsigned on purpose. Everything in /nix/store is world-readable, so a
  # signing key must never become a derivation input — for a throwaway debug
  # key that would be merely untidy, but it is the wrong habit to build into
  # the project before there is a release key to protect.
  #
  # So: Nix produces the reproducible, aligned APK, and `rutter apk` signs it
  # with a key that stays in build/ and is gitignored. Sign this one with:
  #
  #   apksigner sign --ks build/debug.keystore --out signed.apk \
  #     result/rutter-${defaultAbi}-unsigned.apk
  rutterApk = pkgs.runCommand "rutter-${defaultAbi}-unsigned.apk"
    {
      nativeBuildInputs = [ pkgs.zip ];
      passthru = { inherit rutterAndroidLib; };
    }
    ''
      bt=${sdkRoot}/build-tools/${buildTools}
      jar=${sdkRoot}/platforms/android-${compileSdk}/android.jar

      mkdir -p staging/lib/${defaultAbi}
      cp ${rutterAndroidLib}/lib/${defaultAbi}/librutter.so \
        staging/lib/${defaultAbi}/librutter.so
      chmod +w staging/lib/${defaultAbi}/librutter.so

      # No res/ and no classes.dex: the manifest declares hasCode="false" and
      # the activity is the framework's android.app.NativeActivity.
      "$bt/aapt2" link \
        --manifest ${./android/AndroidManifest.xml} \
        -I "$jar" \
        -A ${androidAssets} \
        --min-sdk-version ${minSdk} \
        --target-sdk-version ${compileSdk} \
        -o unaligned.apk \
        --no-version-vectors

      # -0 stores rather than deflates, which extractNativeLibs="false"
      # requires: the loader maps the .so out of the APK in place.
      ( cd staging && zip -q -0 -X ../unaligned.apk "lib/${defaultAbi}/librutter.so" )

      # -P 16 aligns the stored .so to 16 KB pages, which Android 15+ needs.
      # The ELF's own segment alignment is a separate thing, set by the
      # linker flag in androidCargoEnv; both have to be right.
      "$bt/zipalign" -f -P 16 4 unaligned.apk "$out"
    '';

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
        for t in ${lib.concatStringsSep " " androidRustTargets}; do
          if rustc --print target-libdir --target "$t" >/dev/null 2>&1; then
            printf '  %-28s ok\n' "$t"
          else
            printf '  %-28s MISSING\n' "$t"
          fi
        done
        echo

        echo "── NDK compilers ──────────────────────────────────────────────"
        for t in ${lib.concatStringsSep " " androidRustTargets}; do
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
        echo '  On-device testing    `rutter apk` builds and signs one, but'
        echo '                       it has not been run on real hardware.'
        echo '  iOS                  needs Xcode on macOS; cannot be built'
        echo '                       from this machine at all.'
        echo '  Chart data           `rutter charts` is the pipeline; no real'
        echo '                       charts are vendored. The 32 KB fixture'
        echo '                       at charts/tests/data/ is invented data.'
        echo '  Filled polygons      depth areas draw as outlines; filling'
        echo '                       needs a triangulator.'
        echo '  Raster tiles         recognised and refused with a message;'
        echo '                       only vector tiles can be drawn.'
        ;;

      test)
        shift
        # core and charts are where correctness lives, so they are what
        # `test` means. --workspace would drag wgpu in, which takes minutes
        # and tests little that is not better judged by looking at the
        # window.
        exec cargo test --package rutter-core --package rutter-charts "$@"
        ;;

      test-all)
        shift
        exec cargo test --workspace "$@"
        ;;

      run)
        shift
        # The `--` matters: without it cargo eats --charts as its own.
        #
        # With no arguments this draws only the tile grid. Pass a chart to
        # see something:
        #   rutter run --charts charts/tests/data/coast.mbtiles
        exec cargo run --package rutter-linux -- "$@"
        ;;

      build)
        shift
        exec cargo build --workspace "$@"
        ;;

      dist)
        shift
        # ── A Linux binary other people can run ───────────────────────────
        #
        # What `cargo build` produces here is NixOS-only, and not in the
        # "missing a library" sense: its ELF interpreter is a /nix/store
        # path, so on Ubuntu it does not start at all — `execvp: No such
        # file or directory`, with the file sitting right there.
        #
        # The fix is to link against *old glibc stubs* through zig, which
        # gives one ordinary dynamically-linked binary that runs on anything
        # from CentOS 7 onward. Dynamic, not static: a fully static musl
        # build also works and is tempting, but musl's static dlopen is a
        # stub that always fails, and wgpu reaches the GPU driver by
        # dlopen("libvulkan.so.1"). A graphics program cannot be statically
        # linked, because the driver lives on the user's machine.
        glibc=''${RUTTER_DIST_GLIBC:-2.17}
        target="x86_64-unknown-linux-gnu.$glibc"
        out=build/dist/rutter

        echo "building for $target"
        cargo zigbuild --release --target "$target" --package rutter-linux
        mkdir -p build/dist
        cp target/x86_64-unknown-linux-gnu/release/rutter "$out"

        # The checks are the reason this is a subcommand rather than a note
        # in the README. Both failure modes are silent: the binary builds,
        # runs here, and refuses to start on the machine it was built for.
        interp=$(readelf -p .interp "$out" 2>/dev/null | grep -o "/[^ ]*ld-linux[^ ]*" || true)
        case "$interp" in
          "")
            echo "FAILED: no ELF interpreter — is this a static build?" >&2
            exit 1 ;;
          /nix/store/*)
            echo "FAILED: interpreter is $interp" >&2
            echo "That binary only runs on this machine." >&2
            exit 1 ;;
        esac

        wanted=$(readelf -V "$out" 2>/dev/null           | grep -oE "GLIBC_[0-9]+\.[0-9]+" | sort -uV | tail -1)
        echo
        echo "  $out"
        ls -lh "$out" | awk '{print "  size:", $5}'
        echo "  interpreter: $interp"
        echo "  needs up to: $wanted"
        readelf -d "$out" 2>/dev/null | grep -oE "\[lib[^]]*\]"           | tr -d "[]" | paste -sd" " - | awk '{print "  links:", $0}'
        if readelf -d "$out" 2>/dev/null | grep -qE "RPATH|RUNPATH"; then
          echo "  WARNING: carries an RPATH, which may point into /nix/store" >&2
        fi
        echo
        echo "Runs on any x86-64 Linux with glibc >= $glibc. The GPU driver,"
        echo "Wayland or X11, and Vulkan are loaded from the system at run"
        echo "time, so they are the user's to provide — as with any native app."
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
        ;;

      apk)
        shift
        target=$(abi_to_rust "$abi")

        # Build the library first; `rutter android` already prints its size
        # and verifies the ELF, so reuse it rather than duplicating.
        "$0" android

        out=build/apk
        rm -rf "$out"
        mkdir -p "$out/lib/$abi"
        cp "build/android/$abi/librutter.so" "$out/lib/$abi/librutter.so"

        # ── Assets ──────────────────────────────────────────────────────────
        #
        # Whatever real data has been fetched, falling back to the unit
        # tests' tiny fixture so that an APK can always be built on a fresh
        # clone. The names here must match the constants in
        # platform/android/src/lib.rs.
        #
        # Imagery is the big one — a harbour at zoom 16 is tens of megabytes
        # — and it is bundled anyway: a chartplotter with no aerial
        # photography on the one device that goes on the boat is the wrong
        # trade. RUTTER_APK_SKIP_IMAGERY leaves it out when iterating on the
        # app itself, where a 40 MB install over wifi every time is just a
        # wait.
        assets=build/assets
        rm -rf "$assets"
        mkdir -p "$assets"

        bundled=""
        if [ -f build/charts/noaa.mbtiles ]; then
          cp build/charts/noaa.mbtiles "$assets/noaa.mbtiles"
          bundled="$bundled noaa.mbtiles"
        else
          # The fixture, under the name the app looks for, so a clone with
          # no fetched data still shows a chart rather than empty sea.
          cp charts/tests/data/coast.mbtiles "$assets/noaa.mbtiles"
          bundled="$bundled noaa.mbtiles(fixture)"
        fi
        if [ -f build/charts/tides.sqlite ]; then
          cp build/charts/tides.sqlite "$assets/tides.sqlite"
          bundled="$bundled tides.sqlite"
        fi
        if [ -f build/charts/weather.bin ]; then
          cp build/charts/weather.bin "$assets/weather.bin"
          bundled="$bundled weather.bin"
        fi
        if [ -f build/charts/imagery.mbtiles ] && [ -z "''${RUTTER_APK_SKIP_IMAGERY:-}" ]; then
          cp build/charts/imagery.mbtiles "$assets/imagery.mbtiles"
          bundled="$bundled imagery.mbtiles"
        fi
        # The GPS feed's address, if one was given. A file because the
        # Android shim has no command line, and because raising a soft
        # keyboard for the panel's address box is not done yet.
        if [ -n "''${RUTTER_NMEA:-}" ]; then
          printf '%s\n' "''${RUTTER_NMEA}" > "$assets/nmea.txt"
          bundled="$bundled nmea.txt"
        fi
        echo "assets:$bundled"
        du -sh "$assets" | awk '{print "  assets total:", $1}'

        bt=${sdkRoot}/build-tools/${buildTools}
        jar=${sdkRoot}/platforms/android-${compileSdk}/android.jar

        # ── Resources and manifest ──────────────────────────────────────────
        #
        # No Gradle. Gradle under Nix means a wrapper that wants to download
        # its own distribution, an Android Gradle Plugin that fetches a
        # generic-linux aapt2 from Maven that cannot run here, and a JDK
        # version dance — all of which cost a day in ../../opencpn. aapt2,
        # zipalign and apksigner are three commands and do the whole job.
        #
        # There is no res/ directory and no classes.dex: the manifest sets
        # android:hasCode="false" and the activity is the framework's
        # android.app.NativeActivity, so the APK carries no bytecode at all.
        echo "linking resources"
        "$bt/aapt2" link \
          --manifest android/AndroidManifest.xml \
          -I "$jar" \
          -A "$assets" \
          --min-sdk-version ${minSdk} \
          --target-sdk-version ${compileSdk} \
          -o "$out/unaligned.apk" \
          --no-version-vectors

        # ── Native library ──────────────────────────────────────────────────
        #
        # -0 stores without compressing. Required: the manifest says
        # extractNativeLibs="false", so the loader maps the .so straight out
        # of the APK, which it can only do if the bytes are uncompressed and
        # page-aligned.
        echo "adding librutter.so"
        ( cd "$out" && zip -q -0 -X unaligned.apk "lib/$abi/librutter.so" )

        # ── Alignment ───────────────────────────────────────────────────────
        #
        # -P 16 aligns uncompressed .so files to 16 KB pages. Android 15+
        # requires it, and the linker flag in this file only controls the
        # ELF's own segment alignment — the APK's zip offsets are a separate
        # thing that only zipalign can fix.
        #
        # Not `-p -P 16`: zipalign rejects the combination, because -p means
        # the older 4 KB page alignment and -P names a size explicitly.
        echo "aligning"
        "$bt/zipalign" -f -P 16 4 "$out/unaligned.apk" "$out/aligned.apk"

        # ── Signing ─────────────────────────────────────────────────────────
        #
        # A debug keystore, generated on first use. Android refuses to
        # install an unsigned APK; this is the same self-signed key every
        # Android SDK creates for debug builds, with the same well-known
        # password, and it is gitignored because it is worth nothing.
        ks=build/debug.keystore
        if [ ! -f "$ks" ]; then
          echo "generating a debug keystore"
          # PKCS12 rather than the default JKS: keytool warns on every use of
          # a JKS store and tells you to migrate, which is noise on a key
          # that exists only to make the installer happy.
          keytool -genkeypair \
            -keystore "$ks" -storetype PKCS12 \
            -storepass android -keypass android \
            -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10950 \
            -dname "CN=Android Debug, O=Android, C=US"
        fi

        apk=build/rutter-$abi-debug.apk
        echo "signing"
        "$bt/apksigner" sign \
          --ks "$ks" --ks-pass pass:android --key-pass pass:android \
          --ks-key-alias androiddebugkey \
          --out "$apk" "$out/aligned.apk"

        "$bt/apksigner" verify --print-certs "$apk" > /dev/null
        echo
        echo "  $apk"
        ls -lh "$apk" | awk '{print "  size:", $5}'
        # Prove the claims rather than trusting them: no bytecode, the .so
        # stored rather than deflated, and the manifest's own view of itself.
        echo "  dex entries: $(unzip -l "$apk" | grep -c '[.]dex$' || true)"
        echo "  librutter.so: $(unzip -v "$apk" | awk '/librutter[.]so/{print $3, "bytes,", $4}')"
        "$bt/aapt2" dump badging "$apk" 2>/dev/null \
          | grep -E '^(package|sdkVersion|targetSdkVersion|native-code|launchable-activity)' \
          | sed 's/^/  /'
        echo
        echo "Install it with: rutter install"
        ;;

      install)
        shift
        apk=build/rutter-$abi-debug.apk
        if [ ! -f "$apk" ]; then
          echo "no $apk — run 'rutter apk' first" >&2
          exit 1
        fi
        if ! "$adb" get-state >/dev/null 2>&1; then
          echo "no device. Enable wireless debugging on the phone, then:" >&2
          echo "  rutter adb pair <host:port>   # with the pairing code" >&2
          echo "  rutter adb connect <host:port>" >&2
          exit 1
        fi
        # -r reinstalls over an existing copy, keeping its data; -d allows a
        # downgrade, which matters while the version code is not bumped.
        exec "$adb" install -r -d "$apk"
        ;;

      run-android)
        shift
        if ! "$adb" get-state >/dev/null 2>&1; then
          echo "no device — see 'rutter install'" >&2
          exit 1
        fi
        exec "$adb" shell am start -n com.pjhockey7.rutter/android.app.NativeActivity
        ;;

      logcat)
        shift
        # rutter's own log tag, plus the runtime messages that explain a
        # crash the app itself never gets to report.
        exec "$adb" logcat -v time \
          rutter:V RustStdoutStderr:V AndroidRuntime:E DEBUG:V libc:F \
          wgpu:V wgpu_hal:V Vulkan:V "*:S"
        ;;

      uninstall)
        shift
        exec "$adb" uninstall com.pjhockey7.rutter
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

        # ── S-57 reading options ────────────────────────────────────────────
        #
        # SPLIT_MULTIPOINT is the one that matters. In S-57 the whole of a
        # cell's soundings is a single multipoint feature with the depth in
        # each vertex's Z. Without splitting, every sounding on the chart
        # arrives as one feature with no depth attribute, which is useless.
        # With it, one point per sounding, and ADD_SOUNDG_DEPTH lifts the Z
        # into a DEPTH attribute the renderer can label.
        #
        # RETURN_PRIMITIVES off because the edge and node primitives are
        # topology, not chart features, and they would otherwise swamp the
        # output.
        export OGR_S57_OPTIONS="SPLIT_MULTIPOINT=ON,ADD_SOUNDG_DEPTH=ON,RETURN_PRIMITIVES=OFF,RETURN_LINKAGES=OFF,LNAM_REFS=OFF"

        echo "reading $src"
        gdalinfo --version | head -1

        # ── One tile layer per source layer ────────────────────────────────
        #
        # This is the whole point of the rewrite. An ENC cell holds up to a
        # hundred S-57 object classes as separate OGR layers — COALNE for
        # coastline, DEPARE for depth areas, SOUNDG for soundings, LIGHTS,
        # BOYSPP, WRECKS and so on. Flattening them into one tile layer, as
        # this used to, throws away the only information a renderer has for
        # deciding what a feature should look like: a coastline and a cable
        # area come out identical.
        #
        # Most of those classes are empty in any given cell, so empty layers
        # are skipped rather than passed to tippecanoe, which would reject a
        # zero-byte input.
        layers=$(ogrinfo -q "$src" 2>/dev/null \
          | sed -nE 's/^[0-9]+: ([A-Za-z0-9_]+).*/\1/p' | sort -u)

        if [ -z "$layers" ]; then
          echo "  (single-layer source)"
          layers=""
        fi

        args=()
        kept=0
        skipped=0

        if [ -n "$layers" ]; then
          for layer in $layers; do
            f="$work/$layer.geojsons"
            if ! ogr2ogr -f GeoJSONSeq "$f" "$src" "$layer" \
                 -t_srs EPSG:4326 -skipfailures 2>/dev/null; then
              skipped=$((skipped + 1))
              continue
            fi
            if [ ! -s "$f" ]; then
              skipped=$((skipped + 1))
              continue
            fi
            n=$(wc -l < "$f")
            printf '  %-10s %s features\n' "$layer" "$n"
            args+=( "--named-layer=$layer:$f" )
            kept=$((kept + 1))
          done
        fi

        # A source with no layer list — a bare .geojson — still has to work,
        # since "any chart the user provides" includes the simple cases.
        if [ "$kept" -eq 0 ]; then
          f="$work/chart.geojsons"
          ogr2ogr -f GeoJSONSeq "$f" "$src" -t_srs EPSG:4326 -skipfailures
          if [ ! -s "$f" ]; then
            echo "no features found in $src" >&2
            exit 1
          fi
          printf '  %-10s %s features\n' chart "$(wc -l < "$f")"
          args=( "--named-layer=chart:$f" )
          kept=1
        fi

        echo "tiling $kept layer(s)''${skipped:+, $skipped empty or unreadable}"
        # -zg picks the zoom range from the data density; --drop-densest
        # keeps tiles under the 500 KB limit by thinning rather than failing.
        tippecanoe -o "$out" -f -zg \
          --drop-densest-as-needed \
          --extend-zooms-if-still-dropping \
          "''${args[@]}" 2>&1 | tail -1

        echo
        echo "  $out"
        ls -lh "$out" | awk '{print "  size:", $5}'
        # Report what a renderer will actually find, rather than trusting
        # that the layer names survived.
        #
        # Done in SQL rather than with a Python one-liner on purpose. A
        # multi-line script here would need lines at column 0, and Nix's
        # indented-string syntax strips the *common* leading whitespace of
        # the whole script — so one column-0 line silently stops every other
        # line being dedented, and the `rutter help` heredoc below then never
        # finds its terminator. That cost a build to diagnose.
        echo "  layers:"
        sqlite3 -noheader "$out" \
          "select '    ' || (je.value ->> 'id') || '  ' || \
             coalesce((select group_concat(k.key, ', ') \
                       from json_each(je.value -> 'fields') k), '(no attributes)') \
           from metadata, json_each(metadata.value -> 'vector_layers') je \
           where metadata.name = 'json' order by 1;"
        ;;

      noaa)
        shift
        # ── NOAA ENC charts ────────────────────────────────────────────────
        #
        # NOAA publishes Electronic Navigational Charts for US waters free
        # and without registration, which makes them the only official,
        # freely-redistributable chart source this project can default to.
        # Most other hydrographic offices charge.
        #
        # Cells are at a predictable URL; the product catalogue carries every
        # cell's name and coverage, which is what makes `--near` possible.
        base=https://www.charts.noaa.gov/ENCs
        work=build/noaa
        mkdir -p "$work"

        # The national catalogue is ~20 MB. NOAA also publishes per-region
        # and per-state ones that are a fraction of that and enough if you
        # sail one coast — 12Region covers southern California,
        # CA_ENCProdCat_19115.xml the whole state.
        catalog=''${RUTTER_NOAA_CATALOG:-ENCProdCat_19115.xml}

        fetch_catalog() {
          if [ ! -s "$work/$catalog" ]; then
            echo "fetching $catalog (cached in $work)"
            if ! curl -fsSL --retry 3 -o "$work/$catalog" "$base/$catalog"; then
              echo "could not fetch $base/$catalog" >&2
              rm -f "$work/$catalog"
              return 1
            fi
          fi
        }

        case "''${1:-}" in
          --near)
            if [ $# -lt 2 ]; then
              echo "usage: rutter noaa --near <lat>,<lon>" >&2
              exit 64
            fi
            pos=$2; shift 2
            lat=''${pos%%,*}; lon=''${pos##*,}
            if [ "$lat" = "$pos" ] || [ -z "$lon" ]; then
              echo "expected --near <lat>,<lon>, got '$pos'" >&2
              exit 64
            fi

            fetch_catalog || exit 1

            echo "cells covering $lat,$lon, most detailed first:"
            cells=$(python3 ${./scripts/noaa-catalog.py} near "$lat" "$lon" < "$work/$catalog") || exit 1
            echo "$cells" | sed 's/^/  /'
            set -- $cells
            ;;
          --box)
            if [ $# -lt 2 ]; then
              echo "usage: rutter noaa --box <south>,<west>,<north>,<east>" >&2
              exit 64
            fi
            box=$2; shift 2
            IFS=, read -r bs bw bn be <<<"$box" || true
            if [ -z "''${be:-}" ]; then
              echo "expected --box <south>,<west>,<north>,<east>, got '$box'" >&2
              exit 64
            fi
            fetch_catalog || exit 1

            echo "cells overlapping $box, most detailed first:"
            cells=$(python3 ${./scripts/noaa-catalog.py} box "$bs" "$bw" "$bn" "$be" < "$work/$catalog") || exit 1
            echo "$cells" | sed 's/^/  /'
            set -- $cells
            ;;
          --list)
            shift
            fetch_catalog || exit 1
            exec python3 ${./scripts/noaa-catalog.py} list < "$work/$catalog"
            ;;
        esac

        if [ $# -eq 0 ]; then
          echo "usage: rutter noaa <CELL>... | --near <lat>,<lon>" >&2
          echo "                   | --box <s>,<w>,<n>,<e> | --list" >&2
          echo >&2
          echo "  rutter noaa US5CA72M                San Diego Bay, 1:12,000" >&2
          echo "  rutter noaa --near 32.71,-117.17     cells covering a position" >&2
          echo "  rutter noaa --box 32.5,-118.7,33.6,-117.0" >&2
          echo "                                      San Diego to Catalina" >&2
          echo "  rutter noaa --list                  every cell in the catalogue" >&2
          echo >&2
          echo "RUTTER_NOAA_CATALOG picks a smaller catalogue than the" >&2
          echo "national one: 12Region_ENCProdCat_19115.xml for southern" >&2
          echo "California, CA_ENCProdCat_19115.xml for the whole state." >&2
          echo >&2
          echo "Cells are US waters only. Elsewhere, convert your own charts" >&2
          echo "with 'rutter charts <source> <out.mbtiles>'." >&2
          exit 64
        fi

        # Accumulate every cell's layers into one GeoPackage, then tile once.
        #
        # Per-cell MBTiles would mean the app could only ever show one cell
        # at a time, and merging tiles afterwards needs tile-join and care
        # over layer collisions. Merging at the *source* level is simpler and
        # lets the existing `rutter charts` do the rest unchanged.
        export OGR_S57_OPTIONS="SPLIT_MULTIPOINT=ON,ADD_SOUNDG_DEPTH=ON,RETURN_PRIMITIVES=OFF,RETURN_LINKAGES=OFF,LNAM_REFS=OFF"

        # Per-invocation merge file, not a fixed name.
        #
        # Two `rutter noaa` runs at once would otherwise have two ogr2ogr
        # processes appending to the same GeoPackage, which is a SQLite
        # write-lock fight at best and a corrupt chart at worst. Found the
        # hard way by running a second fetch while the first was still going.
        merged="$work/merged.$$.gpkg"
        rm -f "$merged"
        # shellcheck disable=SC2064
        trap "rm -f '$merged' '$merged-journal'" EXIT

        for cell in "$@"; do
          zip="$work/$cell.zip"
          if [ ! -s "$zip" ]; then
            echo "fetching $cell"
            if ! curl -fsSL --retry 3 -o "$zip" "$base/$cell.zip"; then
              echo "  could not fetch $cell — check the name against 'rutter noaa --list'" >&2
              rm -f "$zip"
              continue
            fi
          fi

          rm -rf "$work/$cell.d"
          mkdir -p "$work/$cell.d"
          unzip -qo "$zip" -d "$work/$cell.d"

          # The .000 is the base cell; .001 and up are incremental updates,
          # which GDAL applies automatically when they sit beside it.
          base_file=$(find "$work/$cell.d" -name '*.000' | head -1)
          if [ -z "$base_file" ]; then
            echo "  $cell has no .000 cell file" >&2
            continue
          fi

          echo "  merging $(basename "$base_file")"
          # One call, all layers.
          #
          # The obvious-looking alternative — enumerate layers and convert
          # each — reopens and reparses the whole S-57 cell once per layer,
          # and a real 1:12,000 harbour cell has dozens. That took minutes
          # per cell before this was collapsed into a single pass.
          ogr2ogr -f GPKG -update -append "$merged" "$base_file" \
            -t_srs EPSG:4326 -skipfailures 2>/dev/null || true
        done

        if [ ! -s "$merged" ]; then
          echo "nothing was fetched" >&2
          exit 1
        fi

        out=''${RUTTER_CHART_OUT:-build/charts/noaa.mbtiles}
        mkdir -p "$(dirname "$out")"
        echo
        "$0" charts "$merged" "$out"
        echo
        echo "Load it with: rutter run --charts $out"
        ;;

      tides)
        shift
        # ── Tide stations ─────────────────────────────────────────────────
        #
        # NOAA CO-OPS publishes each harmonic station's constituents — 37
        # amplitudes and phases, a few hundred bytes — and they do not
        # expire. That is the difference between this and the weather: once
        # fetched, the tide is predictable for years with no signal, which
        # is what a boat actually needs.
        #
        # Public domain, no key. See core/src/tide.rs for the prediction and
        # core/tests/tide_noaa.rs for the check against NOAA's own answers.
        out=build/charts/tides.sqlite
        box=""
        near=""
        radius=''${RUTTER_TIDES_RADIUS:-60}

        while [ $# -gt 0 ]; do
          case "$1" in
            --box)    box=$2; shift 2 ;;
            --near)   near=$2; shift 2 ;;
            --radius) radius=$2; shift 2 ;;
            --out)    out=$2; shift 2 ;;
            *) echo "rutter tides: unknown option '$1'" >&2; exit 64 ;;
          esac
        done

        # --near is the common case: you know roughly where you are, not the
        # corners of a box. A degree of latitude is 60 nautical miles, and
        # longitude shrinks with the cosine — close enough for picking which
        # stations to fetch.
        if [ -z "$box" ] && [ -n "$near" ]; then
          IFS=, read -r nlat nlon <<<"$near"
          if [ -z "''${nlon:-}" ]; then
            echo "expected --near <lat>,<lon>, got '$near'" >&2
            exit 64
          fi
          box=$(python3 -c "
import math, sys
lat, lon, nm = float(sys.argv[1]), float(sys.argv[2]), float(sys.argv[3])
dlat = nm / 60.0
dlon = nm / (60.0 * max(math.cos(math.radians(lat)), 0.01))
print('%.4f,%.4f,%.4f,%.4f' % (lat - dlat, lon - dlon, lat + dlat, lon + dlon))
" "$nlat" "$nlon" "$radius")
          echo "within $radius nm of $nlat,$nlon: box $box"
        fi

        if [ -z "$box" ]; then
          echo "usage: rutter tides --near <lat>,<lon> [--radius NM] [--out FILE]" >&2
          echo "       rutter tides --box <s>,<w>,<n>,<e> [--out FILE]" >&2
          echo >&2
          echo "  rutter tides --near 32.68,-117.23" >&2
          echo "  rutter tides --box 32.5,-118.7,33.7,-117.0" >&2
          echo >&2
          echo "Tide constituents are from NOAA CO-OPS — public domain, US" >&2
          echo "waters. They do not expire: fetch once, predict offline." >&2
          exit 64
        fi

        mkdir -p "$(dirname "$out")"
        rm -f "$out"
        python3 ${./scripts/tide-stations.py} --box "$box" --out "$out"
        echo
        if [ "$out" = build/charts/tides.sqlite ]; then
          echo 'rutter run picks this up on its own.'
        else
          echo "Load it with: rutter run --tides $out"
        fi
        ;;

      weather)
        shift
        # ── Wind and temperature ──────────────────────────────────────────
        #
        # GFS from NOAA's NOMADS, subset by variable and bounding box, so a
        # coastline's forecast is a kilobyte an hour instead of the half a
        # gigabyte a global field would be. Public domain, no key.
        #
        # Unlike tides, this expires: a forecast is worth having for a day
        # or two and then it is fiction. `rutter run` says how old the file
        # is rather than drawing it as though it were fresh.
        out=build/charts/weather.bin
        box=""
        hours=''${RUTTER_WEATHER_HOURS:-0,3,6,9,12,15,18,21,24}
        # Degrees of padding added when the box comes from a chart.
        #
        # Generous because the data is almost free: GFS at 0.25 degrees is
        # three floats a node, so a two-degree margin around a harbour chart
        # costs a few tens of kilobytes. Without it the forecast ends at the
        # chart's own edge, and zooming out shows barbs over a small
        # rectangle with nothing beyond — which reads as a broken layer
        # rather than as the edge of the data.
        margin=''${RUTTER_WEATHER_MARGIN:-2}

        while [ $# -gt 0 ]; do
          case "$1" in
            --box)    box=$2; shift 2 ;;
            --chart)  chart=$2; shift 2 ;;
            --hours)  hours=$2; shift 2 ;;
            --margin) margin=$2; shift 2 ;;
            --out)    out=$2; shift 2 ;;
            *) echo "rutter weather: unknown option '$1'" >&2; exit 64 ;;
          esac
        done

        # Same convenience as `rutter imagery`: the chart's own extent is
        # almost always the area wanted.
        if [ -z "$box" ] && [ -n "''${chart:-}" ]; then
          if [ ! -f "$chart" ]; then
            echo "no such chart: $chart" >&2
            exit 1
          fi
          bounds=$(sqlite3 -noheader "$chart"             "select value from metadata where name = 'bounds';")
          if [ -z "$bounds" ]; then
            echo "$chart declares no bounds; pass --box instead" >&2
            exit 1
          fi
          IFS=, read -r bw bs be bn <<<"$bounds"
          box=$(python3 -c "
import sys
s, w, n, e, m = (float(v) for v in sys.argv[1:])
print('%.4f,%.4f,%.4f,%.4f' % (
    max(s - m, -89.0), max(w - m, -179.0),
    min(n + m, 89.0), min(e + m, 179.0)))
" "$bs" "$bw" "$bn" "$be" "$margin")
          echo "using $chart's bounds plus ''${margin} deg: $box"
        fi

        if [ -z "$box" ]; then
          echo "usage: rutter weather --box <s>,<w>,<n>,<e> [--hours 0,3,6] [--out FILE]" >&2
          echo "       rutter weather --chart <file.mbtiles> [--margin DEG]" >&2
          echo >&2
          echo "  rutter weather --chart build/charts/noaa.mbtiles" >&2
          echo "  rutter weather --box 32.5,-118.7,33.6,-117.0 --hours 0,6,12" >&2
          echo >&2
          echo "GFS at 0.25 degrees — about 28 km, so this is the synoptic" >&2
          echo "wind and not the sea breeze in your harbour." >&2
          echo >&2
          echo "--chart pads the chart's bounds by --margin (default 2 deg)," >&2
          echo "because a forecast that stops at the chart's edge looks like" >&2
          echo "a broken layer when you zoom out. The data is cheap: three" >&2
          echo "floats per node per hour." >&2
          exit 64
        fi

        mkdir -p "$(dirname "$out")"
        python3 ${./scripts/weather-grid.py} --box "$box" --hours "$hours" --out "$out"
        echo
        if [ "$out" = build/charts/weather.bin ]; then
          echo 'rutter run picks this up on its own.'
        else
          echo "Load it with: rutter run --weather $out"
        fi
        ;;

      # ── A boat, for when there is not one ─────────────────────────────
      #
      # The GPS path cannot be checked by a unit test alone: "is the marker
      # in the right place on the chart" needs a feed, a chart and a look.
      # So this sends the sentences a real receiver would, and the app has
      # no way to tell the difference.
      nmea)
        shift
        echo "synthetic NMEA feed — in another terminal:"
        echo "  rutter run --nmea udp://:10110"
        echo
        exec python3 "$RUTTER_ROOT/scripts/nmea-feed.py" "$@"
        ;;

      imagery)
        shift
        # ── Aerial imagery ────────────────────────────────────────────────
        #
        # USGS's ImageryOnly basemap: NAIP orthoimagery at 1 m for the
        # conterminous US, 10 m SPOT for Alaska. Federal work, so public
        # domain — no key, no token, and no terms forbidding a cached copy,
        # which matters because the whole point is to have it offline.
        #
        # The credit line (USDA/USGS) is written into the file's metadata and
        # shown by the app, because that is the courtesy owed even when the
        # licence does not demand it.
        #
        # Commercial basemaps were not used on purpose: Google and Bing
        # forbid caching outright, and Esri's World Imagery permits it only
        # under terms that a boat with no signal cannot rely on.
        tiles=https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/tile

        zoom=''${RUTTER_IMAGERY_ZOOM:-14}
        out=build/charts/imagery.mbtiles
        box=""

        # The shallowest level to fetch as well as --zoom.
        #
        # Fetching only the deepest level makes the imagery vanish the moment
        # you zoom out: the viewport then covers thousands of tiles at the one
        # zoom that exists, the reader's tile budget says no, and you get sea
        # colour with a line in the debug log. A pyramid is the fix, and it is
        # close to free — each level up is a quarter of the tiles, so
        # everything above the deepest adds about a third. Cheaper than
        # resampling locally, and the pixels are USGS's rather than ours.
        #
        # Six is low enough to cover a whole-coast view; the levels near it
        # are a handful of tiles each.
        min_zoom=''${RUTTER_IMAGERY_MIN_ZOOM:-6}

        while [ $# -gt 0 ]; do
          case "$1" in
            --box)      box=$2; shift 2 ;;
            --chart)    chart=$2; shift 2 ;;
            --zoom)     zoom=$2; shift 2 ;;
            --min-zoom) min_zoom=$2; shift 2 ;;
            --out)      out=$2; shift 2 ;;
            *) echo "rutter imagery: unknown option '$1'" >&2; exit 64 ;;
          esac
        done

        if [ "$min_zoom" -gt "$zoom" ]; then
          echo "--min-zoom $min_zoom is deeper than --zoom $zoom" >&2
          exit 64
        fi

        # Taking the box from a chart is the common case: imagery is only
        # useful where there is a chart to put it under.
        if [ -z "$box" ] && [ -n "''${chart:-}" ]; then
          if [ ! -f "$chart" ]; then
            echo "no such chart: $chart" >&2
            exit 1
          fi
          bounds=$(sqlite3 -noheader "$chart"             "select value from metadata where name = 'bounds';")
          if [ -z "$bounds" ]; then
            echo "$chart declares no bounds; pass --box instead" >&2
            exit 1
          fi
          # MBTiles bounds are west,south,east,north; --box is s,w,n,e.
          IFS=, read -r bw bs be bn <<<"$bounds"
          box="$bs,$bw,$bn,$be"
          echo "using $chart's bounds: $box"
        fi

        if [ -z "$box" ]; then
          echo "usage: rutter imagery --box <s>,<w>,<n>,<e> [--zoom N] [--min-zoom N] [--out FILE]" >&2
          echo "       rutter imagery --chart <file.mbtiles> [--zoom N] [--min-zoom N]" >&2
          echo >&2
          echo "  rutter imagery --chart build/charts/noaa.mbtiles" >&2
          echo "  rutter imagery --box 32.5,-118.7,33.6,-117.0 --zoom 12" >&2
          echo >&2
          echo "Zoom 14 is roughly 10 m per pixel; 16 is 2.5 m. The tile" >&2
          echo "count quadruples per level, so check it before a big area." >&2
          echo >&2
          echo "--zoom is the deepest level; everything from --min-zoom" >&2
          echo "(default 6) up to it is fetched too, so the imagery survives" >&2
          echo "zooming out. Those extra levels add about a third." >&2
          echo >&2
          echo "Imagery is USGS/USDA NAIP — public domain, US only." >&2
          exit 64
        fi

        IFS=, read -r s w n e <<<"$box"
        if [ -z "''${e:-}" ]; then
          echo "expected --box <south>,<west>,<north>,<east>, got '$box'" >&2
          exit 64
        fi

        count=0
        for z in $(seq "$min_zoom" "$zoom"); do
          at=$(python3 ${./scripts/tile-range.py} --count "$s" "$w" "$n" "$e" "$z")
          echo "  zoom $z: $at tiles"
          count=$((count + at))
        done
        echo "$count tiles across zoom $min_zoom..$zoom"

        cap=''${RUTTER_IMAGERY_MAX_TILES:-2000}
        if [ "$count" -gt "$cap" ]; then
          echo >&2
          echo "That is more than the $cap-tile limit. Either lower --zoom" >&2
          echo "(each level down is a quarter as many), raise --min-zoom," >&2
          echo "or raise the limit" >&2
          echo "with RUTTER_IMAGERY_MAX_TILES if you mean it — at roughly" >&2
          echo "30 KB a tile, $count tiles is about $((count * 30 / 1024)) MB." >&2
          exit 1
        fi

        work=$(mktemp -d)
        trap 'rm -rf "$work"' EXIT
        mkdir -p "$(dirname "$out")"
        rm -f "$out"

        # MBTiles is a SQLite file with an agreed schema. Writing it directly
        # rather than through a tool keeps the dependency list as it is, and
        # the schema is four lines.
        sqlite3 "$out"           "CREATE TABLE metadata (name text, value text);
           CREATE UNIQUE INDEX metadata_name on metadata (name);
           CREATE TABLE tiles (zoom_level integer, tile_column integer,
                               tile_row integer, tile_data blob);
           CREATE UNIQUE INDEX tile_index on tiles
                               (zoom_level, tile_column, tile_row);"

        got=0
        missing=0
        : > "$work/tiles.txt"
        # Deepest first, so an interrupted fetch still leaves the detail
        # you were after rather than only the overviews.
        for z in $(seq "$zoom" -1 "$min_zoom"); do
          python3 ${./scripts/tile-range.py} "$s" "$w" "$n" "$e" "$z"             >> "$work/tiles.txt"
        done

        # Downloads run in parallel inside curl rather than one tile at a
        # time. The serial version spent minutes on a 1400-tile fetch: one
        # connection setup and one sqlite3 process per tile. Curl's own
        # fan-out keeps it to a single process with a shared connection
        # pool, and a config file avoids quoting a shell snippet through
        # xargs.
        #
        # ArcGIS serves tiles as /tile/{z}/{y}/{x} — row before column,
        # which is the reverse of the {z}/{x}/{y} every other scheme uses.
        # Getting it backwards returns valid-looking imagery of the wrong
        # place, which is the worst kind of wrong.
        jobs=''${RUTTER_IMAGERY_JOBS:-8}
        : > "$work/curl.conf"
        while read -r z x y row; do
          printf 'url = "%s/%s/%s/%s"\noutput = "%s/%s-%s-%s.jpg"\n' "$tiles" "$z" "$y" "$x" "$work" "$z" "$x" "$y" >> "$work/curl.conf"
        done < "$work/tiles.txt"

        echo "downloading $count tiles, $jobs at a time"
        # --remove-on-error so a failed transfer leaves no truncated file for
        # the insert pass to mistake for a tile. A non-zero exit is expected
        # whenever any tile is unserved, so it is not treated as fatal — the
        # count below is what decides.
        fetch=( -fSL --retry 2 --max-time 30 --remove-on-error )
        fetch+=( --parallel --parallel-max "$jobs" --progress-bar )
        fetch+=( -K "$work/curl.conf" )
        curl "''${fetch[@]}" || true

        # One transaction in one process, rather than a process per tile.
        # A missing file is a hole, not a failure: an area can simply have no
        # imagery at that level.
        {
          echo 'BEGIN;'
          while read -r z x y row; do
            f="$work/$z-$x-$y.jpg"
            if [ -s "$f" ]; then
              printf "INSERT OR REPLACE INTO tiles VALUES (%s, %s, %s, readfile('%s'));\n" "$z" "$x" "$row" "$f"
              got=$((got + 1))
            else
              missing=$((missing + 1))
            fi
          done < "$work/tiles.txt"
          echo 'COMMIT;'
        } > "$work/insert.sql"
        sqlite3 "$out" < "$work/insert.sql"

        if [ "$got" -eq 0 ]; then
          echo "nothing was downloaded — is the area inside the US?" >&2
          rm -f "$out"
          exit 1
        fi

        # `format` is what tells a reader these are raster tiles, not vector.
        sqlite3 "$out"           "INSERT OR REPLACE INTO metadata VALUES ('name', 'USGS imagery');
           INSERT OR REPLACE INTO metadata VALUES ('format', 'jpg');
           INSERT OR REPLACE INTO metadata VALUES ('type', 'baselayer');
           INSERT OR REPLACE INTO metadata VALUES ('minzoom', '$min_zoom');
           INSERT OR REPLACE INTO metadata VALUES ('maxzoom', '$zoom');
           INSERT OR REPLACE INTO metadata VALUES ('bounds', '$w,$s,$e,$n');
           INSERT OR REPLACE INTO metadata VALUES ('attribution',
             'Imagery: USGS National Map / USDA NAIP (public domain)');"

        echo
        echo "  $out"
        ls -lh "$out" | awk '{print "  size:", $5}'
        echo "  tiles: $got''${missing:+, $missing unavailable}"
        echo
        if [ "$out" = build/charts/imagery.mbtiles ]; then
          echo 'rutter run picks this up on its own.'
        else
          echo "Load it with: rutter run --charts <chart> --imagery $out"
        fi
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
        # printf rather than a heredoc. A heredoc's terminator must sit at
        # column 0 in the *generated* script, which depends on how much
        # common indentation Nix strips from this whole string — so adding
        # one unindented line anywhere above silently breaks it. printf has
        # no such coupling.
        printf '%s\n' \
          'rutter — charts, weather and tides' \
          "" \
          '  doctor       the toolchain and what is still missing  <- start here' \
          '  test         cargo test on core and charts (fast; the real suite)' \
          '  test-all     cargo test on the whole workspace (slow; builds wgpu)' \
          '  run          open the Linux app; --charts <f.mbtiles> to load a chart' \
          '  build        cargo build, whole workspace' \
          '  dist         a portable Linux binary (old glibc via zig)' \
          '  android      cross-compile the shared library for $RUTTER_ABI' \
          '  apk          build a signed, installable APK (no Gradle)' \
          '  install      adb install the APK' \
          '  run-android  launch it on the device' \
          '  logcat       follow the device log, filtered to what matters' \
          '  uninstall    remove it from the device' \
          '  charts       <source> <out.mbtiles> — turn chart data into tiles' \
          '  noaa         fetch NOAA ENC charts (US waters) and tile them' \
          '  imagery      fetch USGS aerial imagery for a chart or a box' \
          '  tides        fetch NOAA tide and tidal-current constituents' \
          '  weather      fetch a GFS wind and temperature forecast' \
          '  nmea         send a synthetic GPS feed, to test without a boat' \
          '  fmt          cargo fmt' \
          '  lint         cargo clippy' \
          '  adb          pass through to the SDK'"'"'s adb' \
          '  clean        remove target/ and build/' \
          "" \
          'Chart sources — `charts` takes anything GDAL reads:' \
          '  an ENC .000 cell or ENC_ROOT directory, a Shapefile, GeoPackage,' \
          '  GeoJSON, or an existing MBTiles passed straight to `run`.' \
          "" \
          'A GPS feed:' \
          '  rutter nmea                    a synthetic boat, UDP to localhost' \
          '  rutter nmea --pty              the same, as a fake serial dongle' \
          '  rutter run --nmea udp://:10110 listen for it (or for a real one)' \
          '  rutter run --nmea /dev/ttyUSB0       a real USB GPS dongle' \
          '  rutter run --nmea tcp://host:10110   connect to a plotter or phone' \
          "" \
          'Environment:' \
          '  RUTTER_ABI   android ABI to build (default ${defaultAbi})' \
          '               one of: ${lib.concatStringsSep ", " (builtins.attrNames androidAbis)}' \
          '  RUTTER_ROOT  project root (default: the current directory)' \
          '  RUTTER_NMEA  a GPS address to bundle into the APK, since the' \
          '               Android shim has no command line to pass it on'
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
      muslCc
      # `rutter dist` links against old glibc stubs through zig, which is
      # what makes one binary run on distributions older than this machine's
      # glibc. cargo-zigbuild is the wrapper that teaches cargo to do it.
      pkgs.zig
      pkgs.cargo-zigbuild
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
      # apksigner and keytool are Java programs. Headless: nothing here
      # opens a window from the JVM, and the full JDK drags in a toolkit.
      pkgs.jdk_headless
      # `zip` adds the .so to the APK aapt2 produced. aapt2 can only package
      # resources and the manifest; native libraries go in afterwards, and
      # they must be STORED rather than deflated for page alignment to mean
      # anything.
      pkgs.zip
      pkgs.unzip
      # `rutter noaa` fetches chart cells and the product catalogue.
      pkgs.curl
    ] ++ linuxRuntimeLibs;

    # The musl cross compiler, for the vendored SQLite in a `rutter dist`
    # build. cargo reads both of these names: the first to link, the second
    # for cc-rs when it compiles C.
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER =
      "${muslCc}/bin/${muslCc.targetPrefix}cc";
    CC_x86_64_unknown_linux_musl = "${muslCc}/bin/${muslCc.targetPrefix}cc";
    AR_x86_64_unknown_linux_musl = "${muslCc.bintools}/bin/${muslCc.targetPrefix}ar";

    JAVA_HOME = "${pkgs.jdk_headless}";
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
  inherit rutterSrc rustPlatform;

  # Nix-built artifacts. See the comment above rutterSrc for why these exist
  # alongside the dev shell rather than instead of it.
  #
  #   nix-build --attr packages.linux       → bin/rutter, wrapped
  #   nix-build --attr packages.androidLib  → lib/<abi>/librutter.so
  #   nix-build --attr packages.apk         → an unsigned, aligned APK
  packages = {
    linux = rutterLinux;
    androidLib = rutterAndroidLib;
    apk = rutterApk;
  };

  shells.default = shell;

  # `nix-shell` with no arguments picks this up via shell.nix.
  inherit shell;
}
