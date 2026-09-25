use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::http;

/// How often the work thread re-checks while running. Kept in memory only:
///
/// the check is cheap and a restart re-checks anyway.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const RELEASES_URL: &str = "https://api.github.com/repos/SnowSquire/twitch-siphon/releases/latest";
const UPDATE_TIMEOUT: Duration = Duration::from_secs(15);
/// Registry key the WiX installer records the install dir under
/// (`packaging/wix/main.wxs` writes `HKCU\Software\Ozeniken\Siphon`).
const INSTALL_KEY: &str = "Software\\Ozeniken\\Siphon";

/// A published release worth offering: the version plus where to get it.
/// `msi_url` is `None` when the release carries no installer asset.
#[derive(Clone, Debug)]
pub struct Release {
    pub version: semver::Version,
    pub msi_url: Option<String>,
    pub page_url: String,
}

#[derive(Debug, serde::Deserialize)]
struct ReleaseJson {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<AssetJson>,
}

#[derive(Debug, serde::Deserialize)]
struct AssetJson {
    name: String,
    browser_download_url: String,
}

/// Decodes one `releases/latest` body. Tags carry a `v` prefix (`v0.1.3`);
/// anything that is not semver after stripping it is refused.
pub fn parse_release(body: &[u8]) -> Result<Release, http::Error> {
    let json: ReleaseJson = serde_json::from_slice(body)?;
    let tag = json.tag_name.strip_prefix('v').unwrap_or(&json.tag_name);
    let version = semver::Version::parse(tag)
        .map_err(|error| format!("bad release tag {}: {error}", json.tag_name))?;
    let msi_url = json
        .assets
        .into_iter()
        .find(|asset| asset.name.to_lowercase().ends_with(".msi"))
        .map(|asset| asset.browser_download_url);
    Ok(Release {
        version,
        msi_url,
        page_url: json.html_url,
    })
}

/// True when the release is strictly newer than this build. An
/// unparseable build version (should not happen; cargo validates it)
/// never offers an update.
pub fn newer_than_current(release: &Release) -> bool {
    match semver::Version::parse(env!("CARGO_PKG_VERSION")) {
        Ok(current) => release.version > current,
        Err(_) => false,
    }
}

/// Fetches the latest published release. Draft releases are invisible to
/// this endpoint, so an update only appears once the release is published.
pub async fn fetch_latest() -> Result<Release, http::Error> {
    fetch_latest_from(RELEASES_URL).await
}

async fn fetch_latest_from(url: &str) -> Result<Release, http::Error> {
    // GitHub rejects API calls without a User-Agent.
    let response = compio::time::timeout(
        UPDATE_TIMEOUT,
        http::client()?
            .get(url)?
            .header("User-Agent", "siphon")?
            .header("Accept", "application/vnd.github+json")?
            .send(),
    )
    .await
    .map_err(|_| "update request timed out")??;
    if !response.status().is_success() {
        return Err(format!("update check returned status {}", response.status()).into());
    }
    let body = compio::time::timeout(UPDATE_TIMEOUT, response.bytes())
        .await
        .map_err(|_| "update body read timed out")??;
    parse_release(&body)
}

/// How this copy was installed: via the per-user MSI or standalone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InstallMode {
    Msi,
    Portable,
}

/// An MSI install lives under the `InstallDir` the installer recorded;
/// anything else (portable exe, dev build) is standalone.
pub fn install_mode() -> InstallMode {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let install_dir = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(INSTALL_KEY)
        .and_then(|key| key.get_value::<String, _>("InstallDir"))
        .ok()
        .map(PathBuf::from);
    match (exe_dir, install_dir) {
        (Some(exe), Some(installed)) if exe.starts_with(&installed) => InstallMode::Msi,
        _ => InstallMode::Portable,
    }
}

/// Downloads the installer to the temp dir. Reuses the streaming fetch,
/// so a failed download never leaves a partial `.msi` behind.
pub async fn download_msi(url: &str, version: &str) -> Result<PathBuf, http::Error> {
    let path = std::env::temp_dir().join(format!("siphon-update-{version}.msi"));
    http::fetch_file(url, &path).await?;
    Ok(path)
}

/// Builds the waiter script: installs the MSI, then reopens the app.
/// Single quotes cover spaces in both paths; embedded quotes are doubled.
fn relaunch_script(msi: &Path, exe: &Path) -> String {
    let quote = |path: &Path| path.display().to_string().replace('\'', "''");
    format!(
        "Start-Process -FilePath 'msiexec.exe' -ArgumentList '/i','{}','/passive' -Wait; Start-Process -FilePath '{}'",
        quote(msi),
        quote(exe),
    )
}

/// Installs the MSI and reopens this app once it finishes. A detached
/// PowerShell waiter outlives this process: it runs `msiexec` to
/// completion, then starts the just-replaced exe (a failed install just
/// reopens the current version). Returns once the waiter is spawned; the
/// caller quits so no files are locked during the upgrade.
pub fn install_msi_and_relaunch(msi: &Path) -> Result<(), http::Error> {
    use std::os::windows::process::CommandExt as _;

    let exe = std::env::current_exe()?;
    std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &relaunch_script(msi, &exe),
        ])
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use super::*;

    const BODY: &[u8] = br#"{
        "tag_name": "v0.2.0",
        "html_url": "https://github.com/SnowSquire/twitch-siphon/releases/tag/v0.2.0",
        "assets": [
            {"name": "Siphon_0.2.0_x64-portable.exe",
             "browser_download_url": "https://example.com/Siphon_0.2.0_x64-portable.exe"},
            {"name": "siphon_0.2.0_x64_en-US.msi",
             "browser_download_url": "https://example.com/siphon.msi"}
        ]
    }"#;

    #[test]
    fn parse_picks_version_msi_and_page() {
        let release = parse_release(BODY).unwrap();
        assert_eq!(release.version, semver::Version::new(0, 2, 0));
        assert_eq!(
            release.msi_url.as_deref(),
            Some("https://example.com/siphon.msi")
        );
        assert_eq!(
            release.page_url,
            "https://github.com/SnowSquire/twitch-siphon/releases/tag/v0.2.0"
        );
    }

    #[test]
    fn parse_without_msi_yields_no_installer() {
        let body = br#"{"tag_name": "v0.2.0", "html_url": "https://example.com/r",
            "assets": [{"name": "notes.txt", "browser_download_url": "https://example.com/n"}]}"#;
        let release = parse_release(body).unwrap();
        assert!(release.msi_url.is_none());
    }

    #[test]
    fn parse_rejects_non_semver_tag() {
        let body = br#"{"tag_name": "nightly", "html_url": "https://example.com/r", "assets": []}"#;
        assert!(parse_release(body).is_err());
    }

    #[test]
    fn newer_tracks_current_build_version() {
        let current = semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let mut next = current.clone();
        next.patch += 1;
        let newer = Release {
            version: next,
            msi_url: None,
            page_url: String::new(),
        };
        assert!(newer_than_current(&newer));
        let same = Release {
            version: current,
            msi_url: None,
            page_url: String::new(),
        };
        assert!(!newer_than_current(&same));
    }

    /// Serves one static HTTP response on loopback; the returned future
    /// must be polled concurrently with the client.
    async fn serve_once(response: Vec<u8>) -> (u16, impl Future<Output = ()>) {
        use compio::io::{AsyncRead as _, AsyncWriteExt as _};

        let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let serve = async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let res = conn.read([0u8; 1024]).await;
                let n = res.0.unwrap();
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&res.1[..n]);
            }
            conn.write_all(response).await.0.unwrap();
        };
        (port, serve)
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        compio::runtime::Runtime::new().unwrap().block_on(future)
    }

    #[test]
    fn fetch_decodes_release_from_json_body() {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            BODY.len()
        )
        .into_bytes();
        response.extend_from_slice(BODY);

        block_on(async {
            let (port, serve) = serve_once(response).await;
            let url = format!("http://127.0.0.1:{port}/releases/latest");
            let ((), result) = futures_util::join!(serve, fetch_latest_from(&url));
            let release = result.unwrap();
            assert_eq!(release.version, semver::Version::new(0, 2, 0));
            assert_eq!(
                release.msi_url.as_deref(),
                Some("https://example.com/siphon.msi")
            );
        });
    }

    #[test]
    fn relaunch_script_installs_before_reopening() {
        let script = relaunch_script(
            Path::new("C:\\Temp\\siphon-update-0.2.0.msi"),
            Path::new("C:\\Users\\me\\AppData\\Local\\Siphon\\siphon.exe"),
        );
        let install = script.find("msiexec.exe").expect("installs via msiexec");
        let open = script.find("siphon.exe").expect("reopens the app");
        assert!(install < open, "reopen must wait for the install");
        assert!(script.contains("-Wait"), "reopen must wait for msiexec");
    }

    #[test]
    fn relaunch_script_quotes_paths_with_spaces() {
        let script = relaunch_script(
            Path::new("C:\\My Dir\\siphon-update-0.2.0.msi"),
            Path::new("C:\\App Dir\\siphon.exe"),
        );
        assert!(script.contains("'C:\\My Dir\\siphon-update-0.2.0.msi'"));
        assert!(script.contains("'C:\\App Dir\\siphon.exe'"));
    }

    #[test]
    fn fetch_rejects_error_status() {
        let response =
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();

        block_on(async {
            let (port, serve) = serve_once(response).await;
            let url = format!("http://127.0.0.1:{port}/releases/latest");
            let ((), result) = futures_util::join!(serve, fetch_latest_from(&url));
            assert!(result.is_err(), "error status should fail");
        });
    }
}
