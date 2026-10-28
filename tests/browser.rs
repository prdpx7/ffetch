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
                        let html = if engine_page {
                            "<!doctype html><title>Search fixture</title><div id='root'></div><script>setTimeout(()=>{document.querySelector('#root').innerHTML=\"<div class='MjjYud'><a href='https://example.org/robot'><h3>Robot research</h3></a><div class='VwiC3b'>Validated engine evidence</div></div>\"},100)</script>"
                        } else if static_page {
                            "<article>Static robot document</article>"
                        } else {
                            html
                        };
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_ffetch"));
        command
            .args(args)
            .args([
                "--no-store",
                "--timeout",
                "15",
                "--browser-path",
                &self.executable,
            ])
            .arg("--data-dir")
            .arg(self.directory.path())
            .env("NO_PROXY", "localhost,127.0.0.1")
            .env("no_proxy", "localhost,127.0.0.1");
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
