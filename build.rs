// Build the Linux PipeWire audio shim for the anland target. The package is a
// single C file (`native/anland_rdp_audio.c`) that calls into libpipewire-0.3
// directly via its public headers, so the build just needs those include dirs
// and one linker flag. The header-only SPA targets live under spa-0.2 and the
// shim only references inline code, so no extra link libraries are required.
//
// Arch's pkg-config database normally covers these paths, and we pass them
// explicitly because the builder Docker image installs the dev packages to the
// same canned locations. Any divergence shows up as a build failure, which is
// exactly the signal you want.

use std::process::Command;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }

    println!("cargo:rerun-if-changed=native/anland_rdp_audio.c");
    println!("cargo:rerun-if-changed=native/anland_rdp_audio.h");

    let mut build = cc::Build::new();
    build.file("native/anland_rdp_audio.c");

    for pkg in ["libpipewire-0.3"] {
        if let Some(out) = pkg_flags(pkg) {
            if let Some(inc) = out {
                build.flag_if_supported(format!("-I{inc}"));
            }
        }
    }
    // Always fall back to the standard image layout too — harmless duplicate
    // -I is a no-op but keeps CI honest if pkg-config's metadata drifts.
    build.include("/usr/include/pipewire-0.3");
    build.include("/usr/include/spa-0.2");
    build.compile("anland_rdp_audio");

    println!("cargo:rustc-link-lib=pipewire-0.3");
}

/// Ask pkg-config for the include path of `pkg`. Returns Some(path) on success,
/// None when pkg-config is missing/unknown (we fall back to the baked path).
fn pkg_flags(pkg: &str) -> Option<Option<String>> {
    let out = Command::new("pkg-config")
        .args(["--cflags", pkg])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let inc = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .find_map(|flag| flag.strip_prefix("-I").map(String::from))?;
    Some(Some(inc))
}
