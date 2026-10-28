pub mod cdp;
mod session;

use crate::{
    extract::{self, Options},
    model::{Issue, Record, Run},
    net::{self, Response},
    store::Store,
};
use anyhow::{Context, Result, anyhow, ensure};
use base64::Engine;
use cdp::Cdp;
use clap::ValueEnum;
use serde_json::{Value, json};
use session::{Process, Session, State};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore};
use url::Url;

pub(crate) fn startup_issue(source: &str, message: &str) -> Option<Issue> {
    if !message.contains("browser exited during startup")
        && !message.contains("browser unavailable for this run")
        && !message.contains("could not launch browser executable")
    {
        return None;
    }
    let mut issue = Issue::new(source, "browser_unavailable", message);
    issue.next_action = Some("Check Chromium installation and sandbox support, select another executable with --browser-path, or use --browser off to keep HTTP retrieval available".into());
    Some(issue)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, ValueEnum)]
pub enum Mode {
    #[default]
    Auto,
    Off,
    Always,
}

#[derive(Clone)]
pub struct Config {
    pub root: PathBuf,
    pub executable: Option<PathBuf>,
    pub timeout: Duration,
    pub max_bytes: usize,
    pub no_sandbox: bool,
}

#[derive(Clone, clap::Subcommand)]
pub enum Action {
    Start {
        #[arg(long)]
        headed: bool,
    },
    Open {
        url: String,
        #[arg(long)]
        new_tab: bool,
        #[arg(long)]
        wait_for: Option<String>,
    },
    Inspect {
        #[arg(long, default_value = "300")]
        limit: usize,
    },
    Click {
        reference: String,
    },
    Fill {
        reference: String,
        text: String,
    },
    Press {
        key: String,
    },
    Extract {
        #[arg(long, value_enum, default_value = "article")]
        mode: extract::Extract,
        #[arg(long)]
        css: Option<String>,
    },
    Screenshot {
        output: PathBuf,
        #[arg(long)]
        full_page: bool,
    },
    Tabs,
    Switch {
        target: String,
    },
    CloseTab,
    Stop,
    Status,
}

pub struct Rendered {
    pub response: Response,
    pub warnings: Vec<String>,
    pub status_observed: bool,
}

impl Config {
    pub fn executable(&self) -> Option<PathBuf> {
        session::discover(self.executable.as_deref())
    }

    pub async fn execute(&self, name: &str, action: Action, store: Option<&Store>) -> Run {
        let start = Instant::now();
        let mut run = Run::new("browser");
        run.meta = json!({"session":name});
        match tokio::time::timeout(self.timeout, self.perform(name, action, store, &mut run)).await
        {
            Ok(Ok(())) => (),
            Ok(Err(e)) => {
                let message = format!("{e:#}");
                let issue = startup_issue("browser", &message).unwrap_or_else(|| {
                    let mut issue = Issue::new("browser", "browser_operation_failed", message);
                    issue.next_action=Some("Inspect the session status; refresh element references with browser inspect after navigation or DOM changes".into());
                    issue
                });
                run.errors.push(issue);
            }
            Err(_) => {
                let mut issue = Issue::new(
                    "browser",
                    "deadline_exceeded",
                    "browser operation exceeded --timeout; inspect the session before retrying an action that may have already executed",
                );
                issue.next_action=Some("Use browser inspect to observe the current page; do not blindly repeat a form submission".into());
                run.errors.push(issue);
            }
        }
        run.finish(start.elapsed().as_millis() as u64);
        run
    }

    async fn connection(
        &self,
        session: &Session,
        start: bool,
        headed: bool,
    ) -> Result<(State, Cdp)> {
        if let Some(state) = session.load()? {
            let mut cdp=Cdp::connect(&state.endpoint,self.timeout.min(Duration::from_secs(3)),self.max_bytes).await.context("saved browser session is unavailable; run browser status and stop before restarting")?;
            cdp.call("Browser.getVersion", json!({})).await?;
            cdp.set_timeout(self.timeout);
            return Ok((state, cdp));
        }
        ensure!(
            start,
            "browser session is not running; use browser open URL or browser start"
        );
        let executable = self.executable().context(
            "Chromium/Chrome not found; install it or set --browser-path/FFETCH_BROWSER_PATH",
        )?;
        let (mut process, state) = session::launch(
            &executable,
            &session.directory,
            headed,
            self.timeout,
            self.max_bytes,
            self.no_sandbox,
        )
        .await?;
        session.save(&state)?;
        let cdp = Cdp::connect(&state.endpoint, self.timeout, self.max_bytes).await?;
        process.owned = false; // The named session intentionally outlives this invocation.
        Ok((state, cdp))
    }

    async fn perform(
        &self,
        name: &str,
        action: Action,
        store: Option<&Store>,
        run: &mut Run,
    ) -> Result<()> {
        let session = Session::lock(&self.root, name, self.timeout).await?;
        if matches!(action, Action::Status) {
            let info = if let Some(state) = session.load()? {
                match Cdp::connect(
                    &state.endpoint,
                    self.timeout.min(Duration::from_secs(3)),
                    self.max_bytes,
                )
                .await
                {
                    Ok(mut cdp) => match cdp.call("Browser.getVersion", json!({})).await {
                        Ok(version) => {
                            json!({"running":true,"pid":state.pid,"target":state.target,"headed":state.headed,"version":version["product"]})
                        }
                        Err(e) => json!({"running":false,"saved_state":true,"error":e.to_string()}),
                    },
                    Err(e) => json!({"running":false,"saved_state":true,"error":e.to_string()}),
                }
            } else {
                json!({"running":false,"saved_state":false})
            };
            add_record(run, "browser_state", "", info);
            return Ok(());
        }
        if matches!(action, Action::Stop) {
            if let Some(state) = session.load()? {
                match Cdp::connect(
                    &state.endpoint,
                    self.timeout.min(Duration::from_secs(3)),
                    self.max_bytes,
                )
                .await
                {
                    Ok(mut cdp) => {
                        let _ = cdp.call("Browser.close", json!({})).await;
                        let start = Instant::now();
                        loop {
                            let endpoint = Url::parse(&state.endpoint)?;
                            if tokio::net::TcpStream::connect((
                                "127.0.0.1",
                                endpoint.port().context("missing CDP port")?,
                            ))
                            .await
                            .is_err()
                            {
                                break;
                            }
                            ensure!(
                                start.elapsed() < Duration::from_secs(3),
                                "browser did not close; session state retained"
                            );
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                    Err(e) => {
                        let refused = e.chain().any(|s| {
                            s.downcast_ref::<std::io::Error>()
                                .is_some_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused)
                        });
                        ensure!(
                            refused,
                            "cannot verify browser stopped: {e:#}; session state retained"
                        );
                    }
                }
                session.clear()?;
            }
            add_record(
                run,
                "browser_state",
                "",
                json!({"running":false,"profile_preserved":true}),
            );
            return Ok(());
        }
        let should_start = matches!(action, Action::Start { .. } | Action::Open { .. });
        let headed = matches!(action, Action::Start { headed: true });
        let (mut state, mut cdp) = self.connection(&session, should_start, headed).await?;
        if matches!(action, Action::Start { .. }) {
            add_record(
                run,
                "browser_state",
                "",
                json!({"running":true,"pid":state.pid,"target":state.target,"headed":state.headed}),
            );
            return Ok(());
        }
        if matches!(action, Action::Tabs) {
            let targets = cdp.call("Target.getTargets", json!({})).await?;
            for target in targets["targetInfos"]
                .as_array()
                .context("missing targets")?
                .iter()
                .filter(|t| t["type"] == "page")
            {
                let mut content = target.clone();
                content["active"] = json!(target["targetId"] == state.target);
                add_record(
                    run,
                    "browser_tab",
                    target["url"].as_str().unwrap_or(""),
                    content,
                );
            }
            return Ok(());
        }
        if let Action::Switch { target } = &action {
            let info = cdp
                .call("Target.getTargetInfo", json!({"targetId":target}))
                .await?;
            ensure!(info["targetInfo"]["type"] == "page", "target is not a page");
            state.target = target.clone();
            session.save(&state)?;
        }
        if matches!(action, Action::CloseTab) {
            let result = cdp
                .call("Target.closeTarget", json!({"targetId":state.target}))
                .await?;
            ensure!(result["success"] == true, "tab did not close");
            let targets = cdp.call("Target.getTargets", json!({})).await?;
            state.target = targets["targetInfos"]
                .as_array()
                .and_then(|a| a.iter().find(|t| t["type"] == "page"))
                .and_then(|t| t["targetId"].as_str())
                .unwrap_or("")
                .into();
            session.save(&state)?;
            add_record(
                run,
                "browser_state",
                "",
                json!({"tab_closed":true,"active_target":state.target}),
            );
            return Ok(());
        }
        if matches!(action, Action::Open { new_tab: true, .. }) || state.target.is_empty() {
            let result = cdp
                .call("Target.createTarget", json!({"url":"about:blank"}))
                .await?;
            state.target = result["targetId"]
                .as_str()
                .context("missing target ID")?
                .into();
            session.save(&state)?;
        }
        cdp.attach(&state.target).await?;
        match action {
            Action::Open { url, wait_for, .. } => {
                let url = net::normalize(&url)?;
                let rendered =
                    navigate(&mut cdp, &url, wait_for.as_deref(), self.max_bytes).await?;
                run.warnings.extend(rendered.warnings);
                run.meta["http_status"] = if rendered.status_observed {
                    json!(rendered.response.status.as_u16())
                } else {
                    Value::Null
                };
                run.meta["http_status_observed"] = json!(rendered.status_observed);
                let snapshot = inspect(&mut cdp, 300).await?;
                add_record(
                    run,
                    "browser_snapshot",
                    rendered.response.url.as_str(),
                    snapshot,
                );
            }
            Action::Inspect { limit } => {
                let snapshot = inspect(&mut cdp, limit).await?;
                let url = snapshot["url"].as_str().unwrap_or("").to_owned();
                add_record(run, "browser_snapshot", &url, snapshot);
            }
            Action::Click { reference } => {
                let context = cdp.isolated_world().await?;
                let code = element_script(
                    &reference,
                    "const r=el.getBoundingClientRect(); const x=Math.max(0,Math.min(innerWidth-1,r.x+r.width/2)); const y=Math.max(0,Math.min(innerHeight-1,r.y+r.height/2)); const hit=document.elementFromPoint(x,y); if(!hit || !(el===hit || el.contains(hit))) throw new Error('element_obscured: another element covers this target'); return {x,y};",
                )?;
                let point = cdp.evaluate(&code, Some(context)).await?;
                for kind in ["mousePressed", "mouseReleased"] {
                    cdp.call("Input.dispatchMouseEvent",json!({"type":kind,"x":point["x"],"y":point["y"],"button":"left","clickCount":1})).await?;
                }
                add_record(
                    run,
                    "browser_action",
                    "",
                    json!({"action":"click","reference":reference,"dispatched":true,"next_action":"inspect to observe the result"}),
                );
            }
            Action::Fill { reference, text } => {
                let context = cdp.isolated_world().await?;
                let code = element_script(
                    &reference,
                    "if(el.readOnly) throw new Error('element_readonly'); if(!(el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el.isContentEditable)) throw new Error('element_not_editable'); el.focus(); if(el.isContentEditable){const range=document.createRange();range.selectNodeContents(el);const sel=getSelection();sel.removeAllRanges();sel.addRange(range);}else{el.select();} return true;",
                )?;
                cdp.evaluate(&code, Some(context)).await?;
                cdp.call("Input.insertText", json!({"text":text})).await?;
                add_record(
                    run,
                    "browser_action",
                    "",
                    json!({"action":"fill","reference":reference,"dispatched":true}),
                );
            }
            Action::Press { key } => {
                let (code, text) = match key.as_str() {
                    "Enter" => (13, "\r"),
                    "Tab" => (9, ""),
                    "Escape" => (27, ""),
                    "Backspace" => (8, ""),
                    "ArrowLeft" => (37, ""),
                    "ArrowUp" => (38, ""),
                    "ArrowRight" => (39, ""),
                    "ArrowDown" => (40, ""),
                    "Space" => (32, " "),
                    _ => {
                        return Err(anyhow!(
                            "unsupported key; use Enter, Tab, Escape, Backspace, ArrowLeft/Up/Right/Down or Space"
                        ));
                    }
                };
                for kind in ["keyDown", "keyUp"] {
                    cdp.call("Input.dispatchKeyEvent",json!({"type":kind,"key":if key=="Space" {" "} else {&key},"windowsVirtualKeyCode":code,"text":if kind=="keyDown" {text} else {""}})).await?;
                }
                add_record(
                    run,
                    "browser_action",
                    "",
                    json!({"action":"press","key":key,"dispatched":true}),
                );
            }
            Action::Extract { mode, css } => {
                let options = Options { mode, css };
                let response = document(&mut cdp, self.max_bytes, None).await?;
                ensure!(
                    extract::invalid_content(&String::from_utf8_lossy(&response.bytes)).is_none(),
                    "browser page is a challenge, error or JavaScript interstitial"
                );
                let artifact = store.map(|s| s.blob(&response.bytes)).transpose()?;
                run.items = extract::extract(&response, &options)?;
                for record in &mut run.items {
                    record.provenance.method = "browser-dom".into();
                    record.provenance.artifact.clone_from(&artifact);
                }
                run.meta["representation"] =
                    json!("rendered DOM; not original HTTP response bytes");
            }
            Action::Screenshot { output, full_page } => {
                let mut params = json!({"format":"png","captureBeyondViewport":full_page});
                if full_page {
                    let metrics = cdp.call("Page.getLayoutMetrics", json!({})).await?;
                    let size = &metrics["cssContentSize"];
                    params["clip"] = json!({"x":0,"y":0,"width":size["width"],"height":size["height"],"scale":1});
                }
                let result = cdp.call("Page.captureScreenshot", params).await?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(result["data"].as_str().context("missing screenshot data")?)?;
                ensure!(
                    bytes.len() <= self.max_bytes,
                    "screenshot exceeds --max-bytes"
                );
                std::fs::write(&output, &bytes)
                    .with_context(|| format!("cannot write screenshot {}", output.display()))?;
                add_record(
                    run,
                    "browser_screenshot",
                    "",
                    json!({"path":output,"bytes":bytes.len(),"mime_type":"image/png"}),
                );
            }
            Action::Switch { .. } => {
                add_record(
                    run,
                    "browser_state",
                    "",
                    json!({"active_target":state.target}),
                );
            }
            _ => unreachable!(),
        }
        Ok(())
    }
}

fn add_record(run: &mut Run, kind: &str, url: &str, content: Value) {
    let mut record = Record::new(kind, "browser", url);
    record.title = content["title"].as_str().map(str::to_owned);
    record.content = content;
    record.provenance.method = "cdp".into();
    run.items.push(record);
}

fn element_script(reference: &str, action: &str) -> Result<String> {
    Ok(include_str!("scripts/element.js")
        .replace("REFERENCE", &serde_json::to_string(reference)?)
        .replace("ACTION", action))
}

async fn inspect(cdp: &mut Cdp, limit: usize) -> Result<Value> {
    let context = cdp.isolated_world().await?;
    let snapshot = uuid::Uuid::new_v4().simple().to_string();
    let code = include_str!("scripts/inspect.js")
        .replace("SNAPSHOT", &serde_json::to_string(&snapshot)?)
        .replace("LIMIT", &limit.to_string());
    cdp.evaluate(&code, Some(context)).await
}

async fn document(cdp: &mut Cdp, max_bytes: usize, status: Option<u16>) -> Result<Response> {
    let data=cdp.evaluate("({url:location.href,html:document.documentElement.outerHTML,mime:document.contentType})",None).await?;
    let html = data["html"]
        .as_str()
        .context("browser returned no document")?;
    ensure!(html.len() <= max_bytes, "rendered DOM exceeds --max-bytes");
    let url = Url::parse(data["url"].as_str().context("browser returned no URL")?)?;
    ensure!(
        matches!(url.scheme(), "http" | "https" | "about"),
        "browser navigated to an unsupported document scheme"
    );
    Ok(Response {
        url,
        status: reqwest::StatusCode::from_u16(status.unwrap_or(200))?,
        content_type: data["mime"].as_str().unwrap_or("text/html").into(),
        retry_after: None,
        bytes: html.as_bytes().to_vec(),
    })
}

pub async fn navigate(
    cdp: &mut Cdp,
    url: &Url,
    wait_for: Option<&str>,
    max_bytes: usize,
) -> Result<Rendered> {
    if let Some(css) = wait_for {
        extract::selector(css)?;
    }
    cdp.events.clear();
    let result = cdp
        .call("Page.navigate", json!({"url":url.as_str()}))
        .await?;
    ensure!(
        result["errorText"].is_null(),
        "browser navigation failed: {}",
        result["errorText"]
    );
    let loader = result["loaderId"].as_str().unwrap_or("").to_owned();
    let frame = result["frameId"].as_str().unwrap_or("").to_owned();
    let mut committed = loader.is_empty();
    let mut status = None;
    let warnings = vec![];
    let ready = if let Some(css) = wait_for {
        format!(
            "document.readyState !== 'loading' && !!document.querySelector({})",
            serde_json::to_string(css)?
        )
    } else {
        "document.readyState !== 'loading' && !!document.body && document.body.innerText.trim().length > 0".into()
    };
    loop {
        let was_committed = committed;
        let is_ready = cdp.evaluate(&ready, None).await.unwrap_or(json!(false)) == true;
        while let Some(event) = cdp.events.pop_front() {
            let p = &event["params"];
            match event["method"].as_str().unwrap_or("") {
                "Page.frameNavigated"
                    if p["frame"]["id"] == frame
                        && (loader.is_empty() || p["frame"]["loaderId"] == loader) =>
                {
                    committed = true;
                }
                "Network.responseReceived"
                    if p["type"] == "Document"
                        && p["frameId"] == frame
                        && (loader.is_empty() || p["loaderId"] == loader) =>
                {
                    status = p["response"]["status"].as_f64().map(|s| s as u16);
                }
                _ => (),
            }
        }
        if is_ready && was_committed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let response = document(cdp, max_bytes, status).await?;
    Ok(Rendered {
        response,
        warnings,
        status_observed: status.is_some(),
    })
}

struct Runtime {
    _process: Process,
    endpoint: String,
    _directory: tempfile::TempDir,
}

enum RuntimeState {
    Cold,
    Running(Runtime),
    Failed(String),
}

#[derive(Clone)]
pub struct Renderer {
    config: Config,
    runtime: Arc<Mutex<RuntimeState>>,
    permits: Arc<Semaphore>,
    pub session: Option<String>,
    pub wait_for: Option<String>,
}

impl Renderer {
    pub fn new(config: Config, session: Option<String>, wait_for: Option<String>) -> Self {
        Self {
            config,
            runtime: Arc::new(Mutex::new(RuntimeState::Cold)),
            permits: Arc::new(Semaphore::new(2)),
            session,
            wait_for,
        }
    }
    pub fn available(&self) -> bool {
        self.session.is_some() || self.config.executable().is_some()
    }
    pub async fn render(&self, url: &Url) -> Result<Rendered> {
        let _permit = self.permits.acquire().await?;
        if let Some(name) = &self.session {
            let session = Session::lock(&self.config.root, name, self.config.timeout).await?;
            let (state, mut cdp) = self.config.connection(&session, true, false).await?;
            cdp.attach(&state.target).await?;
            return navigate(
                &mut cdp,
                url,
                self.wait_for.as_deref(),
                self.config.max_bytes,
            )
            .await;
        }
        let endpoint = {
            let mut runtime = self.runtime.lock().await;
            if matches!(*runtime, RuntimeState::Cold) {
                let executable = self
                    .config
                    .executable()
                    .context("Chromium/Chrome is unavailable")?;
                let directory = tempfile::Builder::new()
                    .prefix("ffetch-browser-")
                    .tempdir()?;
                let launched = session::launch(
                    &executable,
                    directory.path(),
                    false,
                    self.config.timeout,
                    self.config.max_bytes,
                    self.config.no_sandbox,
                )
                .await;
                *runtime = match launched {
                    Ok((process, state)) => RuntimeState::Running(Runtime {
                        _process: process,
                        endpoint: state.endpoint,
                        _directory: directory,
                    }),
                    Err(error) => RuntimeState::Failed(format!("{error:#}")),
                };
            }
            match &*runtime {
                RuntimeState::Running(runtime) => runtime.endpoint.clone(),
                RuntimeState::Failed(message) => {
                    return Err(anyhow!("browser unavailable for this run: {message}"));
                }
                RuntimeState::Cold => unreachable!(),
            }
        };
        let mut cdp = Cdp::connect(&endpoint, self.config.timeout, self.config.max_bytes).await?;
        let target = cdp
            .call("Target.createTarget", json!({"url":"about:blank"}))
            .await?;
        let target = target["targetId"]
            .as_str()
            .context("missing target")?
            .to_owned();
        cdp.attach(&target).await?;
        let result = tokio::time::timeout(
            self.config.timeout,
            navigate(
                &mut cdp,
                url,
                self.wait_for.as_deref(),
                self.config.max_bytes,
            ),
        )
        .await
        .context("browser navigation deadline exceeded");
        cdp.session = None;
        let _ = cdp
            .call("Target.closeTarget", json!({"targetId":target}))
            .await;
        result?
    }
}
