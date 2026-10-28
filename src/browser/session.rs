use super::cdp::Cdp;
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub endpoint: String,
    pub target: String,
    pub pid: u32,
    pub executable: PathBuf,
    pub headed: bool,
}

pub struct Session {
    pub directory: PathBuf,
    _lock: File,
}

impl Session {
    pub async fn lock(root: &Path, name: &str, timeout: Duration) -> Result<Self> {
        ensure!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
            "session name must contain 1-64 letters, digits, underscores or hyphens"
        );
        let directory = root.join("browser").join(name);
        private_dir(&directory)?;
        let lock = private_file(&directory.join("lock"), false)?;
        let start = Instant::now();
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    ensure!(
                        start.elapsed() < timeout,
                        "browser session is busy; another ffetch command holds its lock"
                    );
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                Err(e) => return Err(anyhow!("cannot lock browser session: {e}")),
            }
        }
        Ok(Self {
            directory,
            _lock: lock,
        })
    }
    pub fn load(&self) -> Result<Option<State>> {
        let path = self.directory.join("state.json");
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).context("invalid browser session state")?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn save(&self, state: &State) -> Result<()> {
        use std::io::Write;
        let path = self.directory.join("state.tmp");
        let mut file = private_file(&path, true)?;
        file.write_all(&serde_json::to_vec(state)?)?;
        file.sync_all()?;
        std::fs::rename(path, self.directory.join("state.json"))?;
        Ok(())
    }
    pub fn clear(&self) -> Result<()> {
        match std::fs::remove_file(self.directory.join("state.json")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

pub fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_file(path: &Path, truncate: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .create(true)
        .read(true)
        .write(true)
        .truncate(truncate);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

/// A failed launch always kills/reaps its child. Persistent callers explicitly
/// disarm this guard only after the browser is reachable and state is saved.
pub struct Process {
    pub child: Child,
    pub owned: bool,
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.owned {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn discover(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return path.is_file().then(|| path.to_owned());
    }
    for name in [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
    ] {
        if let Some(paths) = std::env::var_os("PATH") {
            for directory in std::env::split_paths(&paths) {
                let path = directory.join(name);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    for path in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ] {
        if Path::new(path).is_file() {
            return Some(path.into());
        }
    }
    // Reuse an already installed testing browser; do not require Playwright itself.
    if let Some(home) = std::env::var_os("HOME") {
        let cache = PathBuf::from(home).join(".cache/ms-playwright");
        if let Ok(entries) = std::fs::read_dir(cache) {
            let mut paths = entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("chromium-"))
                .map(|e| e.path())
                .collect::<Vec<_>>();
            paths.sort();
            paths.reverse();
            for base in paths {
                for suffix in ["chrome-linux64/chrome", "chrome-linux/chrome"] {
                    let path = base.join(suffix);
                    if path.is_file() {
                        return Some(path);
                    }
                }
            }
        }
    }
    None
}

pub async fn launch(
    executable: &Path,
    directory: &Path,
    headed: bool,
    timeout: Duration,
    max_bytes: usize,
    no_sandbox: bool,
) -> Result<(Process, State)> {
    let profile = directory.join("profile");
    private_dir(&profile)?;
    // A stopped browser may leave this marker behind. It is not a liveness check.
    let port_file = profile.join("DevToolsActivePort");
    if port_file.exists() {
        std::fs::remove_file(&port_file)?;
    }
    let mut command = Command::new(executable);
    command
        .args([
            "--remote-debugging-port=0",
            "--remote-debugging-address=127.0.0.1",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-sync",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(private_file(&directory.join("browser.log"), true)?);
    if !headed {
        command.arg("--headless=new");
    }
    if no_sandbox {
        command.arg("--no-sandbox");
    }
    // Chromium does not consume the standard HTTP proxy environment itself.
    if let Some(proxy) = std::env::var("HTTPS_PROXY")
        .ok()
        .or_else(|| std::env::var("https_proxy").ok())
        .or_else(|| std::env::var("HTTP_PROXY").ok())
        .or_else(|| std::env::var("http_proxy").ok())
        .filter(|proxy| !proxy.is_empty())
    {
        let parsed = url::Url::parse(&proxy).context("invalid browser proxy URL")?;
        ensure!(
            parsed.username().is_empty() && parsed.password().is_none(),
            "browser proxy authentication requires an authenticated browser integration; cannot silently discard credentials"
        );
        command.arg(format!("--proxy-server={proxy}"));
        if let Some(bypass) = std::env::var("NO_PROXY")
            .ok()
            .or_else(|| std::env::var("no_proxy").ok())
        {
            command.arg(format!("--proxy-bypass-list={}", bypass.replace(',', ";")));
        }
    }
    let mut process = Process {
        child: command
            .spawn()
            .context("could not launch browser executable")?,
        owned: true,
    };
    let start = Instant::now();
    loop {
        if let Some(status) = process.child.try_wait()? {
            return Err(anyhow!(
                "browser exited during startup ({status}); inspect {}",
                directory.join("browser.log").display()
            ));
        }
        if let Ok(data) = std::fs::read_to_string(&port_file) {
            let mut lines = data.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                let port: u16 = port.parse().context("invalid browser debugging port")?;
                ensure!(
                    path.starts_with("/devtools/browser/"),
                    "invalid browser debugging path"
                );
                let endpoint = format!("ws://127.0.0.1:{port}{path}");
                let mut cdp = Cdp::connect(&endpoint, timeout, max_bytes).await?;
                cdp.call("Browser.getVersion", json!({})).await?;
                let targets = cdp.call("Target.getTargets", json!({})).await?;
                let target = targets["targetInfos"]
                    .as_array()
                    .and_then(|a| a.iter().find(|t| t["type"] == "page"))
                    .and_then(|t| t["targetId"].as_str())
                    .context("browser did not create an initial tab")?
                    .to_owned();
                let state = State {
                    endpoint,
                    target,
                    pid: process.child.id(),
                    executable: executable.to_owned(),
                    headed,
                };
                return Ok((process, state));
            }
        }
        ensure!(
            start.elapsed() < timeout,
            "browser startup deadline exceeded; inspect {}",
            directory.join("browser.log").display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
