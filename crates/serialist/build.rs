//! Build script: give `serialist.exe` its icon on Windows.
//!
//! Explorer, the taskbar and the Start menu take an executable's icon from an icon
//! resource inside it, and GPUI's Windows backend loads resource id 1 for the window
//! class. Nothing else in this crate needs a build script, and a resource-compiler crate
//! would be a build dependency for one file, so this writes the small binary `.res` file
//! itself, straight from `packaging/icons/serialist.ico`, and hands it to the MSVC linker.
//! On every other target it does nothing.
//!
//! A `.res` file is a list of resources, each a 32-byte header then its data padded to a
//! 4-byte boundary. An icon takes one `RT_ICON` resource per image in the `.ico`, plus one
//! `RT_GROUP_ICON` (id 1) that lists them.

use std::env;
use std::fs;
use std::path::PathBuf;

const ICO_PATH: &str = "../../packaging/icons/serialist.ico";

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
/// The id GPUI's Windows backend loads, and the lowest id, which Explorer shows.
const APP_ICON_ID: u16 = 1;
/// MOVEABLE | PURE | DISCARDABLE, the flags `rc.exe` gives icon resources.
const MEMORY_FLAGS: u16 = 0x1030;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed={ICO_PATH}");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    // link.exe takes a .res file as an input. The GNU toolchain would need windres.
    if target_os != "windows" || target_env != "msvc" {
        return;
    }

    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let ico_path = manifest_dir.join(ICO_PATH);
    let ico = match fs::read(&ico_path) {
        Ok(ico) => ico,
        Err(error) => {
            println!(
                "cargo::warning=no Windows icon embedded, cannot read {}: {error}",
                ico_path.display()
            );
            return;
        }
    };
    let res = match icon_resources(&ico) {
        Ok(res) => res,
        Err(message) => panic!("{}: {message}", ico_path.display()),
    };

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("serialist-icon.res");
    fs::write(&out, res).expect("write the icon resource file");
    println!("cargo::rustc-link-arg-bins={}", out.display());
}

/// The bytes of a `.res` file that embeds `ico` as the application icon.
fn icon_resources(ico: &[u8]) -> Result<Vec<u8>, String> {
    let u16_at = |at: usize| -> Result<u16, String> {
        let bytes = ico.get(at..at + 2).ok_or("truncated .ico")?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    };
    let u32_at = |at: usize| -> Result<u32, String> {
        let bytes = ico.get(at..at + 4).ok_or("truncated .ico")?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };

    // ICONDIR: reserved, type (1 = icon), image count.
    if u16_at(0)? != 0 || u16_at(2)? != 1 {
        return Err("not an .ico file".into());
    }
    let count = usize::from(u16_at(4)?);
    if count == 0 {
        return Err("the .ico has no images".into());
    }

    let mut res = Vec::new();
    // Every .res file starts with an empty resource, which marks it as 32-bit.
    push_resource(&mut res, 0, 0, &[]);

    // GRPICONDIR: the same header, then one 14-byte entry per image.
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&(count as u16).to_le_bytes());

    for index in 0..count {
        // ICONDIRENTRY, 16 bytes: width, height, colors, reserved, planes, bit count,
        // data size, data offset.
        let entry = 6 + 16 * index;
        let size = u32_at(entry + 8)? as usize;
        let offset = u32_at(entry + 12)? as usize;
        let data = ico
            .get(offset..offset + size)
            .ok_or("an image runs past the end of the .ico")?;
        let id = u16::try_from(index + 1).map_err(|_| "too many images")?;

        push_resource(&mut res, RT_ICON, id, data);

        // GRPICONDIRENTRY: the first 12 bytes of the entry, then the id of the RT_ICON
        // resource in place of the 4-byte offset.
        group.extend_from_slice(&ico[entry..entry + 12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    push_resource(&mut res, RT_GROUP_ICON, APP_ICON_ID, &group);
    Ok(res)
}

/// Append one resource named by the ordinals `kind` and `id`.
fn push_resource(res: &mut Vec<u8>, kind: u16, id: u16, data: &[u8]) {
    let header_size: u32 = 32;
    res.extend_from_slice(&(data.len() as u32).to_le_bytes());
    res.extend_from_slice(&header_size.to_le_bytes());
    // An ordinal name is 0xFFFF followed by the number.
    res.extend_from_slice(&0xFFFFu16.to_le_bytes());
    res.extend_from_slice(&kind.to_le_bytes());
    res.extend_from_slice(&0xFFFFu16.to_le_bytes());
    res.extend_from_slice(&id.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // data version
    res.extend_from_slice(&MEMORY_FLAGS.to_le_bytes());
    res.extend_from_slice(&0u16.to_le_bytes()); // language: neutral
    res.extend_from_slice(&0u32.to_le_bytes()); // version
    res.extend_from_slice(&0u32.to_le_bytes()); // characteristics
    res.extend_from_slice(data);
    while !res.len().is_multiple_of(4) {
        res.push(0);
    }
}
