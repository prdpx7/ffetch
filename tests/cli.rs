use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

struct Fixture {
    base: String,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn new(route: impl Fn(&str) -> (u16, &'static str, String) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let quitting = stop.clone();
        let route = Arc::new(route);
        let handle = thread::spawn(move || {
            let mut workers = Vec::new();
            while !quitting.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        let route = route.clone();
                        workers.push(thread::spawn(move || {
                            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                            let mut request = Vec::new();
                            let mut buffer = [0;4096];
                            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                                match socket.read(&mut buffer) { Ok(0) | Err(_) => return, Ok(n) => request.extend_from_slice(&buffer[..n]) }
                            }
                            let request = String::from_utf8_lossy(&request);
                            let path = request.lines().next().unwrap().split_whitespace().nth(1).unwrap();
                            let (status,mime,body) = route(path);
                            let response = format!("HTTP/1.1 {status} Fixture\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                            let _ = socket.write_all(response.as_bytes());
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("fixture accept: {e}"),
                }
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            base,
            stop,
            handle: Some(handle),
        }
    }
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take().unwrap().join().unwrap();
    }
}

fn cli(args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ffetch"));
    cmd.args(["--search-mode", "fast", "--browser", "off"])
        .args(args)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost");
    for key in [
        "FFETCH_ARXIV_SEARCH",
        "FFETCH_YOUTUBE_SEARCH",
        "FFETCH_YOUTUBE_WATCH",
        "FFETCH_YOUTUBE_API_BASE",
        "FFETCH_YOUTUBE_API_KEY",
        "FFETCH_REDDIT_BASE",
        "FFETCH_REDDIT_TOKEN",
        "FFETCH_CROSSREF_SEARCH",
        "FFETCH_EUROPEPMC_SEARCH",
        "FFETCH_CONTACT_EMAIL",
        "FFETCH_SEARXNG_URL",
        "FFETCH_HN_BASE",
        "FFETCH_HN_SEARCH",
        "FFETCH_WEB_SEARCH",
        "FFETCH_BRAVE_SEARCH",
        "FFETCH_WAYBACK_CDX",
        "FFETCH_WAYBACK_REPLAY",
        "FFETCH_DATA_DIR",
    ] {
        cmd.env_remove(key);
    }
    cmd
}

fn parsed(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn page() -> String {
    "<html><title>Actuator</title><body><nav>Navigation</nav><article><h1>Actuator specification</h1><p>Torque is 4 N m.</p><a href='/cad.step'>CAD</a><table><tr><th>Torque</th><th>Mass</th></tr><tr><td>4 N m</td><td>200 g</td></tr></table></article></body></html>".into()
}

#[test]
fn fetch_query_replay_original_artifact_and_table_csv() {
    let fixture = Fixture::new(|_| (200, "text/html", page()));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let output = cli(&[
        "fetch",
        &fixture.url("spec"),
        "--archive",
        "off",
        "--data-dir",
        path.to_str().unwrap(),
    ])
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value = parsed(&output);
    assert_eq!(value["status"], "ok");
    assert!(
        value["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Torque is 4 N m")
    );
    assert!(
        !value["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Navigation")
    );
    let run = value["run_id"].as_str().unwrap();
    let query = cli(&[
        "query",
        run,
        "--data-dir",
        path.to_str().unwrap(),
        "--jq",
        ".items[0].title",
        "-r",
    ])
    .output()
    .unwrap();
    assert!(query.status.success());
    assert_eq!(String::from_utf8(query.stdout).unwrap(), "Actuator\n");
    let artifact = value["items"][0]["provenance"]["artifact"]
        .as_str()
        .unwrap();
    let original = cli(&["artifact", artifact, "--data-dir", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(original.stdout, page().as_bytes());
    let csv = cli(&[
        "fetch",
        &fixture.url("spec"),
        "--extract",
        "tables",
        "--format",
        "csv",
        "--no-store",
    ])
    .output()
    .unwrap();
    assert!(csv.status.success());
    assert_eq!(
        String::from_utf8(csv.stdout).unwrap(),
        "\"Torque\",\"Mass\"\n\"4 N m\",\"200 g\"\n"
    );
}

#[test]
fn batch_retains_success_when_another_target_is_blocked() {
    let fixture = Fixture::new(|path| {
        if path == "/blocked" {
            (403, "text/html", "<h1>Forbidden</h1>".into())
        } else {
            (200, "text/html", page())
        }
    });
    let output = cli(&[
        "fetch",
        &fixture.url("ok"),
        &fixture.url("blocked"),
        "--archive",
        "off",
        "--no-store",
    ])
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let data = parsed(&output);
    assert_eq!(data["status"], "partial");
    assert_eq!(data["items"].as_array().unwrap().len(), 1);
    assert!(
        data["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["outcome"].as_str().unwrap().contains("403"))
    );
}

#[test]
fn archive_walks_back_past_soft_404_and_resolves_original_links() {
    let fixture = Fixture::new(|path| {
        if path.starts_with("/cdx?") {
            (
                200,
                "application/json",
                json!([
                    ["timestamp", "original", "statuscode", "mimetype"],
                    [
                        "20251001000000",
                        "https://example.org/dead",
                        "200",
                        "text/html"
                    ],
                    [
                        "20261001000000",
                        "https://example.org/dead",
                        "200",
                        "text/html"
                    ]
                ])
                .to_string(),
            )
        } else if path.starts_with("/web/20261001") {
            (
                200,
                "text/html",
                "<title>404 Not Found</title><h1>Gone</h1>".into(),
            )
        } else {
            (200, "text/html", page())
        }
    });
    let output = cli(&[
        "fetch",
        "https://example.org/dead",
        "--archive",
        "latest-valid",
        "--extract",
        "links",
        "--no-store",
    ])
    .env("FFETCH_WAYBACK_CDX", fixture.url("cdx"))
    .env("FFETCH_WAYBACK_REPLAY", &fixture.base)
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = parsed(&output);
    assert_eq!(data["items"][0]["url"], "https://example.org/cad.step");
    assert_eq!(
        data["items"][0]["provenance"]["captured_at"],
        "2025-10-01T00:00:00+00:00"
    );
    assert_eq!(data["items"][0]["source"], "wayback");
    assert!(
        data["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["outcome"] == "soft_404")
    );
    assert!(!data["warnings"].as_array().unwrap().is_empty());
}

#[test]
fn all_archive_candidates_invalid_is_a_failure() {
    let fixture = Fixture::new(|path| {
        if path.starts_with("/cdx?") {
            (
                200,
                "application/json",
                json!([
                    ["timestamp", "original"],
                    ["20261001000000", "https://example.org/dead"]
                ])
                .to_string(),
            )
        } else {
            (200, "text/html", "<title>Just a moment...</title>".into())
        }
    });
    let output = cli(&[
        "fetch",
        "https://example.org/dead",
        "--archive",
        "latest-valid",
        "--no-store",
    ])
    .env("FFETCH_WAYBACK_CDX", fixture.url("cdx"))
    .env("FFETCH_WAYBACK_REPLAY", &fixture.base)
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let data = parsed(&output);
    assert_eq!(data["status"], "error");
    assert!(data["items"].as_array().unwrap().is_empty());
}

#[test]
fn hn_feed_order_and_comment_tree_preserve_native_fields() {
    let fixture = Fixture::new(|path| {
        let data = match path {
            "/v0/topstories.json" => json!([2, 1]),
            "/v0/item/1.json" => {
                json!({"id":1,"type":"story","title":"One","score":5,"kids":[3],"descendants":1,"url":"https://example.org/one"})
            }
            "/v0/item/2.json" => json!({"id":2,"type":"story","title":"Two","score":9}),
            "/v0/item/3.json" => {
                json!({"id":3,"type":"comment","text":"<p>Useful reply</p>","parent":1,"by":"scientist","kids":[4]})
            }
            "/v0/item/4.json" => json!({"id":4,"type":"comment","text":"Details","parent":3}),
            _ => panic!("unexpected route {path}"),
        };
        (200, "application/json", data.to_string())
    });
    let output = cli(&["trends", "--sources", "hn", "--limit", "2", "--no-store"])
        .env("FFETCH_HN_BASE", fixture.url("v0/"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let data = parsed(&output);
    assert_eq!(data["items"][0]["id"], "hn:2");
    assert_eq!(data["items"][1]["id"], "hn:1");
    assert_eq!(data["meta"]["momentum_measured"], false);
    let output = cli(&["thread", "1", "--limit", "1", "--no-store"])
        .env("FFETCH_HN_BASE", fixture.url("v0/"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let data = parsed(&output);
    assert_eq!(data["items"].as_array().unwrap().len(), 2);
    assert_eq!(data["items"][1]["content"]["parent_id"], 1);
    assert_eq!(data["items"][1]["native"]["by"], "scientist");
    assert_eq!(data["meta"]["comments_truncated"], true);
}

#[test]
fn hn_search_paginates_and_preserves_partial_provider_failure() {
    let fixture = Fixture::new(|path| {
        let second = path.contains("page=1");
        let id = if second { "2" } else { "1" };
        (200,"application/json",json!({"hits":[{"objectID":id,"title":format!("Paper {id}"),"points":12,"num_comments":51}],"nbPages":2,"nbHits":2}).to_string())
    });
    let output = cli(&[
        "search",
        "robotics",
        "--sources",
        "hn,unavailable",
        "--limit",
        "2",
        "--no-store",
    ])
    .env("FFETCH_HN_SEARCH", fixture.url("search"))
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let data = parsed(&output);
    assert_eq!(data["items"].as_array().unwrap().len(), 2);
    assert_eq!(data["errors"][0]["source"], "unavailable");
    assert_eq!(data["items"][1]["id"], "hn:2");
}

#[test]
fn unknown_search_markup_is_not_empty_success_and_body_limits_apply() {
    let fixture = Fixture::new(|_| {
        (
            200,
            "text/html",
            "<html><p>Unrecognized provider page</p></html>".into(),
        )
    });
    let search = cli(&["search", "robotics", "--sources", "web", "--no-store"])
        .env("FFETCH_WEB_SEARCH", fixture.url("search"))
        .env("FFETCH_BRAVE_SEARCH", fixture.url("brave"))
        .output()
        .unwrap();
    assert_eq!(search.status.code(), Some(1));
    let body = cli(&[
        "fetch",
        &fixture.url("large"),
        "--archive",
        "off",
        "--max-bytes",
        "8",
        "--no-store",
    ])
    .output()
    .unwrap();
    assert_eq!(body.status.code(), Some(1));
    assert!(
        parsed(&body)["attempts"][0]["outcome"]
            .as_str()
            .unwrap()
            .contains("max-bytes")
    );
}

#[test]
fn query_stdin_requires_no_python_or_jq_process() {
    let mut child = cli(&[
        "query",
        "-",
        "--jq",
        "[.items[] | select(.score > 5) | .title]",
        "--no-store",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()
    .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"items":[{"title":"a","score":1},{"title":"b","score":9}]}"#)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(parsed(&output), json!(["b"]));
}

#[test]
fn deadline_does_not_drop_completed_hn_items() {
    let fixture = Fixture::new(|path| {
        if path == "/v0/topstories.json" {
            return (200, "application/json", "[1,2]".into());
        }
        if path == "/v0/item/2.json" {
            thread::sleep(Duration::from_millis(1500));
        }
        (
            200,
            "application/json",
            json!({"id":if path.contains("/1.") {1} else {2},"type":"story","title":"Available"})
                .to_string(),
        )
    });
    let output = cli(&[
        "trends",
        "--sources",
        "hn",
        "--limit",
        "2",
        "--timeout",
        "1",
        "--no-store",
    ])
    .env("FFETCH_HN_BASE", fixture.url("v0/"))
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(parsed(&output)["items"].as_array().unwrap().len(), 1);
}

#[test]
fn web_search_recovers_from_challenge_without_returning_page_chrome() {
    let fixture = Fixture::new(|path| {
        if path.starts_with("/brave") {
            (
                200,
                "text/html",
                "<title>Captcha</title><p>Challenge</p>".into(),
            )
        } else {
            (200,"text/html","<div class='result'><a class='result__a' href='https://robot.example/spec'>Open gripper</a><div class='result__snippet'>Torque specification</div></div>".into())
        }
    });
    let output = cli(&[
        "search",
        "gripper",
        "--sources",
        "web",
        "--limit",
        "2",
        "--no-store",
    ])
    .env("FFETCH_WEB_SEARCH", fixture.url("ddg"))
    .env("FFETCH_BRAVE_SEARCH", fixture.url("brave"))
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = parsed(&output);
    assert_eq!(data["items"][0]["title"], "Open gripper");
    assert_eq!(data["items"][0]["provenance"]["method"], "duckduckgo-html");
    assert_eq!(data["attempts"].as_array().unwrap().len(), 2);
    assert!(!data["warnings"].as_array().unwrap().is_empty());
}

#[test]
fn brave_search_extracts_only_organic_records() {
    let fixture = Fixture::new(|_| {
        (200,"text/html","<div class='snippet' id='llm-snippet'>Generated answer</div><div class='snippet' data-type='web'><a href='https://robot.example/paper'><div class='search-snippet-title'>Robot research</div></a><div class='generic-snippet'><div class='content'>Measured results</div></div></div>".into())
    });
    let output = cli(&["search", "robot", "--sources", "web", "--no-store"])
        .env("FFETCH_WEB_SEARCH", fixture.url("ddg"))
        .env("FFETCH_BRAVE_SEARCH", fixture.url("brave"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let data = parsed(&output);
    assert_eq!(data["items"].as_array().unwrap().len(), 1);
    assert_eq!(data["items"][0]["title"], "Robot research");
    assert_eq!(data["items"][0]["text"], "Measured results");
    assert_eq!(data["items"][0]["provenance"]["method"], "brave-html");
}

#[test]
fn invalid_queries_are_rejected_before_network_or_storage() {
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = requests.clone();
    let server = Fixture::new(move |_| {
        observed.fetch_add(1, Ordering::Relaxed);
        (200, "application/json", "{}".into())
    });
    let directory = tempfile::tempdir().unwrap();
    let absent = directory.path().join("must-not-be-created");
    for flag in ["--jq", "--jmespath"] {
        let result = cli(&["fetch", &server.url("data"), "--archive", "off", flag, "["])
            .arg("--data-dir")
            .arg(&absent)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(!absent.exists());
        assert!(String::from_utf8_lossy(&result.stderr).contains("invalid"));
    }
    assert_eq!(requests.load(Ordering::Relaxed), 0);
}

// Release a blocked provider only after a document reaches the consumer. This
// proves pipeline overlap without relying on a small timing difference.
#[test]
fn jsonl_search_read_emits_document_while_another_source_is_pending() {
    let release = Arc::new(AtomicBool::new(false));
    let slow_finished = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let finished = slow_finished.clone();
    let article = Fixture::new(|_| {
        (
            200,
            "text/html",
            "<article>Validated robotics evidence</article>".into(),
        )
    });
    let article_url = article.url("paper");
    let server = Fixture::new(move |path| {
        if path.starts_with("/slow/") {
            let deadline = std::time::Instant::now() + Duration::from_secs(8);
            while !gate.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            finished.store(true, Ordering::Relaxed);
            (
                200,
                "application/json",
                json!({"hits":[],"nbPages":1}).to_string(),
            )
        } else {
            (
                200,
                "text/html",
                format!(
                    "<div class='snippet' data-type='web'><a href='{article_url}'><div class='search-snippet-title'>Robot paper</div></a></div>"
                ),
            )
        }
    });
    let mut child = cli(&[
        "search",
        "robotics",
        "--sources",
        "web,hn",
        "--limit",
        "1",
        "--read",
        "--format",
        "jsonl",
        "--no-store",
    ])
    .env("FFETCH_BRAVE_SEARCH", server.url("brave"))
    .env("FFETCH_HN_SEARCH", server.url("slow/search"))
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let reader = thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout).lines() {
            if send.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let mut events = Vec::new();
    let mut document_before_release = false;
    while let Ok(line) = receive.recv_timeout(Duration::from_secs(5)) {
        let event: Value = serde_json::from_str(&line).unwrap();
        let is_document = event["item"]["kind"] == "document";
        events.push(event);
        if is_document {
            document_before_release = !slow_finished.load(Ordering::Relaxed);
            break;
        }
        if events.last().unwrap()["type"] == "complete" {
            break;
        }
    }
    release.store(true, Ordering::Relaxed);
    if !document_before_release {
        let _ = child.kill();
    }
    let result = child.wait_with_output().unwrap();
    reader.join().unwrap();
    events.extend(
        receive
            .try_iter()
            .map(|line| serde_json::from_str::<Value>(&line).unwrap()),
    );
    assert!(
        document_before_release,
        "document waited for slow source: {events:?}; stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(events.iter().filter(|e| e["type"] == "record").count(), 2);
    let complete = events.last().unwrap();
    assert_eq!(complete["type"], "complete");
    assert_eq!(complete["result"]["status"], "ok");
    assert!(complete["result"].get("items").is_none());
    for event in &events[..events.len() - 1] {
        assert_eq!(event["run_id"], complete["result"]["run_id"]);
    }
    assert_eq!(
        events[1]["item"]["native"]["discovered_from"],
        events[0]["item"]["id"]
    );
}

#[test]
fn jsonl_whole_result_queries_preserve_aggregate_semantics() {
    let server = Fixture::new(|_| (200, "application/json", "{\"value\":1}".into()));
    let result = cli(&[
        "fetch",
        &server.url("a"),
        &server.url("b"),
        "--archive",
        "off",
        "--format",
        "jsonl",
        "--jq",
        ".items | length",
        "--no-store",
    ])
    .output()
    .unwrap();
    assert!(result.status.success());
    assert_eq!(String::from_utf8(result.stdout).unwrap().trim(), "2");
}

#[test]
fn deep_search_fans_out_and_fuses_origins_with_stream_updates() {
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = requests.clone();
    let server = Fixture::new(move |path| {
        counted.fetch_add(1, Ordering::Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while counted.load(Ordering::Relaxed) < 6 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        if counted.load(Ordering::Relaxed) < 6 {
            return (
                503,
                "text/plain",
                "engines did not start concurrently".into(),
            );
        }
        let shared = "https://research.example/shared";
        let html = if path.starts_with("/brave") {
            format!(
                "<div class='snippet' data-type='web'><a href='{shared}?utm_source=brave'><div class='search-snippet-title'>Shared robot paper</div></a></div>"
            )
        } else if path.starts_with("/duckduckgo") {
            format!(
                "<div class='result'><a class='result__a' href='{shared}'>Shared</a><div class='result__snippet'>Evidence</div></div>"
            )
        } else if path.starts_with("/google") {
            format!(
                "<div class='MjjYud'><a href='{shared}'><h3>Shared</h3></a><div class='VwiC3b'>Evidence</div></div>"
            )
        } else if path.starts_with("/bing") {
            format!(
                "<li class='b_algo'><h2><a href='{shared}'>Shared</a></h2><div class='b_caption'><p>Evidence</p></div></li>"
            )
        } else if path.starts_with("/yandex") {
            format!(
                "<li class='serp-item'><a class='OrganicTitle-Link' href='{shared}'>Shared</a></li>"
            )
        } else {
            format!("<div class='result'><h3><a href='{shared}'>Shared</a></h3></div>")
        };
        (200, "text/html", html)
    });
    let mut command = cli(&[
        "search",
        "robot paper",
        "--sources",
        "web",
        "--search-mode",
        "deep",
        "--format",
        "jsonl",
        "--no-store",
    ]);
    for (variable, path) in [
        ("FFETCH_BRAVE_SEARCH", "brave"),
        ("FFETCH_WEB_SEARCH", "duckduckgo"),
        ("FFETCH_GOOGLE_SEARCH", "google"),
        ("FFETCH_BING_SEARCH", "bing"),
        ("FFETCH_YANDEX_SEARCH", "yandex"),
        ("FFETCH_BAIDU_SEARCH", "baidu"),
    ] {
        command.env(variable, server.url(path));
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(requests.load(Ordering::Relaxed), 6);
    assert_eq!(events.iter().filter(|e| e["type"] == "record").count(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == "record_update")
            .count(),
        5
    );
    let last_record = &events[events.len() - 2]["item"];
    assert_eq!(last_record["metrics"]["engine_count"], 6);
    assert_eq!(
        last_record["native"]["origins"].as_array().unwrap().len(),
        6
    );
    let id = &events[0]["item"]["id"];
    for event in &events[..events.len() - 1] {
        assert_eq!(&event["item"]["id"], id);
    }
    let complete = events.last().unwrap();
    assert_eq!(
        complete["result"]["meta"]["coverage"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    assert_eq!(
        &complete["result"]["meta"]["ranking"]["ordered_record_ids"][0],
        id
    );
}

#[test]
fn deep_engine_failure_preserves_success_and_reports_missing_coverage() {
    let server = Fixture::new(|path| {
        if path.starts_with("/google") {
            (200, "text/html", "<title>Captcha</title>".into())
        } else {
            (200,"text/html","<div class='result'><a class='result__a' href='https://example.org/paper'>Paper</a></div>".into())
        }
    });
    let output = cli(&[
        "search",
        "paper",
        "--sources",
        "web",
        "--search-mode",
        "deep",
        "--engines",
        "google,duckduckgo",
        "--no-store",
    ])
    .env("FFETCH_GOOGLE_SEARCH", server.url("google"))
    .env("FFETCH_WEB_SEARCH", server.url("duckduckgo"))
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let data = parsed(&output);
    assert_eq!(data["items"].as_array().unwrap().len(), 1);
    assert_eq!(data["items"][0]["metrics"]["engine_count"], 1);
    assert_eq!(data["errors"][0]["source"], "engine:google");
    assert_eq!(data["meta"]["coverage"].as_array().unwrap().len(), 2);
}

#[test]
fn reservoir_output_is_bounded_and_full_evidence_remains_queryable() {
    let fixture = Fixture::new(|path| (200, "application/json", json!({"path":path}).to_string()));
    let directory = tempfile::tempdir().unwrap();
    let urls = (0..8)
        .map(|i| fixture.url(&format!("item/{i}")))
        .collect::<Vec<_>>();
    let mut command = cli(&[
        "fetch",
        "--archive",
        "off",
        "--sample",
        "3",
        "--sample-seed",
        "42",
        "--concurrency",
        "1",
    ]);
    command.args(&urls).arg("--data-dir").arg(directory.path());
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let selected = parsed(&result);
    assert_eq!(selected["items"].as_array().unwrap().len(), 3);
    assert_eq!(selected["meta"]["sampling"]["population_records"], 8);
    assert_eq!(selected["meta"]["sampling"]["algorithm"], "reservoir-r");
    let saved = cli(&["inspect", selected["run_id"].as_str().unwrap()])
        .arg("--data-dir")
        .arg(directory.path())
        .output()
        .unwrap();
    assert_eq!(parsed(&saved)["items"].as_array().unwrap().len(), 8);
    let jsonl = cli(&[
        "fetch",
        "--archive",
        "off",
        "--sample",
        "2",
        "--no-store",
        "--format",
        "jsonl",
    ])
    .args(&urls)
    .output()
    .unwrap();
    assert!(jsonl.status.success());
    let events = String::from_utf8(jsonl.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(events.iter().filter(|e| e["type"] == "record").count(), 2);
    assert_eq!(
        events.last().unwrap()["result"]["meta"]["sampling"]["selected_records"],
        2
    );
}

#[test]
fn scholarly_search_preserves_identifiers_abstracts_and_cursor_pages() {
    let fixture = Fixture::new(|path| {
        let url = url::Url::parse(&format!("http://fixture{path}")).unwrap();
        let params = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        if path.starts_with("/crossref") {
            assert_eq!(params.get("filter").unwrap(), "from-pub-date:2025-01-01");
            assert_eq!(params.get("sort").unwrap(), "score");
            assert_eq!(params.get("order").unwrap(), "desc");
            assert_eq!(params.get("query.bibliographic").unwrap(), "robotics");
            let second = params.get("cursor").unwrap() == "next";
            let items = if second {
                vec![
                    json!({"DOI":"10.1234/101","title":["Final paper"],"issued":{"date-parts":[[2025]]}}),
                ]
            } else {
                (1..=100).map(|i|json!({"DOI":format!("10.1234/{i}"),"title":[format!("Paper {i}")],"abstract":"<jats:p>Measured <b>torque</b>.</jats:p>","author":[{"given":"Ada","family":"Lovelace"}],"published":{"date-parts":[[2025,2,3]]},"reference":[{"DOI":"10.1234/previous"}],"is-referenced-by-count":7})).collect()
            };
            (200,"application/json",json!({"status":"ok","message":{"items":items,"total-results":101,"next-cursor":if second {"done"}else{"next"}}}).to_string())
        } else {
            assert!(params.get("query").unwrap().contains("SRC:MED"));
            assert_eq!(params.get("resultType").unwrap(), "core");
            (200,"application/json",json!({"hitCount":1,"resultList":{"result":[{"id":"12345","source":"MED","title":"Robotic medicine","abstractText":"<p>Measured clinical research.</p>","authorString":"A. Author","firstPublicationDate":"2025-03-04","doi":"10.1234/medical","pmcid":"PMC12345","isOpenAccess":"Y","fullTextUrlList":{"fullTextUrl":[{"url":"https://example.org/full"}]}}]}}).to_string())
        }
    });
    let output = cli(&[
        "search",
        "robotics",
        "--sources",
        "crossref,pubmed",
        "--limit",
        "101",
        "--since",
        "2025-01-01T12:00:00Z",
        "--no-store",
    ])
    .env("FFETCH_CROSSREF_SEARCH", fixture.url("crossref"))
    .env("FFETCH_EUROPEPMC_SEARCH", fixture.url("pmc"))
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = parsed(&output);
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), 102);
    let first = items
        .iter()
        .find(|r| r["content"]["doi"] == "10.1234/1")
        .unwrap();
    assert_eq!(first["author"], "Ada Lovelace");
    assert_eq!(first["published_at"], "2025-02-03");
    assert!(first["text"].as_str().unwrap().contains("torque"));
    assert_eq!(first["content"]["references"][0]["DOI"], "10.1234/previous");
    let last = items
        .iter()
        .find(|r| r["content"]["doi"] == "10.1234/101")
        .unwrap();
    assert_eq!(last["published_at"], "2025");
    let medical = items.iter().find(|r| r["source"] == "pubmed").unwrap();
    assert_eq!(medical["content"]["pmid"], "12345");
    assert_eq!(medical["url"], "https://pubmed.ncbi.nlm.nih.gov/12345/");
    assert_eq!(medical["content"]["full_text_retrieved"], false);
    assert_eq!(medical["provenance"]["method"], "europepmc-api");
    assert_eq!(data["attempts"].as_array().unwrap().len(), 3);
}

#[test]
fn scholarly_pagination_failure_keeps_completed_papers() {
    let fixture = Fixture::new(|path| {
        if path.contains("cursor=%2A") || path.contains("cursor=*") {
            let items = (0..100)
                .map(|i| json!({"DOI":format!("10.1234/{i}"),"title":["Paper"]}))
                .collect::<Vec<_>>();
            (
                200,
                "application/json",
                json!({"message":{"items":items,"next-cursor":"broken","total-results":101}})
                    .to_string(),
            )
        } else {
            (
                200,
                "application/json",
                "{\"message\":{\"items\":null}}".into(),
            )
        }
    });
    let result = cli(&[
        "search",
        "paper",
        "--sources",
        "crossref",
        "--limit",
        "101",
        "--no-store",
    ])
    .env("FFETCH_CROSSREF_SEARCH", fixture.url("crossref"))
    .output()
    .unwrap();
    assert_eq!(result.status.code(), Some(3));
    let data = parsed(&result);
    assert_eq!(data["items"].as_array().unwrap().len(), 100);
    assert_eq!(data["errors"][0]["source"], "crossref");
}

#[test]
fn scholarly_sources_merge_doi_metadata_and_stream_an_update() {
    let fixture = Fixture::new(|path| {
        if path.starts_with("/crossref") {
            (200,"application/json",json!({"message":{"items":[{"DOI":"10.1234/ROBOT","type":"journal-article","title":["Publisher title"],"abstract":"<p>Publisher abstract</p>","published":{"date-parts":[[2024]]},"reference":[{"DOI":"10.1234/ref"}]}]}}).to_string())
        } else {
            (200,"application/json",json!({"hitCount":1,"resultList":{"result":[{"id":"12345","source":"MED","doi":"10.1234/robot","title":"Indexed title","firstPublicationDate":"2025-02-03","pmcid":"PMC12345"}]}}).to_string())
        }
    });
    let result = cli(&[
        "search",
        "robot",
        "--sources",
        "crossref,europepmc",
        "--limit",
        "1",
        "--format",
        "jsonl",
        "--no-store",
    ])
    .env("FFETCH_CROSSREF_SEARCH", fixture.url("crossref"))
    .env("FFETCH_EUROPEPMC_SEARCH", fixture.url("pmc"))
    .output()
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let events = String::from_utf8(result.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str::<Value>(s).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["type"], "record");
    assert_eq!(events[1]["type"], "record_update");
    let merged = &events[1]["item"];
    assert_eq!(events[0]["item"]["id"], merged["id"]);
    assert_eq!(merged["title"], "Indexed title");
    assert_eq!(merged["text"], "Publisher abstract");
    assert_eq!(merged["content"]["references"][0]["DOI"], "10.1234/ref");
    assert_eq!(merged["native"]["field_sources"]["text"], "crossref");
    assert_eq!(
        merged["native"]["research_origins"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        merged["native"]["conflicts"]["published_at"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        events[2]["result"]["meta"]["coverage"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

fn reddit_post(id: &str) -> Value {
    json!({"kind":"t3","data":{"id":id,"name":format!("t3_{id}"),"title":format!("Robot {id}"),"selftext":"Source discussion","author":"researcher","created_utc":1740000000,"score":42,"num_comments":3,"subreddit":"robotics","permalink":format!("/r/robotics/comments/{id}/robot/"),"is_self":true}})
}
#[test]
fn reddit_search_paginates_and_filters_native_timestamps() {
    let fixture = Fixture::new(|path| {
        assert!(path.starts_with("/r/robotics/search.json?"));
        let url = url::Url::parse(&format!("http://fixture{path}")).unwrap();
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query.get("q").unwrap(), "gripper");
        let second = query.contains_key("after");
        let children = if second {
            vec![reddit_post("b")]
        } else {
            vec![reddit_post("a")]
        };
        (200,"application/json",json!({"kind":"Listing","data":{"after":if second{Value::Null}else{json!("t3_a")},"children":children}}).to_string())
    });
    let result = cli(&[
        "search",
        "gripper",
        "--sources",
        "reddit",
        "--community",
        "robotics",
        "--limit",
        "2",
        "--since",
        "2025-01-01T00:00:00Z",
        "--no-store",
    ])
    .env("FFETCH_REDDIT_BASE", &fixture.base)
    .output()
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let data = parsed(&result);
    assert_eq!(data["items"].as_array().unwrap().len(), 2);
    assert_eq!(data["items"][0]["content"]["native_id"], "t3_a");
    assert_eq!(data["items"][1]["content"]["native_id"], "t3_b");
    assert_eq!(data["items"][0]["metrics"]["score"], 42);
    assert_eq!(data["attempts"].as_array().unwrap().len(), 2);
}
#[test]
fn reddit_feed_and_thread_keep_rank_parents_and_unexpanded_comments() {
    let fixture = Fixture::new(|path| {
        if path.starts_with("/r/robotics/top.json") {
            assert!(path.contains("t=week"));
            (
                200,
                "application/json",
                json!({"kind":"Listing","data":{"after":null,"children":[reddit_post("abc")]}})
                    .to_string(),
            )
        } else {
            let reply = json!({"kind":"t1","data":{"id":"c2","name":"t1_c2","parent_id":"t1_c1","link_id":"t3_abc","body":"Nested evidence","permalink":"/r/robotics/comments/abc/robot/c2/","replies":""}});
            let root = json!({"kind":"t1","data":{"id":"c1","name":"t1_c1","parent_id":"t3_abc","link_id":"t3_abc","body":"First evidence","permalink":"/r/robotics/comments/abc/robot/c1/","replies":{"kind":"Listing","data":{"children":[reply]}}}});
            let more = json!({"kind":"more","data":{"children":["c3"],"count":1}});
            (200,"application/json",json!([{"kind":"Listing","data":{"children":[reddit_post("abc")]}},{"kind":"Listing","data":{"children":[root,more]}}]).to_string())
        }
    });
    let result = cli(&[
        "trends",
        "--sources",
        "reddit",
        "--community",
        "robotics",
        "--feed",
        "top",
        "--period",
        "week",
        "--no-store",
    ])
    .env("FFETCH_REDDIT_BASE", &fixture.base)
    .output()
    .unwrap();
    assert!(result.status.success());
    let data = parsed(&result);
    assert_eq!(data["items"][0]["metrics"]["rank"], 1);
    assert_eq!(data["meta"]["reddit"]["momentum_measured"], false);
    let result = cli(&[
        "thread",
        "https://www.reddit.com/r/robotics/comments/abc/robot/",
        "--limit",
        "2",
        "--no-store",
    ])
    .env("FFETCH_REDDIT_BASE", &fixture.base)
    .output()
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let data = parsed(&result);
    assert_eq!(data["items"].as_array().unwrap().len(), 3);
    assert_eq!(data["items"][2]["content"]["parent_id"], "t1_c1");
    assert_eq!(data["meta"]["comments_truncated"], true);
    assert_eq!(data["meta"]["unexpanded_comment_ids"][0], "c3");
    assert!(data["items"][1]["native"].get("replies").is_none());
}
#[test]
fn reddit_auth_is_not_sent_to_an_endpoint_override() {
    let fixture = Fixture::new(|_| panic!("credentials must be rejected before a fixture request"));
    let result = cli(&["search", "robot", "--sources", "reddit", "--no-store"])
        .env("FFETCH_REDDIT_BASE", &fixture.base)
        .env("FFETCH_REDDIT_TOKEN", "fixture-secret-do-not-print")
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    let stdout = String::from_utf8(result.stdout).unwrap();
    assert!(!stdout.contains("fixture-secret-do-not-print"));
    assert!(stdout.contains("official HTTPS OAuth origin"));
}

#[test]
fn reddit_api_block_recovers_scoped_web_links_without_claiming_native_metrics() {
    let fixture = Fixture::new(|path| {
        if path.starts_with("/native/") {
            (403, "application/json", "{}".into())
        } else if path.starts_with("/brave") {
            assert!(path.contains("site%3Areddit.com%2Fr%2Frobotics"));
            (200,"text/html","<div class='snippet' data-type='web'><a href='https://www.reddit.com/r/robotics/comments/abc/robot/'><div class='search-snippet-title'>Robot discussion</div></a></div><div class='snippet' data-type='web'><a href='https://other.example/robot'><div class='search-snippet-title'>Other page</div></a></div>".into())
        } else {
            (200, "text/html", "<title>Captcha</title>".into())
        }
    });
    let result = cli(&[
        "search",
        "robot",
        "--sources",
        "reddit",
        "--community",
        "robotics",
        "--no-store",
    ])
    .env("FFETCH_REDDIT_BASE", fixture.url("native/"))
    .env("FFETCH_BRAVE_SEARCH", fixture.url("brave"))
    .env("FFETCH_WEB_SEARCH", fixture.url("ddg"))
    .output()
    .unwrap();
    assert_eq!(result.status.code(), Some(3));
    let data = parsed(&result);
    assert_eq!(data["items"].as_array().unwrap().len(), 1);
    assert_eq!(data["items"][0]["source"], "web");
    assert_eq!(data["items"][0]["kind"], "search_result");
    assert_eq!(
        data["items"][0]["content"]["native_metadata_available"],
        false
    );
    assert_eq!(data["items"][0]["metrics"]["engine_count"], 1);
    assert_eq!(data["errors"][0]["code"], "native_metadata_unavailable");
    assert_eq!(
        data["meta"]["coverage"][0]["meta"]["discovery_fallback"]["native_comments"],
        false
    );
}

#[test]
fn youtube_search_parses_native_initial_data_and_skips_promoted_video_nodes() {
    let fixture = Fixture::new(|path| {
        assert!(path.contains("search_query=robot"));
        let video = json!({"videoId":"abcdefghijk","title":{"runs":[{"text":"Robot gripper"}]},"ownerText":{"runs":[{"text":"Research lab"}]},"publishedTimeText":{"simpleText":"3 weeks ago"},"viewCountText":{"simpleText":"1.2K views"}});
        let ad = json!({"videoId":"zzzzzzzzzzz","title":{"simpleText":"Sponsored"}});
        let data = json!({"contents":{"twoColumnSearchResultsRenderer":{"primaryContents":{"contents":[{"videoRenderer":video},{"adSlotRenderer":{"videoRenderer":ad}}]}}}});
        (
            200,
            "text/html",
            format!("<script>var ytInitialData = {data};</script>"),
        )
    });
    let result = cli(&[
        "search",
        "robot",
        "--sources",
        "youtube",
        "--limit",
        "2",
        "--no-store",
    ])
    .env("FFETCH_YOUTUBE_SEARCH", fixture.url("results"))
    .output()
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let data = parsed(&result);
    assert_eq!(data["items"].as_array().unwrap().len(), 1);
    assert_eq!(data["items"][0]["title"], "Robot gripper");
    assert_eq!(data["items"][0]["author"], "Research lab");
    assert!(data["items"][0]["published_at"].is_null());
    assert_eq!(data["items"][0]["metrics"]["views_label"], "1.2K views");
    assert_eq!(
        data["items"][0]["provenance"]["method"],
        "youtube-initial-data"
    );
}
#[test]
fn youtube_video_metadata_survives_unavailable_caption_language() {
    let fixture = Fixture::new(|_| {
        let data = json!({"videoDetails":{"videoId":"abcdefghijk","title":"Robot gripper","shortDescription":"Measured torque","author":"Lab","lengthSeconds":"120","viewCount":"456"},"microformat":{"playerMicroformatRenderer":{"publishDate":"2025-01-02"}},"captions":{"playerCaptionsTracklistRenderer":{"captionTracks":[{"languageCode":"en","kind":"asr","name":{"simpleText":"English"},"baseUrl":"https://www.youtube.com/api/timedtext?signature=private-fixture-signature"}]}}});
        (
            200,
            "text/html",
            format!("<script>var ytInitialPlayerResponse = {data};</script>"),
        )
    });
    let result = cli(&[
        "fetch",
        "https://www.youtube.com/watch?v=abcdefghijk",
        "--archive",
        "off",
        "--no-store",
    ])
    .env("FFETCH_YOUTUBE_WATCH", fixture.url("watch"))
    .output()
    .unwrap();
    assert!(result.status.success());
    let data = parsed(&result);
    assert_eq!(data["items"][0]["metrics"]["views"], 456);
    assert_eq!(data["items"][0]["content"]["duration_seconds"], 120);
    let result = cli(&[
        "transcript",
        "https://youtu.be/abcdefghijk",
        "--language",
        "fr",
        "--no-store",
    ])
    .env("FFETCH_YOUTUBE_WATCH", fixture.url("watch"))
    .output()
    .unwrap();
    assert_eq!(result.status.code(), Some(3));
    let text = String::from_utf8_lossy(&result.stdout);
    assert!(!text.contains("private-fixture-signature"));
    let data = parsed(&result);
    assert_eq!(data["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        data["items"][0]["content"]["caption_tracks"][0]["automatic"],
        true
    );
    assert!(
        data["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("caption language")
    );
}
#[test]
fn youtube_api_key_cannot_be_sent_to_an_endpoint_override() {
    let fixture = Fixture::new(|_| panic!("API key must not reach fixture origin"));
    let result = cli(&[
        "trends",
        "--sources",
        "youtube",
        "--feed",
        "popular",
        "--no-store",
    ])
    .env("FFETCH_YOUTUBE_API_KEY", "fixture-private-key")
    .env("FFETCH_YOUTUBE_API_BASE", &fixture.base)
    .output()
    .unwrap();
    assert_eq!(result.status.code(), Some(1));
    let text = String::from_utf8_lossy(&result.stdout);
    assert!(!text.contains("fixture-private-key"));
    assert!(text.contains("official HTTPS API origin"));
}

#[test]
fn youtube_blocked_search_recovers_video_links_as_partial_discovery() {
    let fixture = Fixture::new(|path| {
        if path.starts_with("/native") {
            (200, "text/html", "<title>Captcha</title>".into())
        } else if path.starts_with("/brave") {
            (200,"text/html","<div class='snippet' data-type='web'><a href='https://www.youtube.com/watch?v=abcdefghijk'><div class='search-snippet-title'>Robot video</div></a></div>".into())
        } else {
            (200, "text/html", "<title>Captcha</title>".into())
        }
    });
    let result = cli(&["search", "robot", "--sources", "youtube", "--no-store"])
        .env("FFETCH_YOUTUBE_SEARCH", fixture.url("native"))
        .env("FFETCH_BRAVE_SEARCH", fixture.url("brave"))
        .env("FFETCH_WEB_SEARCH", fixture.url("ddg"))
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(3));
    let data = parsed(&result);
    assert_eq!(data["items"][0]["kind"], "search_result");
    assert_eq!(data["items"][0]["content"]["video_id"], "abcdefghijk");
    assert_eq!(
        data["items"][0]["content"]["native_metadata_available"],
        false
    );
    assert_eq!(data["errors"][0]["code"], "native_metadata_unavailable");
}

fn arxiv_feed(entries: &str, total: usize) -> String {
    format!(
        "<?xml version='1.0' encoding='UTF-8'?><feed xmlns='http://www.w3.org/2005/Atom' xmlns:o='http://a9.com/-/spec/opensearch/1.1/' xmlns:a='http://arxiv.org/schemas/atom'><o:totalResults>{total}</o:totalResults>{entries}</feed>"
    )
}
fn arxiv_entry(id: &str) -> String {
    format!(
        "<entry><id>http://arxiv.org/abs/{id}</id><title>Robot &amp; gripper</title><summary><![CDATA[Abstract with <math> and torque.]]></summary><published>2025-02-03T12:00:00Z</published><updated>2025-03-04T12:00:00Z</updated><author><name>Ada Researcher</name><a:affiliation>Lab</a:affiliation></author><a:doi>10.1234/published</a:doi><a:primary_category term='cs.RO'/><category term='cs.RO'/><link title='pdf' href='https://arxiv.org/pdf/{id}' type='application/pdf'/></entry>"
    )
}
#[test]
fn arxiv_search_and_versioned_lookup_preserve_atom_metadata() {
    let fixture = Fixture::new(|path| {
        let url = url::Url::parse(&format!("http://fixture{path}")).unwrap();
        let params = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        if params.contains_key("id_list") {
            assert_eq!(params.get("id_list").unwrap(), "2501.12345v2");
        } else {
            assert!(params.get("search_query").unwrap().contains("all:robot"));
            assert_eq!(params.get("sortBy").unwrap(), "relevance");
        }
        (
            200,
            "application/atom+xml",
            arxiv_feed(&arxiv_entry("2501.12345v2"), 1),
        )
    });
    for args in [
        vec![
            "search",
            "robot gripper",
            "--sources",
            "arxiv",
            "--no-store",
        ],
        vec!["fetch", "https://arxiv.org/abs/2501.12345v2", "--no-store"],
    ] {
        let result = cli(&args)
            .env("FFETCH_ARXIV_SEARCH", fixture.url("query"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let data = parsed(&result);
        let item = &data["items"][0];
        assert_eq!(item["title"], "Robot & gripper");
        assert!(item["text"].as_str().unwrap().contains("<math>"));
        assert_eq!(item["content"]["arxiv_id"], "2501.12345v2");
        assert_eq!(item["content"]["version"], 2);
        assert_eq!(item["content"]["primary_category"], "cs.RO");
        assert_eq!(item["content"]["doi"], "10.1234/published");
        assert_eq!(item["content"]["authors"][0]["affiliations"][0], "Lab");
        assert_eq!(item["provenance"]["method"], "arxiv-atom-api");
        assert_eq!(item["content"]["full_text_retrieved"], false);
    }
}
