//! Build script: give `serialist.exe` its icon and version resource on Windows.
//!
//! Explorer, the taskbar and the Start menu take an executable's icon from an icon
//! resource inside it, and GPUI's Windows backend loads resource id 1 for the window
//! class. Explorer's Details tab and Task Manager read the version resource (file and
//! product version, product name, description, copyright). Nothing else in this crate
//! needs a build script, and a resource-compiler crate would be a build dependency for one
//! file, so this writes the small binary `.res` file itself, from
//! `packaging/icons/serialist.ico` and the package version, and hands it to the MSVC
//! linker. On every other target it does nothing.
//!
//! The bytes come from `build/windows_res.rs`, which `tests/windows_res.rs` also includes,
//! so their layout is tested on every platform.

use std::env;
use std::fs;
use std::path::PathBuf;

#[path = "build/windows_res.rs"]
mod windows_res;

const ICO_PATH: &str = "../../packaging/icons/serialist.ico";

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=build/windows_res.rs");
    println!("cargo::rerun-if-changed={ICO_PATH}");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    // link.exe takes a .res file as an input. The GNU toolchain would need windres.
    if target_os != "windows" || target_env != "msvc" {
        return;
    }

    let mut res = windows_res::start();

    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let ico_path = manifest_dir.join(ICO_PATH);
    match fs::read(&ico_path) {
        Ok(ico) => {
            if let Err(message) = windows_res::push_icon(&mut res, &ico) {
                panic!("{}: {message}", ico_path.display());
            }
        }
        Err(error) => println!(
            "cargo::warning=no Windows icon embedded, cannot read {}: {error}",
            ico_path.display()
        ),
    }

    let cargo = |name: &str| env::var(name).unwrap_or_default();
    let version = windows_res::Version::from_cargo(
        &cargo("CARGO_PKG_VERSION_MAJOR"),
        &cargo("CARGO_PKG_VERSION_MINOR"),
        &cargo("CARGO_PKG_VERSION_PATCH"),
        &cargo("CARGO_PKG_VERSION_PRE"),
        &cargo("CARGO_PKG_VERSION"),
    )
    .unwrap_or_else(|message| panic!("{message}"));
    windows_res::push_version(&mut res, &version);

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("serialist.res");
    fs::write(&out, res).expect("write the resource file");
    println!("cargo::rustc-link-arg-bins={}", out.display());
}
