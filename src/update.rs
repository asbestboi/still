//! Update checks against the project's GitHub releases, and installing a
//! newer build over the running one (Windows .exe, Linux AppImage, macOS app).
//!
//! The check is one anonymous request to the GitHub API. Nothing is sent
//! besides a generic User-Agent naming the app and its version.

use serde::Deserialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The GitHub repository releases come from.
pub const REPO: &str = "asbestboi/still";
/// The running version.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// Lower-case hex SHA-256 when GitHub lists one.
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub notes: String,
    /// The release page, for "What's New" and manual downloads.
    pub page: String,
    /// The build for this computer, if the release has one.
    pub asset: Option<Asset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

/// "v0.4.1" → [0, 4, 1]. A pre-release suffix ("-beta") is ignored.
pub fn parse_version(s: &str) -> Option<Vec<u64>> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next()?;
    let v: Option<Vec<u64>> = core.split('.').map(|p| p.parse().ok()).collect();
    v.filter(|v| !v.is_empty())
}

/// Is `candidate` a later version than `current`?
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(mut a), Some(mut b)) => {
            let n = a.len().max(b.len());
            a.resize(n, 0);
            b.resize(n, 0);
            a > b
        }
        _ => false,
    }
}

/// The release file built for this system.
pub fn asset_name() -> Option<&'static str> {
    if cfg!(windows) {
        Some("Still-windows-x64.exe")
    } else if cfg!(target_os = "macos") {
        Some("Still-macos.dmg")
    } else if cfg!(target_os = "linux") {
        Some("Still-x86_64.AppImage")
    } else {
        None
    }
}

/// The installed copy Still can replace by itself: the .exe on Windows, the
/// AppImage on Linux, the .app bundle on macOS. `None` for builds run from
/// source (`cargo run`, `install.sh`): those update with git.
pub fn install_target() -> Option<PathBuf> {
    if cfg!(target_os = "linux") {
        return std::env::var_os("APPIMAGE").map(PathBuf::from).filter(|p| p.is_file());
    }
    let exe = std::env::current_exe().ok()?;
    if cfg!(target_os = "macos") {
        return exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(|p| p.to_path_buf());
    }
    if cfg!(windows) && !exe.components().any(|c| c.as_os_str() == "target") {
        return Some(exe);
    }
    None
}

pub fn can_self_update() -> bool {
    install_target().is_some() && asset_name().is_some()
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(30))
        .user_agent(&format!("Still/{CURRENT}"))
        .build()
}

fn release_from(gh: GhRelease) -> Release {
    let want = asset_name();
    let asset = gh.assets.into_iter().find(|a| Some(a.name.as_str()) == want).map(|a| Asset {
        name: a.name,
        url: a.browser_download_url,
        size: a.size,
        sha256: a.digest.and_then(|d| d.strip_prefix("sha256:").map(|h| h.to_lowercase())),
    });
    Release {
        version: gh.tag_name.trim_start_matches(['v', 'V']).to_string(),
        notes: gh.body.unwrap_or_default(),
        page: gh.html_url,
        asset,
    }
}

/// The newest published release, if it is newer than this one.
pub fn check() -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = agent().get(&url).set("Accept", "application/vnd.github+json").call().map_err(|e| match e {
        ureq::Error::Status(404, _) => "no releases published yet".to_string(),
        ureq::Error::Status(code, _) => format!("GitHub answered {code}"),
        ureq::Error::Transport(t) => format!("no connection ({})", t.kind()),
    })?;
    let gh: GhRelease = resp.into_json().map_err(|e| format!("unexpected answer from GitHub: {e}"))?;
    if gh.draft || gh.prerelease || !is_newer(&gh.tag_name, CURRENT) {
        return Ok(None);
    }
    Ok(Some(release_from(gh)))
}

/// Download `asset` to `dest`, reporting progress (0..1) and checking its
/// size and SHA-256.
fn download(asset: &Asset, dest: &Path, progress: &dyn Fn(f32)) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let resp = agent().get(&asset.url).call().map_err(|e| format!("download failed: {e}"))?;
    let total = resp.header("Content-Length").and_then(|v| v.parse::<u64>().ok()).unwrap_or(asset.size).max(1);
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| format!("can't write the update: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut got = 0u64;
    let mut last = 0.0f32;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("download interrupted: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| format!("can't write the update: {e}"))?;
        hasher.update(&buf[..n]);
        got += n as u64;
        let f = (got as f32 / total as f32).min(1.0);
        if f - last > 0.01 {
            last = f;
            progress(f);
        }
    }
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    let fail = |msg: String| {
        let _ = std::fs::remove_file(dest);
        Err(msg)
    };
    if asset.size > 0 && got != asset.size {
        return fail(format!("the download is incomplete ({got} of {} bytes)", asset.size));
    }
    if let Some(want) = &asset.sha256 {
        let have: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if *want != have {
            return fail("the download is damaged (checksum mismatch)".into());
        }
    }
    progress(1.0);
    Ok(())
}

/// Download the release and put it in place of the running copy. Returns
/// what to start to run the new version. The running app keeps working;
/// the new one takes over on restart.
pub fn install(rel: &Release, progress: &dyn Fn(f32)) -> Result<PathBuf, String> {
    let asset = rel.asset.as_ref().ok_or("this release has no download for your system")?;
    let target = install_target().ok_or("this copy of Still wasn't installed from a release, so it can't update itself")?;
    let dir = target.parent().ok_or("unexpected install location")?.to_path_buf();

    if cfg!(target_os = "macos") {
        return install_macos(asset, &target, progress);
    }
    // download next to the target, so the swap is a rename on one disk
    let part = dir.join(format!(".{}.download", asset.name));
    download(asset, &part, progress)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    if cfg!(windows) {
        // a running .exe can be renamed but not overwritten
        let old = target.with_extension("exe.old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&target, &old).map_err(|e| format!("can't replace Still: {e}"))?;
        if let Err(e) = std::fs::rename(&part, &target) {
            let _ = std::fs::rename(&old, &target);
            let _ = std::fs::remove_file(&part);
            return Err(format!("can't replace Still: {e}"));
        }
    } else if let Err(e) = std::fs::rename(&part, &target) {
        let _ = std::fs::remove_file(&part);
        return Err(format!("can't replace Still: {e}"));
    }
    Ok(target)
}

fn install_macos(asset: &Asset, bundle: &Path, progress: &dyn Fn(f32)) -> Result<PathBuf, String> {
    use std::process::Command;
    let work = std::env::temp_dir().join(format!("still-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let dmg = work.join(&asset.name);
    download(asset, &dmg, progress)?;
    let mnt = work.join("mnt");
    std::fs::create_dir_all(&mnt).map_err(|e| e.to_string())?;
    let ok = |c: &mut Command| c.status().map(|s| s.success()).unwrap_or(false);
    if !ok(Command::new("hdiutil").args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"]).arg(&mnt).arg(&dmg)) {
        return Err("couldn't open the downloaded disk image".into());
    }
    let result = (|| {
        let src = std::fs::read_dir(&mnt)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .ok_or("the disk image has no app in it")?;
        let parent = bundle.parent().ok_or("unexpected install location")?;
        let fresh = parent.join(".Still-update.app");
        let old = parent.join(".Still-old.app");
        let _ = std::fs::remove_dir_all(&fresh);
        let _ = std::fs::remove_dir_all(&old);
        if !ok(Command::new("ditto").arg(&src).arg(&fresh)) {
            return Err("couldn't copy the new version (is the Applications folder writable?)".to_string());
        }
        std::fs::rename(bundle, &old).map_err(|e| format!("can't replace Still: {e}"))?;
        if let Err(e) = std::fs::rename(&fresh, bundle) {
            let _ = std::fs::rename(&old, bundle);
            return Err(format!("can't replace Still: {e}"));
        }
        let _ = std::fs::remove_dir_all(&old);
        Ok(bundle.to_path_buf())
    })();
    let _ = Command::new("hdiutil").args(["detach", "-quiet"]).arg(&mnt).status();
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// Start the freshly installed version.
pub fn relaunch(target: &Path) -> Result<(), String> {
    let r = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg("-n").arg(target).spawn()
    } else {
        std::process::Command::new(target).spawn()
    };
    r.map(|_| ()).map_err(|e| e.to_string())
}

/// Remove what a previous update left behind (Windows keeps the old .exe
/// until the next start).
pub fn clean_up() {
    if cfg!(windows)
        && let Some(t) = install_target()
    {
        let _ = std::fs::remove_file(t.with_extension("exe.old"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert_eq!(parse_version("v0.10.2"), Some(vec![0, 10, 2]));
        assert_eq!(parse_version("1.2.3-beta.1"), Some(vec![1, 2, 3]));
        assert!(parse_version("latest").is_none());
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("0.4", "0.3.9"));
        assert!(!is_newer("v0.3.0", "0.3.0"));
        assert!(!is_newer("0.3.0", "0.3"));
        assert!(!is_newer("0.2.9", "0.3.0"));
        assert!(!is_newer("nightly", "0.3.0"));
    }

    #[test]
    fn picks_this_systems_build_and_its_checksum() {
        let gh: GhRelease = serde_json::from_str(
            r#"{"tag_name":"v9.0.0","html_url":"https://example.invalid/r","body":"notes","draft":false,"prerelease":false,
               "assets":[{"name":"Still-windows-x64.exe","browser_download_url":"https://example.invalid/w","size":10,"digest":"sha256:ABCD"},
                         {"name":"Still-x86_64.AppImage","browser_download_url":"https://example.invalid/l","size":11,"digest":null},
                         {"name":"Still-macos.dmg","browser_download_url":"https://example.invalid/m","size":12}]}"#,
        )
        .unwrap();
        let r = release_from(gh);
        assert_eq!(r.version, "9.0.0");
        let a = r.asset.expect("an asset for this system");
        assert_eq!(Some(a.name.as_str()), asset_name());
        if cfg!(windows) {
            assert_eq!(a.sha256.as_deref(), Some("abcd"));
        }
    }

    /// Talks to GitHub; run by hand with `cargo test live_update_check -- --ignored`.
    #[test]
    #[ignore]
    fn live_update_check() {
        let r = check();
        eprintln!("{r:?}");
        assert!(r.is_ok());
    }
}
