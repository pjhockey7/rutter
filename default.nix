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
  androidAssets = pkgs.runCommand "rutter-android-assets" { } ''
    mkdir -p "$out"
    cp ${./charts/tests/data/coast.mbtiles} "$out/coast.mbtiles"
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

        # Assets, assembled from the one copy of the chart rather than a
        # second one checked in beside the manifest. Same reason as the
        # androidAssets derivation.
        assets=build/assets
        rm -rf "$assets"
        mkdir -p "$assets"
        cp charts/tests/data/coast.mbtiles "$assets/coast.mbtiles"

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
      test       cargo test on core and charts (fast; the real test suite)
      test-all   cargo test on the whole workspace (slow; builds wgpu)
      run        open the Linux app; --charts <file.mbtiles> to load a chart
      build      cargo build, whole workspace
      android    cross-compile the shared library for $RUTTER_ABI
      apk        build a signed, installable APK (no Gradle)
      install    adb install the APK
      run-android  launch it on the device
      logcat     follow the device log, filtered to what matters
      uninstall  remove it from the device
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
      # apksigner and keytool are Java programs. Headless: nothing here
      # opens a window from the JVM, and the full JDK drags in a toolkit.
      pkgs.jdk_headless
      # `zip` adds the .so to the APK aapt2 produced. aapt2 can only package
      # resources and the manifest; native libraries go in afterwards, and
      # they must be STORED rather than deflated for page alignment to mean
      # anything.
      pkgs.zip
      pkgs.unzip
    ] ++ linuxRuntimeLibs;

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
