//! Fetches Microsoft's ConPTY for Windows targets, to embed in the library.
//!
//! Windows' own ConPTY re-renders a program's output. The one Windows
//! Terminal ships (`conpty.dll` and its `OpenConsole.exe` host) passes it
//! through untouched, and Microsoft publishes it as the NuGet package
//! `Microsoft.Windows.Console.ConPTY`. A pinned version of that package is
//! downloaded, checked against its SHA-256, and the two files for the target
//! architecture are unpacked into `OUT_DIR` for `include_bytes!`.
//!
//! Offline builds can point `TERMHOG_CONPTY_NUPKG` at a copy of the package,
//! which is checked the same way.

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The package version, and the SHA-256 of its `.nupkg`.
const VERSION: &str = "1.25.260930003";
const SHA256: &str = "02b07b349af66d801159bdf9e440d4a1ce78bb951f37fc8609731665afdae7ee";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=TERMHOG_CONPTY_NUPKG");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    println!("cargo:rustc-env=TERMHOG_CONPTY_VERSION={VERSION}");
    // docs.rs builds without network access and never runs the code.
    if env::var_os("DOCS_RS").is_some() {
        fs::write(out.join("conpty.dll"), b"").unwrap();
        fs::write(out.join("OpenConsole.exe"), b"").unwrap();
        return;
    }
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x64",
        Ok("aarch64") => "arm64",
        Ok("x86") => "x86",
        other => panic!("termhog: Microsoft's ConPTY has no build for {other:?}"),
    };
    let package = package(&out);
    let files = [
        (
            format!("runtimes/win-{arch}/native/conpty.dll"),
            "conpty.dll",
        ),
        (
            format!("build/native/runtimes/{arch}/OpenConsole.exe"),
            "OpenConsole.exe",
        ),
    ];
    for (entry, name) in files {
        let bytes = unzip(&package, &entry)
            .unwrap_or_else(|| panic!("termhog: {entry} is missing from the ConPTY package"));
        fs::write(out.join(name), bytes).unwrap();
    }
}

/// The verified package: from `TERMHOG_CONPTY_NUPKG`, a copy cached in
/// `OUT_DIR` by an earlier build, or nuget.org.
fn package(out: &Path) -> Vec<u8> {
    if let Some(path) = env::var_os("TERMHOG_CONPTY_NUPKG") {
        let bytes = fs::read(&path)
            .unwrap_or_else(|e| panic!("termhog: reading TERMHOG_CONPTY_NUPKG ({path:?}): {e}"));
        return verified(bytes, "TERMHOG_CONPTY_NUPKG");
    }
    let cached = out.join(format!("conpty-{VERSION}.nupkg"));
    if let Ok(bytes) = fs::read(&cached) {
        if sha256(&bytes) == SHA256 {
            return bytes;
        }
    }
    let url = format!(
        "https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/{VERSION}/microsoft.windows.console.conpty.{VERSION}.nupkg"
    );
    let bytes = download(&url).unwrap_or_else(|e| {
        panic!(
            "termhog: downloading Microsoft's ConPTY from {url} failed: {e}\n\
             For an offline build, download it elsewhere and set TERMHOG_CONPTY_NUPKG to its path."
        )
    });
    let bytes = verified(bytes, &url);
    fs::write(&cached, &bytes).unwrap();
    bytes
}

/// Fetch `url` with curl, which Windows 10 and later ship, as do most
/// systems that cross-compile. It honors the usual proxy variables.
fn download(url: &str) -> Result<Vec<u8>, String> {
    let output = std::process::Command::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location"])
        .args(["--retry", "3", url])
        .output()
        .map_err(|e| format!("couldn't run curl: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(output.stdout)
}

/// `bytes`, if they're the pinned package. Panics otherwise.
fn verified(bytes: Vec<u8>, source: &str) -> Vec<u8> {
    let actual = sha256(&bytes);
    assert!(
        actual == SHA256,
        "termhog: the ConPTY package from {source} has SHA-256 {actual}, expected {SHA256}"
    );
    bytes
}

/// The contents of `name` in the zip archive `zip` (a `.nupkg` is one), or
/// `None` if it's not there. Only what NuGet packages use is supported:
/// stored and deflated entries, no ZIP64.
fn unzip(zip: &[u8], name: &str) -> Option<Vec<u8>> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(zip.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(zip.get(at..at + 4)?.try_into().ok()?));
    // The end of central directory record is last, before a comment of at
    // most 64 KiB.
    let floor = zip.len().saturating_sub(22 + 0xFFFF);
    let end = (floor..zip.len().saturating_sub(21))
        .rev()
        .find(|&at| u32_at(at) == Some(0x0605_4b50))?;
    let count = u16_at(end + 10)?;
    let mut at = u32_at(end + 16)? as usize;
    for _ in 0..count {
        if u32_at(at)? != 0x0201_4b50 {
            return None;
        }
        let method = u16_at(at + 10)?;
        let compressed = u32_at(at + 20)? as usize;
        let name_len = u16_at(at + 28)? as usize;
        let extra_len = u16_at(at + 30)? as usize;
        let comment_len = u16_at(at + 32)? as usize;
        let local = u32_at(at + 42)? as usize;
        let entry = zip.get(at + 46..at + 46 + name_len)?;
        at += 46 + name_len + extra_len + comment_len;
        if entry != name.as_bytes() {
            continue;
        }
        // The data follows the local header, whose name and extra field
        // lengths can differ from the central directory's.
        if u32_at(local)? != 0x0403_4b50 {
            return None;
        }
        let start = local + 30 + u16_at(local + 26)? as usize + u16_at(local + 28)? as usize;
        let data = zip.get(start..start + compressed)?;
        return match method {
            0 => Some(data.to_vec()),
            8 => {
                let mut bytes = Vec::new();
                flate2::read::DeflateDecoder::new(data)
                    .read_to_end(&mut bytes)
                    .ok()?;
                Some(bytes)
            }
            _ => None,
        };
    }
    None
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
