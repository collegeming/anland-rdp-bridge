// Link-platform wiring: Linux compiles + links the PipeWire capture shim
// (`native/anland_rdp_audio.c`); macOS bakes the Swift-runtime rpath into the
// final binary.
//
// The `screencapturekit` crate's own build.rs emits the same rpath flags on
// macOS, but `cargo:rustc-link-arg` is scoped to the emitting package only —
// so a lib crate's flags never reach a downstream binary's link command. We
// have to re-emit them here, where the package being linked IS the binary.
//
// Symptom if you drop the macOS branch:
// `dyld: Library not loaded: @rpath/libswift_Concurrency.dylib`.

use std::process::Command;

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "linux" => build_linux_pw_shim(),
        "macos" => build_macos(),
        _ => {}
    }
}

/// Compile the PipeWire desktop-audio capture shim and link libpipewire/spa.
/// Flags come from `pkg-config` (libpipewire-0.3 / libspa-0.2) with a fallback
/// to the standard Arch header/library paths so a missing pkg-config database
/// doesn't break a known-good install.
fn build_linux_pw_shim() {
    println!("cargo:rerun-if-changed=native/anland_rdp_audio.c");
    println!("cargo:rerun-if-changed=native/anland_rdp_audio.h");

    let mut build = cc::Build::new();
    build.file("native/anland_rdp_audio.c");

    let pw = pkg_config("libpipewire-0.3");
    let spa = pkg_config("libspa-0.2");

    // Header search paths: prefer pkg-config -I, fall back to standard dirs.
    let mut have_pw_inc = false;
    for inc in pw.include.iter().chain(spa.include.iter()) {
        build.flag_if_supported(format!("-I{inc}"));
        have_pw_inc = true;
    }
    if !have_pw_inc {
        for inc in ["/usr/include/pipewire-0.3", "/usr/include/spa-0.2"] {
            build.flag_if_supported(format!("-I{inc}"));
        }
    }

    build.compile("anland_rdp_audio");

    // Library search: prefer pkg-config -L/-l, fall back to bare -l names
    // (the linker's default search paths cover /usr/lib on Arch).
    emit_lib_flags(&pw);
    emit_lib_flags(&spa);
    if pw.lib_paths.is_empty() && pw.libs.is_empty() {
        println!("cargo:rustc-link-lib=pipewire-0.3");
        println!("cargo:rustc-link-lib=spa-0.2");
    }
}

struct PkgConfig {
    include: Vec<String>,
    lib_paths: Vec<String>,
    libs: Vec<String>,
}

fn pkg_config(name: &str) -> PkgConfig {
    fn split_ws(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }
    fn flags(kind: &str, name: &str) -> Vec<String> {
        Command::new("pkg-config")
            .args([kind, name])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().to_string().into())
            .map(|s| split_ws(&s))
            .unwrap_or_default()
    }
    let cflags = flags("--cflags", name);
    let libs = flags("--libs-only-l", name);
    let lib_paths = flags("--libs-only-L", name);
    let include = cflags
        .iter()
        .filter_map(|f| f.strip_prefix("-I").map(String::from))
        .collect();
    let libs = libs
        .iter()
        .filter_map(|f| f.strip_prefix("-l").map(String::from))
        .collect();
    let lib_paths = lib_paths
        .iter()
        .filter_map(|f| f.strip_prefix("-L").map(String::from))
        .collect();
    PkgConfig {
        include,
        lib_paths,
        libs,
    }
}

fn emit_lib_flags(pc: &PkgConfig) {
    for p in &pc.lib_paths {
        println!("cargo:rustc-link-search=native={p}");
    }
    for l in &pc.libs {
        println!("cargo:rustc-link-lib={l}");
    }
}

fn build_macos() {
    // Compile the USB-redirection Obj-C shim (Phase-1b UserHCI spike) and link
    // the public IOUSBHost framework. macOS-only; the module's non-macOS stub
    // never references the extern, so Linux CI builds without any of this.
    println!("cargo:rerun-if-changed=src/usb_redirect/usb_spike.m");
    cc::Build::new()
        .file("src/usb_redirect/usb_spike.m")
        .flag("-fobjc-arc")
        .compile("anland_usb_spike");
    println!("cargo:rustc-link-lib=framework=IOUSBHost");
    println!("cargo:rustc-link-lib=framework=Foundation");

    // /usr/lib/swift holds most of Swift's stdlib on modern macOS — cheap to
    // include even though libswift_Concurrency.dylib lives only in the Xcode
    // toolchain on this version of macOS.
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    let xcode_dev_dir = match Command::new("xcode-select").arg("-p").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => {
            println!(
                "cargo:warning=xcode-select -p failed; anland-rdp-bridge may not load at runtime. \
                 Install full Xcode (not just Command Line Tools)."
            );
            return;
        }
    };

    // Both the old swift-5.5 layout and the unversioned `swift/` layout — Xcode
    // versions differ on which exists. Adding both rpaths is harmless if one
    // is absent; the dynamic linker just skips missing entries.
    for slice in ["swift-5.5", "swift"] {
        let path =
            format!("{xcode_dev_dir}/Toolchains/XcodeDefault.xctoolchain/usr/lib/{slice}/macosx");
        println!("cargo:rustc-link-arg=-Wl,-rpath,{path}");
    }
}
