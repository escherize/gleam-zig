// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 The Gleam contributors

//! Resolve the pinned zig toolchain for `--target zig` commands.
//!
//! Resolution order in [`ensure_zig`]:
//! 1. The `GLEAM_ZIG` environment variable (explicit override).
//! 2. A previously fetched copy in gleam's global cache.
//! 3. A `zig` on PATH reporting exactly the pinned version.
//! 4. Download from ziglang.org, verify its sha256, and extract into the cache.

use std::process::Command;

use camino::Utf8PathBuf;
use gleam_core::{Error, io::HttpClient as _};
use sha2::{Digest, Sha256};

use crate::{cli, http::HttpClient};

pub const ZIG_VERSION: &str = "0.16.0";

#[derive(Debug, PartialEq, Eq)]
/// One supported `(arch, os)` pair with its ziglang.org archive metadata.
struct Platform {
    arch: &'static str,
    os: &'static str,
    /// Archive extension as served by ziglang.org.
    extension: &'static str,
    sha256: &'static str,
}

/// Checksums for 0.16.0 as published at <https://ziglang.org/download/index.json>.
const PLATFORMS: &[Platform] = &[
    Platform {
        arch: "x86_64",
        os: "linux",
        extension: "tar.xz",
        sha256: "70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00",
    },
    Platform {
        arch: "aarch64",
        os: "linux",
        extension: "tar.xz",
        sha256: "ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17",
    },
    Platform {
        arch: "x86_64",
        os: "macos",
        extension: "tar.xz",
        sha256: "0387557ed1877bc6a2e1802c8391953baddba76081876301c522f52977b52ba7",
    },
    Platform {
        arch: "aarch64",
        os: "macos",
        extension: "tar.xz",
        sha256: "b23d70deaa879b5c2d486ed3316f7eaa53e84acf6fc9cc747de152450d401489",
    },
    Platform {
        arch: "x86_64",
        os: "windows",
        extension: "zip",
        sha256: "68659eb5f1e4eb1437a722f1dd889c5a322c9954607f5edcf337bc3684a75a7e",
    },
    Platform {
        arch: "aarch64",
        os: "windows",
        extension: "zip",
        sha256: "aee38316ee4111717900f45dd3130145c39289e105541d737eb8c5ed653c78ef",
    },
];

impl Platform {
    fn triple(&self) -> String {
        format!("{}-{}", self.arch, self.os)
    }

    fn directory_name(&self) -> String {
        format!("zig-{}-{ZIG_VERSION}", self.triple())
    }

    fn archive_name(&self) -> String {
        format!("{}.{}", self.directory_name(), self.extension)
    }

    fn url(&self) -> String {
        format!(
            "https://ziglang.org/download/{ZIG_VERSION}/{}",
            self.archive_name()
        )
    }

    fn binary_name(&self) -> &'static str {
        if self.os == "windows" {
            "zig.exe"
        } else {
            "zig"
        }
    }
}

fn platform(arch: &str, os: &str) -> Option<&'static Platform> {
    PLATFORMS
        .iter()
        .find(|platform| platform.arch == arch && platform.os == os)
}

fn unsupported_platform_error() -> Error {
    Error::ZigToolchain {
        action: "resolving".into(),
        detail: format!(
            "no prebuilt zig {ZIG_VERSION} for this platform; supported: {}",
            PLATFORMS
                .iter()
                .map(Platform::triple)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Resolve the pinned zig binary. GLEAM_ZIG wins; else cache; else a PATH
/// probe; else download+verify+extract into the cache.
pub fn ensure_zig() -> Result<Utf8PathBuf, Error> {
    if let Ok(path) = std::env::var("GLEAM_ZIG") {
        return Ok(path.into());
    }

    let cache_root = gleam_core::paths::default_global_gleam_cache().join("zig");
    let platform = platform(std::env::consts::ARCH, std::env::consts::OS)
        .ok_or_else(unsupported_platform_error)?;
    let directory = cache_root.join(platform.directory_name());
    let binary = directory.join(platform.binary_name());
    if binary.exists() {
        return Ok(binary);
    }

    if let Some(zig) = path_zig_at_pinned_version() {
        return Ok(zig);
    }

    download_and_install(platform, &cache_root, &directory)?;
    Ok(binary)
}

/// A `zig` on PATH is only trusted when it reports exactly the pinned version;
/// anything else falls through to download rather than being used silently.
fn path_zig_at_pinned_version() -> Option<Utf8PathBuf> {
    let output = Command::new("zig").arg("version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout);
    (version.trim() == ZIG_VERSION).then(|| Utf8PathBuf::from("zig"))
}

fn download_and_install(
    platform: &Platform,
    cache_root: &Utf8PathBuf,
    directory: &Utf8PathBuf,
) -> Result<(), Error> {
    cli::print_downloading_zig(ZIG_VERSION);

    let bytes = download(platform)?;
    verify(platform, &bytes)?;

    // Temp state lives next to the final directory so renames stay on one
    // filesystem and concurrent processes converge instead of clobbering.
    let _ = std::fs::create_dir_all(cache_root);
    let archive = cache_root.join(format!("{}.tmp", platform.archive_name()));
    crate::fs::write_bytes(&archive, &bytes)?;

    let extracting = Utf8PathBuf::from(format!("{directory}.extracting"));
    let _ = std::fs::remove_dir_all(&extracting);
    if let Err(error) = std::fs::create_dir_all(extracting.as_std_path()) {
        let _ = std::fs::remove_file(&archive);
        return Err(write_error("extracting", &extracting, error));
    }

    let status = Command::new("tar")
        .arg("-xf")
        .arg(archive.as_std_path())
        .arg("-C")
        .arg(extracting.as_std_path())
        .status();
    match status {
        Ok(status) if status.success() => {}
        _ => {
            best_effort_cleanup(&extracting, &archive);
            return Err(Error::ZigToolchain {
                action: "extracting".into(),
                detail: format!(
                    "`tar -xf {}` failed; install zig {ZIG_VERSION} manually from \
                     https://ziglang.org/download/{ZIG_VERSION}/ and set GLEAM_ZIG",
                    platform.archive_name()
                ),
            });
        }
    }

    if directory.exists() {
        // A concurrent process won the race; keep its copy.
        best_effort_cleanup(&extracting, &archive);
    } else {
        let extracted = extracting.join(platform.directory_name());
        if let Err(error) = std::fs::rename(extracted.as_std_path(), directory.as_std_path()) {
            best_effort_cleanup(&extracting, &archive);
            return Err(write_error("extracting", directory, error));
        }
        let _ = std::fs::remove_dir_all(&extracting);
        let _ = std::fs::remove_file(&archive);
    }
    Ok(())
}

fn download(platform: &Platform) -> Result<Vec<u8>, Error> {
    let runtime = tokio::runtime::Runtime::new().expect("Unable to start Tokio async runtime");
    let client = HttpClient::new();
    let request = http::Request::get(platform.url())
        .body(Vec::new())
        .expect("request should be valid");
    let response = runtime.block_on(client.send(request))?;
    if !response.status().is_success() {
        return Err(Error::ZigToolchain {
            action: "downloading".into(),
            detail: format!("{} for {}", response.status(), platform.url()),
        });
    }
    Ok(response.into_body())
}

fn verify(platform: &Platform, bytes: &[u8]) -> Result<(), Error> {
    let digest = Sha256::digest(bytes);
    let actual: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    if actual != platform.sha256 {
        return Err(Error::ZigToolchain {
            action: "verifying".into(),
            detail: format!("expected {}, got {}", platform.sha256, actual),
        });
    }
    Ok(())
}

fn write_error(action: &str, path: &Utf8PathBuf, error: std::io::Error) -> Error {
    Error::ZigToolchain {
        action: action.into(),
        detail: format!("{path}: {error}"),
    }
}

fn best_effort_cleanup(extracting: &Utf8PathBuf, archive: &Utf8PathBuf) {
    let _ = std::fs::remove_dir_all(extracting);
    let _ = std::fs::remove_file(archive);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_matches_exactly_the_six_supported_pairs() {
        for expected in PLATFORMS {
            assert_eq!(platform(expected.arch, expected.os), Some(expected));
        }
        assert_eq!(platform("wasm32", "linux"), None);
        assert_eq!(platform("x86_64", "freebsd"), None);
        assert_eq!(platform("", ""), None);
    }

    #[test]
    fn archive_naming_matches_ziglang_org_layout() {
        let linux = Platform {
            arch: "x86_64",
            os: "linux",
            extension: "tar.xz",
            sha256: "",
        };
        assert_eq!(linux.archive_name(), "zig-x86_64-linux-0.16.0.tar.xz");
        assert_eq!(
            linux.url(),
            "https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz"
        );
        let windows = Platform {
            arch: "aarch64",
            os: "windows",
            extension: "zip",
            sha256: "",
        };
        assert_eq!(
            windows.url(),
            "https://ziglang.org/download/0.16.0/zig-aarch64-windows-0.16.0.zip"
        );
    }

    #[test]
    fn embedded_checksums_are_64_lowercase_hex_characters() {
        for expected in PLATFORMS {
            assert_eq!(expected.sha256.len(), 64, "{}", expected.sha256);
            assert!(
                expected
                    .sha256
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{}",
                expected.sha256
            );
        }
    }
}
