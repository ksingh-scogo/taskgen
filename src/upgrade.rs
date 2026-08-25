use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use sha2::{Digest, Sha256};

const RELEASE_BASE_URL: &str = "https://github.com/ksingh-scogo/taskgen/releases/latest/download";
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: usize = 64 * 1024;

#[derive(Debug, PartialEq, Eq)]
enum UpgradeOutcome {
    AlreadyCurrent,
    Installed { bytes: u64 },
}

pub async fn run() -> Result<()> {
    let asset = release_asset(std::env::consts::OS, std::env::consts::ARCH)?;
    let executable = std::env::current_exe().context("failed to locate the taskgen executable")?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .user_agent(format!("taskgen/{} upgrade", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to create the upgrade HTTP client")?;

    println!(
        "Checking the latest taskgen release for {}-{}...",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    match upgrade_from(&client, RELEASE_BASE_URL, &executable, asset).await? {
        UpgradeOutcome::AlreadyCurrent => {
            println!(
                "taskgen {} already matches the latest official release ({})",
                env!("CARGO_PKG_VERSION"),
                executable.display()
            );
        }
        UpgradeOutcome::Installed { bytes } => {
            println!(
                "Installed the latest taskgen release ({} bytes) -> {}",
                bytes,
                executable.display()
            );
            println!("Run 'taskgen --version' to confirm the installed version.");
        }
    }
    Ok(())
}

fn release_asset(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("linux", "aarch64" | "arm64") => Ok("taskgen-linux-arm64"),
        ("macos", "aarch64" | "arm64") => Ok("taskgen-darwin-arm64"),
        _ => bail!(
            "no prebuilt taskgen release for {os}-{arch}; supported platforms are Linux ARM64 and macOS Apple Silicon"
        ),
    }
}

async fn upgrade_from(
    client: &reqwest::Client,
    release_base_url: &str,
    executable: &Path,
    asset: &str,
) -> Result<UpgradeOutcome> {
    let checksums_url = format!("{}/SHA256SUMS", release_base_url.trim_end_matches('/'));
    let checksums = fetch_checksums(client, &checksums_url).await?;
    let expected = checksum_for_asset(&checksums, asset)?;
    let current = sha256_file(executable).with_context(|| {
        format!(
            "failed to hash current executable: {}",
            executable.display()
        )
    })?;
    if current == expected {
        return Ok(UpgradeOutcome::AlreadyCurrent);
    }

    let parent = executable.parent().with_context(|| {
        format!(
            "current executable has no parent directory: {}",
            executable.display()
        )
    })?;
    let mut pending = PendingDownload::create(parent).with_context(|| {
        format!(
            "cannot create an upgrade file beside {}; ensure the directory is writable (for a system install, run 'sudo taskgen upgrade')",
            executable.display()
        )
    })?;
    let asset_url = format!("{}/{}", release_base_url.trim_end_matches('/'), asset);
    let response = client
        .get(&asset_url)
        .send()
        .await
        .with_context(|| format!("failed to download {asset_url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("release download returned HTTP {status}: {asset_url}");
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BINARY_BYTES)
    {
        bail!("release asset exceeds the {MAX_BINARY_BYTES}-byte safety limit");
    }
    let mut bytes = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("failed while downloading the release asset")?;
        bytes = bytes.saturating_add(chunk.len() as u64);
        if bytes > MAX_BINARY_BYTES {
            bail!("release asset exceeds the {MAX_BINARY_BYTES}-byte safety limit");
        }
        pending
            .file_mut()?
            .write_all(&chunk)
            .context("failed while writing the release asset")?;
    }
    if bytes == 0 {
        bail!("invalid release asset size: {bytes} bytes");
    }
    pending.close()?;

    let actual = sha256_file(pending.path())?;
    if actual != expected {
        bail!("release checksum mismatch for {asset}: expected {expected}, got {actual}");
    }
    pending.commit(executable)?;
    Ok(UpgradeOutcome::Installed { bytes })
}

async fn fetch_checksums(client: &reqwest::Client, url: &str) -> Result<String> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to download {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("checksum download returned HTTP {status}: {url}");
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CHECKSUM_BYTES as u64)
    {
        bail!("checksum file exceeds the {MAX_CHECKSUM_BYTES}-byte safety limit");
    }
    let text = response
        .text()
        .await
        .context("failed to read the checksum file")?;
    if text.len() > MAX_CHECKSUM_BYTES {
        bail!("checksum file exceeds the {MAX_CHECKSUM_BYTES}-byte safety limit");
    }
    Ok(text)
}

fn checksum_for_asset(contents: &str, asset: &str) -> Result<String> {
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        let Some(checksum) = fields.next() else {
            continue;
        };
        let Some(filename) = fields.next() else {
            continue;
        };
        if filename.trim_start_matches('*') == asset {
            let normalized = checksum.to_ascii_lowercase();
            if normalized.len() != 64 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!("invalid SHA-256 checksum for {asset}");
            }
            return Ok(normalized);
        }
    }
    bail!("release checksum file does not contain {asset}")
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

struct PendingDownload {
    path: PathBuf,
    file: Option<File>,
    committed: bool,
}

impl PendingDownload {
    fn create(parent: &Path) -> Result<Self> {
        let path = parent.join(format!(
            ".taskgen-upgrade-{}-{:016x}.tmp",
            std::process::id(),
            rand::random::<u64>()
        ));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            path,
            file: Some(file),
            committed: false,
        })
    }

    fn file_mut(&mut self) -> Result<&mut File> {
        self.file.as_mut().context("upgrade file is closed")
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn close(&mut self) -> Result<()> {
        let file = self.file.take().context("upgrade file is closed")?;
        file.sync_all()?;
        drop(file);
        Ok(())
    }

    fn commit(mut self, destination: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o755))?;
        }
        #[cfg(not(unix))]
        bail!("self-upgrade is supported only on Unix platforms");

        fs::rename(&self.path, destination).with_context(|| {
            format!(
                "failed to replace {}; ensure its directory is writable (for a system install, run 'sudo taskgen upgrade')",
                destination.display()
            )
        })?;
        self.committed = true;
        File::open(destination.parent().unwrap_or_else(|| Path::new(".")))
            .and_then(|directory| directory.sync_all())
            .context("failed to sync the upgraded executable directory")?;
        Ok(())
    }
}

impl Drop for PendingDownload {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_published_platform_assets() {
        assert_eq!(
            release_asset("linux", "aarch64").unwrap(),
            "taskgen-linux-arm64"
        );
        assert_eq!(
            release_asset("macos", "aarch64").unwrap(),
            "taskgen-darwin-arm64"
        );
        assert!(release_asset("linux", "x86_64").is_err());
        assert!(release_asset("windows", "aarch64").is_err());
    }

    #[test]
    fn parses_the_exact_asset_checksum() {
        let contents = concat!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  taskgen-darwin-arm64\n",
            "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB  taskgen-linux-arm64\n"
        );
        assert_eq!(
            checksum_for_asset(contents, "taskgen-linux-arm64").unwrap(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
        assert!(checksum_for_asset(contents, "missing").is_err());
    }

    #[tokio::test]
    async fn downloads_verifies_and_atomically_replaces_the_executable() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let replacement = b"new taskgen release";
        let expected = format!("{:x}", Sha256::digest(replacement));
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/SHA256SUMS"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!("{expected}  taskgen-linux-arm64\n")),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/taskgen-linux-arm64"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(replacement))
            .mount(&server)
            .await;

        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("taskgen");
        fs::write(&executable, b"old taskgen release").unwrap();
        let outcome = upgrade_from(
            &reqwest::Client::new(),
            &server.uri(),
            &executable,
            "taskgen-linux-arm64",
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            UpgradeOutcome::Installed {
                bytes: replacement.len() as u64
            }
        );
        assert_eq!(fs::read(&executable).unwrap(), replacement);
        assert!(fs::read_dir(temporary.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp")
        }));
    }

    #[tokio::test]
    async fn matching_checksum_skips_the_binary_download() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let current = b"current official release";
        let expected = format!("{:x}", Sha256::digest(current));
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/SHA256SUMS"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!("{expected}  taskgen-linux-arm64\n")),
            )
            .mount(&server)
            .await;

        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("taskgen");
        fs::write(&executable, current).unwrap();
        let outcome = upgrade_from(
            &reqwest::Client::new(),
            &server.uri(),
            &executable,
            "taskgen-linux-arm64",
        )
        .await
        .unwrap();

        assert_eq!(outcome, UpgradeOutcome::AlreadyCurrent);
        assert_eq!(fs::read(&executable).unwrap(), current);
    }

    #[tokio::test]
    async fn checksum_mismatch_preserves_the_current_executable() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/SHA256SUMS"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!("{}  taskgen-linux-arm64\n", "a".repeat(64))),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/taskgen-linux-arm64"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"tampered release"))
            .mount(&server)
            .await;

        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("taskgen");
        fs::write(&executable, b"current release").unwrap();
        let result = upgrade_from(
            &reqwest::Client::new(),
            &server.uri(),
            &executable,
            "taskgen-linux-arm64",
        )
        .await;

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("checksum mismatch")
        );
        assert_eq!(fs::read(&executable).unwrap(), b"current release");
    }
}
