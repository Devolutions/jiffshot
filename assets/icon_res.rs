// Included by the app's build script: turns `assets/jiffshot.ico` and the
// package metadata into a compiled resource file holding the program icon and
// `VS_VERSION_INFO`, and hands it to the linker. MSVC's linker takes `.res`
// files directly, so no resource compiler (or Windows SDK) is needed.

use std::path::Path;

/// Writes `ico` and `VS_VERSION_INFO` for `version` as a `.res` file: one
/// `RT_ICON` per image, the `RT_GROUP_ICON` (id 1) that lists them, and
/// `RT_VERSION` (id 1).
fn write_icon_res(ico: &[u8], version: &str, out: &Path) -> std::io::Result<()> {
    let u16_at = |i: usize| u16::from_le_bytes([ico[i], ico[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([ico[i], ico[i + 1], ico[i + 2], ico[i + 3]]);
    let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "not an .ico file");
    if ico.len() < 6 || u16_at(0) != 0 || u16_at(2) != 1 {
        return Err(bad());
    }
    let count = u16_at(4) as usize;
    let mut res = Vec::new();
    // A resource file starts with an empty entry.
    entry(&mut res, 0, 0, 0, &[]);
    let mut group = Vec::new();
    group.extend_from_slice(&[0, 0, 1, 0]);
    group.extend_from_slice(&(count as u16).to_le_bytes());
    for i in 0..count {
        let e = 6 + 16 * i;
        if e + 16 > ico.len() {
            return Err(bad());
        }
        let (size, offset) = (u32_at(e + 8) as usize, u32_at(e + 12) as usize);
        let image = ico.get(offset..offset + size).ok_or_else(bad)?;
        let id = (i + 1) as u16;
        // RT_ICON, moveable and discardable.
        entry(&mut res, 3, id, 0x1010, image);
        // The group entry is the file's directory entry with the id in
        // place of the offset.
        group.extend_from_slice(&ico[e..e + 12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    // RT_GROUP_ICON.
    entry(&mut res, 14, 1, 0x1030, &group);
    // RT_VERSION.
    entry(&mut res, 16, 1, 0x0030, &version_info(version));
    std::fs::write(out, res)
}

/// Builds a `VS_VERSION_INFO` block for `jiffshot.exe` at `version`.
fn version_info(version: &str) -> Vec<u8> {
    let parts = pe_version_parts(version);
    let file_version = format!("{}.{}.{}.{}", parts[0], parts[1], parts[2], parts[3]);
    let mut fixed = Vec::with_capacity(52);
    let ver_ms = (u32::from(parts[0]) << 16) | u32::from(parts[1]);
    let ver_ls = (u32::from(parts[2]) << 16) | u32::from(parts[3]);
    for word in [
        0xFEEF_04BDu32, // dwSignature
        0x0001_0000,    // dwStrucVersion (1.0)
        ver_ms,         // dwFileVersionMS
        ver_ls,         // dwFileVersionLS
        ver_ms,         // dwProductVersionMS
        ver_ls,         // dwProductVersionLS
        0x0000_003F,    // dwFileFlagsMask (VS_FFI_FILEFLAGSMASK)
        0,              // dwFileFlags
        0x0004_0004,    // dwFileOS (VOS_NT_WINDOWS32)
        0x0000_0001,    // dwFileType (VFT_APP)
        0,              // dwFileSubtype
        0,              // dwFileDateMS
        0,              // dwFileDateLS
    ] {
        fixed.extend_from_slice(&word.to_le_bytes());
    }

    let strings = [
        ("CompanyName", "Devolutions Inc."),
        ("FileDescription", env!("CARGO_PKG_DESCRIPTION")),
        ("FileVersion", file_version.as_str()),
        ("InternalName", "jiffshot"),
        ("LegalCopyright", "Copyright (c) Devolutions Inc."),
        ("OriginalFilename", "jiffshot.exe"),
        ("ProductName", "Devolutions Jiffshot"),
        ("ProductVersion", version),
    ];
    let mut string_entries = Vec::with_capacity(strings.len());
    for (key, val) in strings {
        let mut value_bytes = Vec::new();
        for u in val.encode_utf16().chain(std::iter::once(0)) {
            value_bytes.extend_from_slice(&u.to_le_bytes());
        }
        let value_words = (value_bytes.len() / 2) as u16;
        string_entries.push(ver_node(key, 1, value_words, &value_bytes, &[]));
    }
    let string_table = ver_node("040904b0", 1, 0, &[], &string_entries);
    let string_file_info = ver_node("StringFileInfo", 1, 0, &[], &[string_table]);
    // US English (0x0409), Unicode (1200 = 0x04B0).
    let translation = ver_node("Translation", 0, 4, &[0x09, 0x04, 0xB0, 0x04], &[]);
    let var_file_info = ver_node("VarFileInfo", 1, 0, &[], &[translation]);
    ver_node(
        "VS_VERSION_INFO",
        0,
        fixed.len() as u16,
        &fixed,
        &[string_file_info, var_file_info],
    )
}

/// Parses up to four `u16` version components from `version` (stripping any
/// `-prerelease` or `+build` suffix).
fn pe_version_parts(version: &str) -> [u16; 4] {
    let numeric = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = [0u16; 4];
    for (slot, part) in parts.iter_mut().zip(numeric.split('.')) {
        if let Ok(value) = part.parse::<u16>() {
            *slot = value;
        }
    }
    parts
}

/// One node in a `VS_VERSION_INFO` tree: `(wLength, wValueLength, wType,
/// szKey, padding, Value, padding, Children)`.
fn ver_node(key: &str, kind: u16, value_len: u16, value: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
    let mut node = vec![0u8; 6];
    for u in key.encode_utf16().chain(std::iter::once(0)) {
        node.extend_from_slice(&u.to_le_bytes());
    }
    while !node.len().is_multiple_of(4) {
        node.push(0);
    }
    node.extend_from_slice(value);
    for child in children {
        while !node.len().is_multiple_of(4) {
            node.push(0);
        }
        node.extend_from_slice(child);
    }
    let len = node.len() as u16;
    node[0..2].copy_from_slice(&len.to_le_bytes());
    node[2..4].copy_from_slice(&value_len.to_le_bytes());
    node[4..6].copy_from_slice(&kind.to_le_bytes());
    node
}

/// One resource with a numeric type and name, in US English, padded to a
/// 4-byte boundary.
fn entry(res: &mut Vec<u8>, kind: u16, name: u16, flags: u16, data: &[u8]) {
    res.extend_from_slice(&(data.len() as u32).to_le_bytes());
    res.extend_from_slice(&32u32.to_le_bytes());
    for v in [0xFFFF, kind, 0xFFFF, name] {
        res.extend_from_slice(&u16::to_le_bytes(v));
    }
    res.extend_from_slice(&0u32.to_le_bytes());
    res.extend_from_slice(&flags.to_le_bytes());
    res.extend_from_slice(&(if kind == 0 { 0u16 } else { 0x0409 }).to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes());
    res.extend_from_slice(data);
    while !res.len().is_multiple_of(4) {
        res.push(0);
    }
}

/// Embeds `ico` (a path relative to the crate) and `VS_VERSION_INFO` into the
/// executable.
fn embed_icon(ico: &str) {
    println!("cargo:rerun-if-changed={ico}");
    println!("cargo:rerun-if-env-changed=JIFFSHOT_VERSION");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    let version = std::env::var("JIFFSHOT_VERSION")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("CARGO_PKG_VERSION").ok())
        .unwrap_or_else(|| "0.0.0".to_string());
    let bytes = std::fs::read(ico).unwrap_or_else(|e| panic!("reading {ico}: {e}"));
    let out = Path::new(&std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("icon.res");
    write_icon_res(&bytes, &version, &out)
        .unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
    println!("cargo:rustc-link-arg-bins={}", out.display());
}
