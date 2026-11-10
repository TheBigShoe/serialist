//! The bytes of the `.res` file that `build.rs` hands to the MSVC linker: the application
//! icon and the version resource.
//!
//! `build.rs` includes this file as a module, and so does `tests/windows_res.rs`, so the
//! byte layout is tested on every platform although only a Windows build links it. It uses
//! nothing beyond `std`.
//!
//! A `.res` file is a list of resources, each a 32-byte header then its data padded to a
//! 4-byte boundary. An icon takes one `RT_ICON` resource per image in the `.ico`, plus one
//! `RT_GROUP_ICON` (id 1) that lists them. The version is one `RT_VERSION` resource (id 1)
//! holding a `VS_VERSIONINFO` tree, which Explorer's Details tab and Task Manager read.

/// `RT_ICON`: one image of an icon.
pub const RT_ICON: u16 = 3;
/// `RT_GROUP_ICON`: the list of an icon's images.
pub const RT_GROUP_ICON: u16 = 14;
/// `RT_VERSION`: the `VS_VERSIONINFO` tree.
pub const RT_VERSION: u16 = 16;
/// The id GPUI's Windows backend loads, and the lowest id, which Explorer shows.
pub const APP_ICON_ID: u16 = 1;
/// `VS_VERSION_INFO`, the id `GetFileVersionInfo` looks up.
pub const VERSION_ID: u16 = 1;
/// MOVEABLE | PURE | DISCARDABLE, the flags `rc.exe` gives icon resources.
const ICON_FLAGS: u16 = 0x1030;
/// MOVEABLE | PURE, the flags `rc.exe` gives a version resource. Windows ignores both.
const VERSION_FLAGS: u16 = 0x0030;

/// The size of a resource header with ordinal type and name.
pub const HEADER_SIZE: usize = 32;

/// `VS_FIXEDFILEINFO.dwSignature`.
pub const FIXED_SIGNATURE: u32 = 0xFEEF_04BD;
/// `VS_FIXEDFILEINFO.dwStrucVersion`: version 1.0.
const FIXED_STRUC_VERSION: u32 = 0x0001_0000;
/// `VS_FFI_FILEFLAGSMASK`: every flag `dwFileFlags` may hold.
const FILE_FLAGS_MASK: u32 = 0x3F;
/// `VS_FF_PRERELEASE`.
pub const FF_PRERELEASE: u32 = 0x02;
/// `VOS_NT_WINDOWS32`.
const FILE_OS: u32 = 0x0004_0004;
/// `VFT_APP`.
const FILE_TYPE_APP: u32 = 0x01;

/// US English, the language of the strings.
pub const LANGUAGE: u16 = 0x0409;
/// Code page 1200, UTF-16: the strings are Unicode.
pub const CODE_PAGE: u16 = 0x04B0;

/// The product as `packaging/windows/main.wxs` (`Product Name`) and
/// `packaging/macos/Info.plist.in` (`CFBundleName`) name it.
pub const PRODUCT_NAME: &str = "Serialist";
/// The `Manufacturer` of `packaging/windows/main.wxs`.
pub const COMPANY_NAME: &str = "John Gunn";
/// The `NSHumanReadableCopyright` of `packaging/macos/Info.plist.in`.
pub const COPYRIGHT: &str = "Copyright \u{a9} 2026 John Gunn. Licensed under MIT OR Apache-2.0.";
/// The executable's name, which `OriginalFilename` records.
pub const EXE_NAME: &str = "serialist.exe";

/// A `.res` file holding only the empty resource every one starts with, which marks it as
/// a 32-bit resource file. Push the real resources after it.
pub fn start() -> Vec<u8> {
    let mut res = Vec::new();
    push_resource(&mut res, 0, 0, 0, &[]);
    res
}

/// Append `ico` as the application icon: its images, then the group that lists them.
pub fn push_icon(res: &mut Vec<u8>, ico: &[u8]) -> Result<(), String> {
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

    // GRPICONDIR: the same header, then one 14-byte entry per image.
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&(count as u16).to_le_bytes());

    let mut images = Vec::new();
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

        push_resource(&mut images, RT_ICON, id, ICON_FLAGS, data);

        // GRPICONDIRENTRY: the first 12 bytes of the entry, then the id of the RT_ICON
        // resource in place of the 4-byte offset.
        group.extend_from_slice(&ico[entry..entry + 12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    // Only a whole icon is appended, so a bad .ico leaves `res` as it was.
    res.extend_from_slice(&images);
    push_resource(res, RT_GROUP_ICON, APP_ICON_ID, ICON_FLAGS, &group);
    Ok(())
}

/// A version as `VS_FIXEDFILEINFO` holds it, and as text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
    /// The full version, pre-release suffix included: `0.2.0-rc.1`.
    pub text: String,
    pub pre_release: bool,
}

impl Version {
    /// The version from the parts cargo gives a build script (`CARGO_PKG_VERSION_MAJOR`,
    /// `_MINOR`, `_PATCH`, `_PRE` and `CARGO_PKG_VERSION`). `Err` when a part does not fit
    /// the 16 bits each gets.
    pub fn from_cargo(
        major: &str,
        minor: &str,
        patch: &str,
        pre: &str,
        text: &str,
    ) -> Result<Version, String> {
        let part = |name: &str, value: &str| -> Result<u16, String> {
            value
                .parse()
                .map_err(|_| format!("version {name} {value:?} is not a number from 0 to 65535"))
        };
        Ok(Version {
            major: part("major", major)?,
            minor: part("minor", minor)?,
            patch: part("patch", patch)?,
            text: text.to_owned(),
            pre_release: !pre.is_empty(),
        })
    }

    /// `dwFileVersionMS` and `dwFileVersionLS`: major.minor and patch.0.
    pub fn ms_ls(&self) -> (u32, u32) {
        (
            (u32::from(self.major) << 16) | u32::from(self.minor),
            u32::from(self.patch) << 16,
        )
    }
}

/// The strings of Serialist's version resource, in the order `rc.exe` users write them.
pub fn serialist_strings(version: &Version) -> Vec<(&'static str, String)> {
    vec![
        ("CompanyName", COMPANY_NAME.to_owned()),
        // Task Manager lists a process by its file description, so it is the plain name.
        ("FileDescription", PRODUCT_NAME.to_owned()),
        ("FileVersion", version.text.clone()),
        ("InternalName", "serialist".to_owned()),
        ("LegalCopyright", COPYRIGHT.to_owned()),
        ("OriginalFilename", EXE_NAME.to_owned()),
        ("ProductName", PRODUCT_NAME.to_owned()),
        ("ProductVersion", version.text.clone()),
    ]
}

/// Append the version resource for `version` with Serialist's strings.
pub fn push_version(res: &mut Vec<u8>, version: &Version) {
    let data = version_info(version, &serialist_strings(version));
    push_resource(res, RT_VERSION, VERSION_ID, VERSION_FLAGS, &data);
}

/// A `VS_VERSIONINFO` tree: the fixed numbers, a `StringFileInfo` with one US English,
/// Unicode `StringTable` of `strings`, and a `VarFileInfo` whose `Translation` names that
/// table.
///
/// Every node has the same header: `wLength` (bytes, the node and its children, without
/// the padding after it), `wValueLength`, `wType` (0 binary, 1 text), the NUL-terminated
/// UTF-16 `szKey`, zero padding to a 4-byte boundary, the value, then each child after
/// padding to a 4-byte boundary again.
pub fn version_info(version: &Version, strings: &[(&str, String)]) -> Vec<u8> {
    let (ms, ls) = version.ms_ls();
    let mut fixed = Vec::with_capacity(52);
    for field in [
        FIXED_SIGNATURE,
        FIXED_STRUC_VERSION,
        ms, // dwFileVersionMS
        ls, // dwFileVersionLS
        ms, // dwProductVersionMS
        ls, // dwProductVersionLS
        FILE_FLAGS_MASK,
        if version.pre_release {
            FF_PRERELEASE
        } else {
            0
        },
        FILE_OS,
        FILE_TYPE_APP,
        0, // dwFileSubtype: none for an application
        0, // dwFileDateMS: no date, as rc.exe writes it
        0, // dwFileDateLS
    ] {
        fixed.extend_from_slice(&field.to_le_bytes());
    }

    let table_key = format!("{LANGUAGE:04X}{CODE_PAGE:04X}");
    let string_nodes: Vec<Vec<u8>> = strings
        .iter()
        .map(|(key, value)| node(key, Value::Text(value), &[]))
        .collect();
    let table = node(&table_key, Value::None, &string_nodes);
    let string_file_info = node("StringFileInfo", Value::None, &[table]);

    let mut translation = Vec::with_capacity(4);
    translation.extend_from_slice(&LANGUAGE.to_le_bytes());
    translation.extend_from_slice(&CODE_PAGE.to_le_bytes());
    let var = node("Translation", Value::Binary(&translation), &[]);
    let var_file_info = node("VarFileInfo", Value::None, &[var]);

    node(
        "VS_VERSION_INFO",
        Value::Binary(&fixed),
        &[string_file_info, var_file_info],
    )
}

/// A node's value.
enum Value<'a> {
    /// No value: `wValueLength` 0, `wType` 1, as `rc.exe` writes the grouping nodes.
    None,
    /// Bytes: `wValueLength` counts bytes, `wType` 0.
    Binary(&'a [u8]),
    /// A string: stored NUL-terminated, `wValueLength` counts its UTF-16 units with the NUL,
    /// `wType` 1.
    Text(&'a str),
}

/// One node of a `VS_VERSIONINFO` tree, without padding after it.
fn node(key: &str, value: Value<'_>, children: &[Vec<u8>]) -> Vec<u8> {
    let (value_bytes, value_length, kind) = match value {
        Value::None => (Vec::new(), 0, 1u16),
        Value::Binary(bytes) => (bytes.to_vec(), bytes.len(), 0),
        Value::Text(text) => {
            let bytes = utf16z(text);
            let units = bytes.len() / 2;
            (bytes, units, 1)
        }
    };
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // wLength, filled in below
    out.extend_from_slice(&(value_length as u16).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&utf16z(key));
    pad4(&mut out);
    out.extend_from_slice(&value_bytes);
    for child in children {
        pad4(&mut out);
        out.extend_from_slice(child);
    }
    let length = u16::try_from(out.len()).expect("a version resource node under 64 KiB");
    out[0..2].copy_from_slice(&length.to_le_bytes());
    out
}

/// `text` as NUL-terminated little-endian UTF-16.
fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn pad4(bytes: &mut Vec<u8>) {
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
}

/// Append one resource named by the ordinals `kind` and `id`, in the neutral language.
fn push_resource(res: &mut Vec<u8>, kind: u16, id: u16, flags: u16, data: &[u8]) {
    res.extend_from_slice(&(data.len() as u32).to_le_bytes());
    res.extend_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
    // An ordinal name is 0xFFFF followed by the number.
    res.extend_from_slice(&0xFFFFu16.to_le_bytes());
    res.extend_from_slice(&kind.to_le_bytes());
    res.extend_from_slice(&0xFFFFu16.to_le_bytes());
    res.extend_from_slice(&id.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // data version
    res.extend_from_slice(&flags.to_le_bytes());
    res.extend_from_slice(&0u16.to_le_bytes()); // language: neutral
    res.extend_from_slice(&0u32.to_le_bytes()); // version
    res.extend_from_slice(&0u32.to_le_bytes()); // characteristics
    res.extend_from_slice(data);
    pad4(res);
}
