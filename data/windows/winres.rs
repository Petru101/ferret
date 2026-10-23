// The version information Windows shows in a program's Properties (and antivirus heuristics
// read: an unknown program without any counts against it), and optionally its icon. Shared by
// build.rs and helper/build.rs through include!; only Windows builds get it. The resource is
// compiled with MinGW's windres: `windres` in MSYS2, `x86_64-w64-mingw32-windres` when cross-
// building from Linux (WINDRES overrides).

fn windows_resources(file: &str, description: &str, icon: Option<&str>) {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=WINDRES");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let part = |name: &str| std::env::var(name).unwrap_or_else(|_| "0".into());
    let numbers = format!("{},{},{},0", part("CARGO_PKG_VERSION_MAJOR"), part("CARGO_PKG_VERSION_MINOR"), part("CARGO_PKG_VERSION_PATCH"));
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut rc = String::new();
    if let Some(icon) = icon {
        let path = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join(icon);
        println!("cargo:rerun-if-changed={}", path.display());
        rc += &format!("1 ICON \"{}\"\n", path.display().to_string().replace('\\', "/"));
    }
    // 0x40004: Windows NT, 32/64-bit; type 1: an application.
    rc += &format!(
        r#"1 VERSIONINFO
FILEVERSION {numbers}
PRODUCTVERSION {numbers}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Petru Mare"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "{file}"
      VALUE "LegalCopyright", "Copyright (C) 2026 Petru Mare. GPL-3.0-or-later."
      VALUE "OriginalFilename", "{file}.exe"
      VALUE "ProductName", "Ferret"
      VALUE "ProductVersion", "{version}"
      VALUE "Comments", "Find and change values in your single-player games. https://github.com/Petru101/ferret"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let rc_path = out.join(format!("{file}.rc"));
    let obj = out.join(format!("{file}-resources.o"));
    std::fs::write(&rc_path, rc).unwrap();
    let windres = std::env::var("WINDRES").unwrap_or_else(|_| {
        if cfg!(windows) { "windres".into() } else { "x86_64-w64-mingw32-windres".into() }
    });
    match std::process::Command::new(&windres).args(["-O", "coff", "-i"]).arg(&rc_path).arg("-o").arg(&obj).status() {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg-bins={}", obj.display()),
        // A build without windres still works, without the details.
        Ok(s) => println!("cargo:warning={windres} failed ({s}): no version information in {file}.exe"),
        Err(e) => println!("cargo:warning={windres} not found ({e}): no version information in {file}.exe"),
    }
}
