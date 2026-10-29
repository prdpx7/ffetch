//! Opt-in real Chromium checks: FFETCH_TEST_BROWSER=path cargo test --test browser.
use serde_json::Value;
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

struct Site {
    url: String,
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}
impl Site {
    fn new() -> Self {
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", socket.local_addr().unwrap());
        socket.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let task = thread::spawn(move || {
            while !signal.load(Ordering::Relaxed) {
                match socket.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut buffer = [0; 4096];
                        let n = stream.read(&mut buffer).unwrap_or(0);
                        let static_page = buffer[..n].starts_with(b"GET /static ");
                        let html = "<!doctype html><title>Browser fixture</title><div id='root'></div><script>setTimeout(()=>{document.querySelector('#root').innerHTML=`<article style='min-height:1600px'><h1>Robot fixture ready</h1><label>Name <input id='name'></label><button onclick=\"document.querySelector('#answer').textContent='Hello '+document.querySelector('#name').value\">Run</button><p id='answer'></p></article>`},100)</script>";
                        let engine_page = buffer[..n].starts_with(b"GET /engine?");
                        let attribute_page = buffer[..n].starts_with(b"GET /attribute ");
                        let redirect_page = buffer[..n].starts_with(b"GET /redirect ");
                        let missing_page = buffer[..n].starts_with(b"GET /missing ");
                        let request = String::from_utf8_lossy(&buffer[..n]);
                        let path = request
                            .lines()
                            .next()
                            .unwrap_or("")
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("");
                        let challenged_header =
                            matches!(path, "/challenge-redirect" | "/challenge-opaque");
                        let html = if engine_page
                            && String::from_utf8_lossy(&buffer[..n]).contains("start=10")
                        {
                            "<!doctype html><title>Search fixture</title><div id='root'></div><script>setTimeout(()=>{document.querySelector('#root').innerHTML=\"<div class='MjjYud'><a href='https://example.org/robot2'><h3>Second robot research</h3></a></div>\"},100)</script>"
                        } else if engine_page {
                            "<!doctype html><title>Search fixture</title><div id='root'></div><script>setTimeout(()=>{document.querySelector('#root').innerHTML=\"<div class='MjjYud'><a href='https://example.org/robot'><h3>Robot research</h3></a><div class='VwiC3b'>Validated engine evidence</div></div><a id='pnnext' href='/engine?q=robotics&amp;start=10'>Next</a>\"},100)</script>"
                        } else if static_page {
                            "<article>Static robot document</article>"
                        } else if attribute_page {
                            "<!doctype html><article id='state' data-state='waiting'>Attribute fixture</article><script>setTimeout(()=>document.querySelector('#state').dataset.state='ready',100)</script>"
                        } else if redirect_page {
                            "<!doctype html><script>setTimeout(()=>location.replace('/missing'),30)</script>"
                        } else if missing_page {
                            "<!doctype html><article>Missing robot document</article>"
                        } else if path == "/challenge" {
                            "<!doctype html><title>Just a moment...</title><div id='cf-challenge-running'>Checking your browser</div><script>setTimeout(()=>{document.title='Research';document.body.innerHTML='<article>Recovered research evidence</article>';document.cookie='cleared=yes; path=/'},100)</script>"
                        } else if path == "/challenge-selector" {
                            "<!doctype html><title>Research</title><div id='cf-please-wait'>Checking</div><script>setTimeout(()=>{document.body.innerHTML='<article>Selector challenge cleared</article>'},100)</script>"
                        } else if path == "/challenge-redirect" {
                            "<!doctype html><title>Just a moment...</title><p>Checking</p><script>setTimeout(()=>location.replace('/static'),100)</script>"
                        } else if path == "/challenge-opaque" {
                            "<!doctype html><title>Unrecognized challenge</title><article>This is a challenge response, not research</article>"
                        } else if path == "/session-evidence"
                            && request.to_lowercase().contains("cookie: cleared=yes")
                        {
                            "<!doctype html><article>Session clearance retained</article>"
                        } else if path == "/challenge-never" || path == "/session-evidence" {
                            "<!doctype html><title>Just a moment...</title><div id='cf-challenge-running'>Checking forever</div>"
                        } else if path == "/denied" {
                            "<!doctype html><title>Access denied</title><p>This request was denied</p>"
                        } else if path == "/widget" {
                            "<!doctype html><title>Research</title><article>Research with an embedded Turnstile widget</article><div class='cf-turnstile'></div><input name='cf-turnstile-response'>"
                        } else {
                            html
                        };
                        let _ = write!(
                            stream,
                            "HTTP/1.1 {} Fixture\r\n{}Content-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            if missing_page {
                                404
                            } else if path == "/challenge-redirect" {
                                503
                            } else {
                                200
                            },
                            if challenged_header {
                                "cf-mitigated: challenge\r\n"
                            } else {
                                ""
                            },
                            html.len(),
                            html
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("fixture: {e}"),
                }
            }
        });
        Self {
            url,
            stop,
            task: Some(task),
        }
    }
}
impl Drop for Site {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.task.take().unwrap().join().unwrap();
    }
}
struct Browser {
    directory: tempfile::TempDir,
    executable: String,
}
impl Browser {
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(
            std::env::var_os("FFETCH_BENCH_BROWSER_BIN")
                .unwrap_or_else(|| env!("CARGO_BIN_EXE_ffetch").into()),
        );
        command
            .args(args)
            .args(["--no-store", "--browser-path", &self.executable])
            .arg("--data-dir")
            .arg(self.directory.path())
            .env("NO_PROXY", "localhost,127.0.0.1")
            .env("no_proxy", "localhost,127.0.0.1");
        if !args.contains(&"--timeout") {
            command.args(["--timeout", "15"]);
        }
        if std::env::var_os("FFETCH_TEST_BROWSER_NO_SANDBOX").is_some() {
            command.arg("--browser-no-sandbox");
        }
        command
    }
    fn run(&self, args: &[&str]) -> Value {
        let output = self
            .command(args)
            .args(["--session", "test"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}
impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self
            .command(&["browser", "stop"])
            .args(["--session", "test"])
            .output();
    }
}

#[test]
#[ignore = "controlled browser latency benchmark; set FFETCH_TEST_BROWSER and FFETCH_BENCH_BROWSER_BIN"]
fn benchmark_browser_navigation_readiness() {
    let browser = Browser {
        directory: tempfile::tempdir().unwrap(),
        executable: std::env::var("FFETCH_TEST_BROWSER").expect("set FFETCH_TEST_BROWSER"),
    };
    let site = Site::new();
    browser.run(&["browser", "start"]);
    let mut measurements = serde_json::Map::new();
    for (name, url, selector, expected) in [
        (
            "static",
            format!("{}static", site.url),
            "article",
            "Static robot document",
        ),
        (
            "javascript_100ms",
            site.url.clone(),
            "#name",
            "Robot fixture ready",
        ),
    ] {
        let mut samples = Vec::new();
        for _ in 0..12 {
            let began = std::time::Instant::now();
            let result = browser.run(&["browser", "open", &url, "--wait-for", selector]);
            samples.push(began.elapsed().as_secs_f64() * 1000.0);
            assert!(
                result["items"][0]["content"]["text"]
                    .as_str()
                    .unwrap()
                    .contains(expected)
            );
            assert_eq!(result["meta"]["http_status"], 200);
        }
        samples.sort_by(f64::total_cmp);
        let median = (samples[samples.len() / 2 - 1] + samples[samples.len() / 2]) / 2.0;
        measurements.insert(name.into(),serde_json::json!({"samples_ms":samples,"p50_ms":median,"p95_ms":samples[samples.len()-1]}));
    }
    let result = serde_json::json!({"benchmark":"warm browser session, separate CLI invocations; localhost","binary":std::env::var("FFETCH_BENCH_BROWSER_BIN").ok(),"browser":browser.executable,"platform":std::env::consts::OS,"samples_per_case":12,"p50":"median of two middle observations","p95":"nearest rank","measurements":measurements});
    if let Some(path) = std::env::var_os("FFETCH_BENCH_BROWSER_OUTPUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
    }
    println!("{result}");
}

#[test]
fn persistent_browser_interactions_and_static_retrieval_policy() {
    let Ok(executable) = std::env::var("FFETCH_TEST_BROWSER") else {
        eprintln!("set FFETCH_TEST_BROWSER to run the real-browser suite");
        return;
    };
    let browser = Browser {
        directory: tempfile::tempdir().unwrap(),
        executable,
    };
    let site = Site::new();
    let opened = browser.run(&["browser", "open", &site.url, "--wait-for", "#name"]);
    let elements = opened["items"][0]["content"]["elements"]
        .as_array()
        .unwrap();
    let input = elements.iter().find(|e| e["tag"] == "input").unwrap()["ref"]
        .as_str()
        .unwrap();
    let button = elements.iter().find(|e| e["tag"] == "button").unwrap()["ref"]
        .as_str()
        .unwrap();
    browser.run(&["browser", "fill", input, "Ada"]);
    browser.run(&["browser", "click", button]);
    let extracted = browser.run(&["browser", "extract"]);
    assert!(
        extracted["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Hello Ada")
    );
    assert_eq!(extracted["items"][0]["provenance"]["method"], "browser-dom");
    let image = browser.directory.path().join("page.png");
    browser.run(&[
        "browser",
        "screenshot",
        image.to_str().unwrap(),
        "--full-page",
    ]);
    let png = std::fs::read(image).unwrap();
    assert!(png.starts_with(b"\x89PNG"));
    assert!(u32::from_be_bytes(png[20..24].try_into().unwrap()) >= 1600);
    browser.run(&[
        "browser",
        "open",
        &format!("{}#route", site.url),
        "--wait-for",
        "#name",
    ]);
    let stale = browser
        .command(&["browser", "fill", input, "stale"])
        .args(["--session", "test"])
        .output()
        .unwrap();
    assert!(!stale.status.success());
    let fetched = browser.run(&[
        "fetch",
        &format!("{}#route", site.url),
        "--browser",
        "always",
        "--archive",
        "off",
    ]);
    assert!(
        fetched["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Robot fixture ready")
    );
    assert!(
        fetched["items"][0]["url"]
            .as_str()
            .unwrap()
            .ends_with("#route")
    );
    browser.run(&["browser", "stop"]);
    let status = browser.run(&["browser", "status"]);
    assert_eq!(status["items"][0]["content"]["running"], false);
}

#[test]
fn automatic_rendering_is_lazy_and_recovers_a_javascript_shell() {
    let Ok(executable) = std::env::var("FFETCH_TEST_BROWSER") else {
        return;
    };
    let browser = Browser {
        directory: tempfile::tempdir().unwrap(),
        executable,
    };
    let site = Site::new();
    let output = browser
        .command(&["fetch", &format!("{}static", site.url), "--archive", "off"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["items"][0]["provenance"]["method"], "http");
    assert_eq!(data["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(data["attempts"][0]["source"], "http");
    assert!(!browser.directory.path().join("browser").exists());
    let output = browser
        .command(&["fetch", &site.url, "--archive", "off"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["items"][0]["provenance"]["method"], "browser-dom");
    assert_eq!(data["attempts"][0]["source"], "http");
    assert_eq!(data["attempts"][1]["source"], "browser");
    assert!(
        data["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Robot fixture ready")
    );
}

#[test]
fn readiness_observes_attributes_redirects_and_missing_selectors() {
    let Ok(executable) = std::env::var("FFETCH_TEST_BROWSER") else {
        return;
    };
    let browser = Browser {
        directory: tempfile::tempdir().unwrap(),
        executable,
    };
    let site = Site::new();
    let attribute = browser.run(&[
        "browser",
        "open",
        &format!("{}attribute", site.url),
        "--wait-for",
        "[data-state='ready']",
    ]);
    assert!(
        attribute["items"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("Attribute fixture")
    );
    let redirected = browser.run(&[
        "browser",
        "open",
        &format!("{}redirect", site.url),
        "--wait-for",
        "article",
    ]);
    assert_eq!(
        redirected["items"][0]["url"],
        format!("{}missing", site.url)
    );
    assert_eq!(redirected["meta"]["http_status"], 404);
    assert_eq!(redirected["meta"]["http_status_observed"], true);
    let missing = browser
        .command(&[
            "browser",
            "open",
            &format!("{}static", site.url),
            "--wait-for",
            "#never-exists",
            "--timeout",
            "1",
        ])
        .args(["--session", "test"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    let missing: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(missing["status"], "error");
    assert!(!missing["errors"].as_array().unwrap().is_empty());
    let recovered = browser.run(&["browser", "open", &format!("{}static", site.url)]);
    assert!(
        recovered["items"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("Static robot document")
    );
}

#[test]
fn challenge_readiness_recovers_content_keeps_named_sessions_and_rejects_false_success() {
    let Ok(executable) = std::env::var("FFETCH_TEST_BROWSER") else {
        return;
    };
    let browser = Browser {
        directory: tempfile::tempdir().unwrap(),
        executable,
    };
    let site = Site::new();
    let recovered = browser.run(&[
        "fetch",
        &format!("{}challenge", site.url),
        "--archive",
        "off",
    ]);
    assert!(
        recovered["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Recovered research evidence")
    );
    assert_eq!(recovered["items"][0]["provenance"]["method"], "browser-dom");
    assert!(
        recovered["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("challenge interstitial"))
    );
    // Reusing a named session retains the browser's own cookies, without
    // exporting them into the Rust HTTP pool or command output.
    let session = browser.run(&[
        "fetch",
        &format!("{}session-evidence", site.url),
        "--archive",
        "off",
    ]);
    assert!(
        session["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Session clearance retained")
    );
    let selector = browser.run(&[
        "browser",
        "open",
        &format!("{}challenge-selector", site.url),
    ]);
    assert!(
        selector["items"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("Selector challenge cleared")
    );
    // A 503 challenge is eligible for browser recovery, while an ordinary
    // service failure or Retry-After remains a scheduling/backoff signal.
    let redirected = browser
        .command(&[
            "fetch",
            &format!("{}challenge-redirect", site.url),
            "--archive",
            "off",
        ])
        .output()
        .unwrap();
    assert!(
        redirected.status.success(),
        "{}",
        String::from_utf8_lossy(&redirected.stdout)
    );
    let redirected: Value = serde_json::from_slice(&redirected.stdout).unwrap();
    assert_eq!(
        redirected["meta"]["targets"][0]["http_status"], 200,
        "{redirected}"
    );
    assert!(
        redirected["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Static robot document")
    );
    assert!(
        redirected["attempts"][0]["outcome"]
            .as_str()
            .unwrap()
            .contains("challenge_detected")
    );
    for path in ["challenge-never", "denied", "challenge-opaque"] {
        let failed = browser
            .command(&[
                "fetch",
                &format!("{}{path}", site.url),
                "--archive",
                "off",
                "--timeout",
                "2",
            ])
            .args(["--session", "test"])
            .output()
            .unwrap();
        assert!(
            !failed.status.success(),
            "{path}: {}",
            String::from_utf8_lossy(&failed.stdout)
        );
        let failed: Value = serde_json::from_slice(&failed.stdout).unwrap();
        assert!(failed["items"].as_array().unwrap().is_empty());
        assert_eq!(
            failed["errors"][0]["code"], "challenge_unresolved",
            "{path}: {failed}"
        );
        assert!(
            failed["errors"][0]["next_action"]
                .as_str()
                .unwrap()
                .contains("--session")
        );
    }
    let widget = browser.run(&["fetch", &format!("{}widget", site.url), "--archive", "off"]);
    assert!(
        widget["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("embedded Turnstile widget")
    );
    // Automatic rendering must not inherit the named session's clearance.
    let isolated = browser
        .command(&[
            "fetch",
            &format!("{}session-evidence", site.url),
            "--archive",
            "off",
            "--timeout",
            "2",
        ])
        .output()
        .unwrap();
    assert!(!isolated.status.success());
    let isolated: Value = serde_json::from_slice(&isolated.stdout).unwrap();
    assert_eq!(
        isolated["errors"][0]["code"], "challenge_unresolved",
        "{isolated}"
    );
}

#[test]
fn engine_search_recovers_using_dom_and_preserves_route_attempts() {
    let Ok(executable) = std::env::var("FFETCH_TEST_BROWSER") else {
        return;
    };
    let browser = Browser {
        directory: tempfile::tempdir().unwrap(),
        executable,
    };
    let site = Site::new();
    let output = browser
        .command(&[
            "search",
            "robotics",
            "--sources",
            "web",
            "--engines",
            "google",
            "--search-pages",
            "2",
            "--limit",
            "10",
        ])
        .env("FFETCH_GOOGLE_SEARCH", format!("{}engine", site.url))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["items"][0]["title"], "Robot research");
    assert_eq!(
        result["items"][0]["provenance"]["method"],
        "google-browser-dom"
    );
    assert_eq!(
        result["items"][0]["native"]["origins"][0]["engine"],
        "google"
    );
    assert_eq!(result["attempts"][0]["source"], "google:http");
    assert_eq!(result["attempts"][1]["source"], "google:browser");
    assert_eq!(result["items"].as_array().unwrap().len(), 2);
    assert_eq!(result["attempts"].as_array().unwrap().len(), 4);
    assert_eq!(result["attempts"][2]["source"], "google:http");
    assert!(
        result["attempts"][3]["url"]
            .as_str()
            .unwrap()
            .contains("start=10")
    );
    assert_eq!(
        result["items"][1]["provenance"]["method"],
        "google-browser-dom"
    );
    assert!(
        result["items"][1]["provenance"]["source_url"]
            .as_str()
            .unwrap()
            .contains("start=10")
    );
    assert_eq!(
        result["meta"]["coverage"][0]["meta"]["web_pagination"]["pages_completed"],
        2
    );
    assert_eq!(
        result["meta"]["coverage"][0]["meta"]["retrieval_route"],
        "browser-dom"
    );
    let disabled = browser
        .command(&[
            "search",
            "robotics",
            "--sources",
            "web",
            "--engines",
            "google",
            "--browser",
            "off",
        ])
        .env("FFETCH_GOOGLE_SEARCH", format!("{}engine", site.url))
        .output()
        .unwrap();
    assert!(!disabled.status.success());
}
