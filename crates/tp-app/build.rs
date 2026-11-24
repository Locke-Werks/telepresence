fn main() {
    #[cfg(feature = "gui")]
    {
        println!("cargo::rerun-if-changed=src/video_sink.h");
        use cxx_qt_build::{CxxQtBuilder, QmlFile, QmlModule};
        let files = vec![QmlFile::from("qml/Main.qml"), QmlFile::from("qml/SmoothWheel.qml")];
        CxxQtBuilder::new_qml_module(QmlModule::new("TelePresence").qml_files(files))
            .qt_module("Quick")
            .qt_module("QuickControls2")
            .qt_module("Multimedia")
            .include_dir("src")
            .cpp_file("src/video_sink.cpp")
            .files(["src/bridge.rs"])
            .build();
    }

    // Build scripts run on the host, so the target decides, not cfg!(windows).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        windows_resource::embed();
    }
}

/// Icon and version resource for telepresence.exe. Compiled with the SDK's
/// rc.exe directly rather than through a crate, so the build adds no
/// dependency, and linked into the binary only (tests and the lib skip it).
mod windows_resource {
    use std::path::{Path, PathBuf};

    pub fn embed() {
        let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let icon = manifest.join("../../assets/telepresence.ico");
        println!("cargo::rerun-if-changed={}", icon.display());
        println!("cargo::rerun-if-env-changed=RC");
        let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());

        let num = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u16>().ok()).unwrap_or(0);
        let (maj, min, pat) =
            (num("CARGO_PKG_VERSION_MAJOR"), num("CARGO_PKG_VERSION_MINOR"), num("CARGO_PKG_VERSION_PATCH"));
        let version = std::env::var("CARGO_PKG_VERSION").unwrap();
        let icon_path = icon.canonicalize().unwrap_or(icon).display().to_string();
        // rc.exe reads \\?\ prefixes and single backslashes badly.
        let icon_path = icon_path.trim_start_matches(r"\\?\").replace('\\', "\\\\");

        // Ordinal 1 first so Explorer uses it as the file icon. IDI_ICON1 is the
        // name Qt's Windows platform plugin loads for every window class, which
        // is what puts this icon on the title bar and taskbar at runtime
        // without any code in the app.
        let rc = format!(
            r#"#pragma code_page(65001)
1 ICON "{icon_path}"
IDI_ICON1 ICON "{icon_path}"

1 VERSIONINFO
FILEVERSION {maj},{min},{pat},0
PRODUCTVERSION {maj},{min},{pat},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "Locke Werks"
            VALUE "FileDescription", "TelePresence"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "telepresence"
            VALUE "LegalCopyright", "Copyright (C) Locke Werks"
            VALUE "OriginalFilename", "telepresence.exe"
            VALUE "ProductName", "TelePresence"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
        );
        let rc_file = out.join("telepresence.rc");
        let res_file = out.join("telepresence.res");
        std::fs::write(&rc_file, rc).expect("write telepresence.rc");

        let rc_exe = find_rc().expect(
            "rc.exe not found: install the Windows SDK, or set RC to its path",
        );
        let status = std::process::Command::new(&rc_exe)
            .args(["/nologo", "/fo"])
            .arg(&res_file)
            .arg(&rc_file)
            .status()
            .expect("run rc.exe");
        assert!(status.success(), "rc.exe failed on {}", rc_file.display());
        println!("cargo::rustc-link-arg-bin=telepresence={}", res_file.display());
    }

    fn find_rc() -> Option<PathBuf> {
        if let Ok(rc) = std::env::var("RC") {
            return Some(PathBuf::from(rc));
        }
        // A VS developer prompt puts the SDK bin dir on PATH.
        if let Some(p) = std::env::var_os("PATH")
            .iter()
            .flat_map(std::env::split_paths)
            .map(|d| d.join("rc.exe"))
            .find(|p| p.is_file())
        {
            return Some(p);
        }
        // Otherwise the newest installed SDK. Newest first, so a stale SDK
        // left behind by an older Visual Studio does not win.
        let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
        let mut versions: Vec<PathBuf> = std::fs::read_dir(kits)
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("10.")))
            .collect();
        versions.sort_by_key(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.split('.').map(|x| x.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>())
                .unwrap_or_default()
        });
        versions.into_iter().rev().map(|v| v.join(r"x64\rc.exe")).find(|p| p.is_file())
    }
}
