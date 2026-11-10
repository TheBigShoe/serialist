//! The `.res` bytes `build.rs` links into `serialist.exe`, checked on every platform.
//!
//! The expected layouts are Microsoft's: the resource header of the `.res` format
//! (`RESOURCEHEADER`), `GRPICONDIR`, and the `VS_VERSIONINFO`, `VS_FIXEDFILEINFO`,
//! `StringFileInfo`, `StringTable`, `String`, `VarFileInfo` and `Var` structures. The
//! walker below finds values the way `VerQueryValue` does, by key, stepping from one child
//! to the next by its `wLength` rounded up to 4 bytes.

#[path = "../build/windows_res.rs"]
mod windows_res;

use std::path::Path;

use windows_res::{
    APP_ICON_ID, CODE_PAGE, COMPANY_NAME, COPYRIGHT, EXE_NAME, FF_PRERELEASE, FIXED_SIGNATURE,
    HEADER_SIZE, LANGUAGE, PRODUCT_NAME, RT_GROUP_ICON, RT_ICON, RT_VERSION, VERSION_ID, Version,
};

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn align4(at: usize) -> usize {
    at.div_ceil(4) * 4
}

fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// One resource of a `.res` file.
#[derive(Debug)]
struct Resource {
    kind: u16,
    id: u16,
    flags: u16,
    language: u16,
    data: Vec<u8>,
}

/// The resources of a `.res` file, checking each header's fixed fields and padding.
fn resources(res: &[u8]) -> Vec<Resource> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < res.len() {
        assert_eq!(at % 4, 0, "a resource starts on a 4-byte boundary");
        let data_size = u32_at(res, at) as usize;
        assert_eq!(u32_at(res, at + 4) as usize, HEADER_SIZE, "HeaderSize");
        assert_eq!(u16_at(res, at + 8), 0xFFFF, "TYPE is an ordinal");
        assert_eq!(u16_at(res, at + 12), 0xFFFF, "NAME is an ordinal");
        assert_eq!(u32_at(res, at + 16), 0, "DataVersion");
        assert_eq!(u32_at(res, at + 24), 0, "Version");
        assert_eq!(u32_at(res, at + 28), 0, "Characteristics");
        let data = res[at + HEADER_SIZE..at + HEADER_SIZE + data_size].to_vec();
        let end = at + HEADER_SIZE + data_size;
        let padded = align4(end);
        assert!(padded <= res.len(), "the data is padded to 4 bytes");
        assert!(res[end..padded].iter().all(|&byte| byte == 0));
        out.push(Resource {
            kind: u16_at(res, at + 10),
            id: u16_at(res, at + 14),
            flags: u16_at(res, at + 20),
            language: u16_at(res, at + 22),
            data,
        });
        at = padded;
    }
    out
}

/// A node of a `VS_VERSIONINFO` tree.
#[derive(Debug)]
struct Node {
    value_length: u16,
    kind: u16,
    key: String,
    value: Vec<u8>,
    children: Vec<Node>,
}

/// Read the node at the start of `bytes`, and its children.
fn node(bytes: &[u8]) -> Node {
    let length = usize::from(u16_at(bytes, 0));
    let value_length = u16_at(bytes, 2);
    let kind = u16_at(bytes, 4);
    let mut units = Vec::new();
    let mut at = 6;
    loop {
        let unit = u16_at(bytes, at);
        at += 2;
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    let key = String::from_utf16(&units).unwrap();
    let value_start = align4(at);
    assert!(
        bytes[at..value_start].iter().all(|&byte| byte == 0),
        "Padding1 of {key} is zero"
    );
    // wValueLength counts bytes for binary values and UTF-16 units for text.
    let value_bytes = match kind {
        0 => usize::from(value_length),
        1 => 2 * usize::from(value_length),
        other => panic!("{key}: wType {other} is neither 0 nor 1"),
    };
    let value = bytes[value_start..value_start + value_bytes].to_vec();
    let mut children = Vec::new();
    let mut child = align4(value_start + value_bytes);
    while child < length {
        let child_node_length = usize::from(u16_at(bytes, child));
        assert!(child_node_length > 0, "{key}: a child with no length");
        assert!(
            child + child_node_length <= length,
            "{key}: a child runs past its parent"
        );
        children.push(node(&bytes[child..child + child_node_length]));
        child = align4(child + child_node_length);
    }
    Node {
        value_length,
        kind,
        key,
        value,
        children,
    }
}

impl Node {
    fn child(&self, key: &str) -> &Node {
        self.children
            .iter()
            .find(|child| child.key == key)
            .unwrap_or_else(|| panic!("{} has no {key}", self.key))
    }

    /// A `String` node's text, without its NUL.
    fn text(&self) -> String {
        assert_eq!(self.kind, 1, "{} is text", self.key);
        let units: Vec<u16> = self
            .value
            .chunks(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(units.last(), Some(&0), "{} ends with a NUL", self.key);
        String::from_utf16(&units[..units.len() - 1]).unwrap()
    }
}

fn version(text: &str) -> Version {
    let (numbers, pre) = text.split_once('-').unwrap_or((text, ""));
    let mut parts = numbers.split('.');
    let mut next = || parts.next().unwrap();
    Version::from_cargo(next(), next(), next(), pre, text).unwrap()
}

fn version_resource(version: &Version) -> Resource {
    let mut res = windows_res::start();
    windows_res::push_version(&mut res, version);
    let mut all = resources(&res);
    assert_eq!(all.len(), 2);
    all.pop().unwrap()
}

#[test]
fn a_res_file_starts_with_the_empty_32_bit_marker() {
    let mut expected = vec![0u8; 32];
    expected[4] = 0x20; // HeaderSize
    expected[8..10].copy_from_slice(&[0xFF, 0xFF]);
    expected[12..14].copy_from_slice(&[0xFF, 0xFF]);
    assert_eq!(windows_res::start(), expected);
}

#[test]
fn the_shipped_icon_becomes_one_rt_icon_per_image_and_a_group() {
    let ico = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/icons/serialist.ico"),
    )
    .unwrap();
    let count = usize::from(u16_at(&ico, 4));
    assert!(count > 0);

    let mut res = windows_res::start();
    windows_res::push_icon(&mut res, &ico).unwrap();
    let all = resources(&res);
    assert_eq!(all.len(), 1 + count + 1);

    for (index, image) in all[1..=count].iter().enumerate() {
        let entry = 6 + 16 * index;
        let size = u32_at(&ico, entry + 8) as usize;
        let offset = u32_at(&ico, entry + 12) as usize;
        assert_eq!(image.kind, RT_ICON);
        assert_eq!(usize::from(image.id), index + 1);
        assert_eq!(image.language, 0);
        assert_eq!(image.data, ico[offset..offset + size]);
    }

    let group = &all[count + 1];
    assert_eq!(group.kind, RT_GROUP_ICON);
    assert_eq!(group.id, APP_ICON_ID);
    assert_eq!(group.flags, 0x1030);
    // GRPICONDIR: reserved 0, type 1, count; then 14-byte GRPICONDIRENTRYs.
    assert_eq!(group.data.len(), 6 + 14 * count);
    assert_eq!(group.data[0..6], ico[0..6]);
    for index in 0..count {
        let entry = &group.data[6 + 14 * index..6 + 14 * (index + 1)];
        assert_eq!(entry[..12], ico[6 + 16 * index..6 + 16 * index + 12]);
        assert_eq!(usize::from(u16_at(entry, 12)), index + 1, "nID");
    }
}

#[test]
fn a_bad_icon_is_refused_and_appends_nothing() {
    let mut res = windows_res::start();
    let before = res.clone();
    assert!(windows_res::push_icon(&mut res, b"not an icon").is_err());
    // One image whose data runs past the end of the file.
    let mut ico = vec![0, 0, 1, 0, 1, 0];
    ico.extend_from_slice(&[16, 16, 0, 0, 1, 0, 32, 0]);
    ico.extend_from_slice(&100u32.to_le_bytes());
    ico.extend_from_slice(&22u32.to_le_bytes());
    assert!(windows_res::push_icon(&mut res, &ico).is_err());
    assert_eq!(res, before);
}

#[test]
fn the_version_resource_is_rt_version_id_1() {
    let resource = version_resource(&version("1.2.3"));
    assert_eq!(resource.kind, RT_VERSION);
    assert_eq!(resource.id, VERSION_ID);
    assert_eq!(resource.flags, 0x0030);
    assert_eq!(resource.language, 0);
    assert_eq!(
        resource.data,
        windows_res::version_info(
            &version("1.2.3"),
            &windows_res::serialist_strings(&version("1.2.3"))
        )
    );
}

#[test]
fn vs_versioninfo_header_and_fixed_file_info_field_by_field() {
    let data = version_resource(&version("1.2.3")).data;
    // VS_VERSIONINFO: wLength is the whole tree; wValueLength is
    // sizeof(VS_FIXEDFILEINFO); wType 0 (binary).
    assert_eq!(usize::from(u16_at(&data, 0)), data.len());
    assert_eq!(u16_at(&data, 2), 52);
    assert_eq!(u16_at(&data, 4), 0);
    // szKey at 6: 15 characters and a NUL, 32 bytes; Padding1 to offset 40.
    assert_eq!(data[6..38], utf16z("VS_VERSION_INFO"));
    assert_eq!(data[38..40], [0, 0]);
    // VS_FIXEDFILEINFO at 40, thirteen DWORDs.
    let fixed: Vec<u32> = (0..13).map(|i| u32_at(&data, 40 + 4 * i)).collect();
    assert_eq!(
        fixed,
        [
            FIXED_SIGNATURE, // dwSignature 0xFEEF04BD
            0x0001_0000,     // dwStrucVersion 1.0
            0x0001_0002,     // dwFileVersionMS: 1.2
            0x0003_0000,     // dwFileVersionLS: 3.0
            0x0001_0002,     // dwProductVersionMS
            0x0003_0000,     // dwProductVersionLS
            0x0000_003F,     // dwFileFlagsMask: VS_FFI_FILEFLAGSMASK
            0,               // dwFileFlags
            0x0004_0004,     // dwFileOS: VOS_NT_WINDOWS32
            0x0000_0001,     // dwFileType: VFT_APP
            0,               // dwFileSubtype
            0,               // dwFileDateMS
            0,               // dwFileDateLS
        ]
    );
    // The children start right after: 40 + 52 = 92 is already on a 4-byte boundary.
    // StringFileInfo at 92: wValueLength 0, wType 1, then its key.
    assert_eq!(u16_at(&data, 94), 0);
    assert_eq!(u16_at(&data, 96), 1);
    assert_eq!(data[98..98 + 30], utf16z("StringFileInfo"));
}

#[test]
fn var_file_info_matches_the_documented_bytes() {
    let tree = node(&version_resource(&version("0.2.0")).data);
    assert_eq!(tree.children.len(), 2);
    assert_eq!(tree.children[0].key, "StringFileInfo");
    assert_eq!(tree.children[1].key, "VarFileInfo");

    // VarFileInfo: 6 + 24 ("VarFileInfo" and NUL) = 30, padded to 32, then the Var.
    // Var: 6 + 24 ("Translation" and NUL) = 30, padded to 32, then one DWORD: 36 bytes.
    let data = version_resource(&version("0.2.0")).data;
    let mut var = Vec::new();
    var.extend_from_slice(&36u16.to_le_bytes()); // wLength
    var.extend_from_slice(&4u16.to_le_bytes()); // wValueLength: bytes
    var.extend_from_slice(&0u16.to_le_bytes()); // wType: binary
    var.extend_from_slice(&utf16z("Translation"));
    var.extend_from_slice(&[0, 0]); // Padding
    var.extend_from_slice(&[0x09, 0x04, 0xB0, 0x04]); // MAKELONG(0x0409, 1200)
    let mut var_file_info = Vec::new();
    var_file_info.extend_from_slice(&68u16.to_le_bytes()); // wLength
    var_file_info.extend_from_slice(&0u16.to_le_bytes()); // wValueLength
    var_file_info.extend_from_slice(&1u16.to_le_bytes()); // wType
    var_file_info.extend_from_slice(&utf16z("VarFileInfo"));
    var_file_info.extend_from_slice(&[0, 0]); // Padding
    var_file_info.extend_from_slice(&var);
    assert!(data.ends_with(&var_file_info));
    assert_eq!(data.len() % 4, 0);
}

#[test]
fn the_string_table_holds_serialist_s_strings() {
    let tree = node(&version_resource(&version("0.2.0")).data);
    let string_file_info = tree.child("StringFileInfo");
    assert_eq!(
        (string_file_info.value_length, string_file_info.kind),
        (0, 1)
    );
    assert_eq!(string_file_info.children.len(), 1);

    // The table's key is the language and code page, as eight hex digits.
    let key = format!("{LANGUAGE:04X}{CODE_PAGE:04X}");
    assert_eq!(key, "040904B0");
    let table = string_file_info.child(&key);
    assert_eq!((table.value_length, table.kind), (0, 1));

    let expected = [
        ("CompanyName", COMPANY_NAME),
        ("FileDescription", PRODUCT_NAME),
        ("FileVersion", "0.2.0"),
        ("InternalName", "serialist"),
        ("LegalCopyright", COPYRIGHT),
        ("OriginalFilename", EXE_NAME),
        ("ProductName", PRODUCT_NAME),
        ("ProductVersion", "0.2.0"),
    ];
    assert_eq!(table.children.len(), expected.len());
    for (key, value) in expected {
        let string = table.child(key);
        assert_eq!(string.text(), value);
        // wValueLength counts UTF-16 units with the NUL.
        assert_eq!(
            usize::from(string.value_length),
            value.encode_utf16().count() + 1
        );
    }

    // The translation names the table.
    let translation = tree.child("VarFileInfo").child("Translation");
    assert_eq!((translation.value_length, translation.kind), (4, 0));
    assert_eq!(
        translation.value,
        [LANGUAGE.to_le_bytes(), CODE_PAGE.to_le_bytes()].concat()
    );
}

#[test]
fn a_pre_release_keeps_its_suffix_in_text_and_sets_the_flag() {
    let rc = version("0.3.0-rc.1");
    assert!(rc.pre_release);
    let data = version_resource(&rc).data;
    assert_eq!(u32_at(&data, 40 + 4 * 7), FF_PRERELEASE, "dwFileFlags");
    assert_eq!(
        u32_at(&data, 40 + 4 * 2),
        0x0000_0003,
        "dwFileVersionMS: 0.3"
    );
    let tree = node(&data);
    let table = &tree.child("StringFileInfo").children[0];
    assert_eq!(table.child("FileVersion").text(), "0.3.0-rc.1");
    assert_eq!(table.child("ProductVersion").text(), "0.3.0-rc.1");
}

#[test]
fn a_version_part_must_fit_16_bits() {
    assert!(Version::from_cargo("1", "70000", "0", "", "1.70000.0").is_err());
    assert!(Version::from_cargo("1", "x", "0", "", "1.x.0").is_err());
    let max = Version::from_cargo("65535", "65535", "65535", "", "65535.65535.65535").unwrap();
    assert_eq!(max.ms_ls(), (0xFFFF_FFFF, 0xFFFF_0000));
}

#[test]
fn this_package_s_version_fits() {
    let version = Version::from_cargo(
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
        env!("CARGO_PKG_VERSION_PATCH"),
        env!("CARGO_PKG_VERSION_PRE"),
        env!("CARGO_PKG_VERSION"),
    )
    .unwrap();
    assert_eq!(version.text, env!("CARGO_PKG_VERSION"));
}

#[test]
fn the_strings_match_the_other_packages() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let plist = std::fs::read_to_string(root.join("packaging/macos/Info.plist.in")).unwrap();
    assert!(plist.contains(&format!("<string>{COPYRIGHT}</string>")));
    assert!(plist.contains(&format!("<string>{PRODUCT_NAME}</string>")));
    let wxs = std::fs::read_to_string(root.join("packaging/windows/main.wxs")).unwrap();
    assert!(wxs.contains(&format!("Manufacturer='{COMPANY_NAME}'")));
    assert!(wxs.contains(&format!("Name='{PRODUCT_NAME}'")));
    assert!(wxs.contains(&format!("Name='{EXE_NAME}'")));
}
