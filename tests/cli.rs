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
        Self::bytes(move |path| {
            let (status, mime, body) = route(path);
            (status, mime, body.into_bytes())
        })
    }
    fn bytes(route: impl Fn(&str) -> (u16, &'static str, Vec<u8>) + Send + Sync + 'static) -> Self {
        Self::request(move |path, _| route(path))
    }
    fn request(
        route: impl Fn(&str, &str) -> (u16, &'static str, Vec<u8>) + Send + Sync + 'static,
    ) -> Self {
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
                            let (status,mime,body) = route(path,&request);
                            let response = format!("HTTP/1.1 {status} Fixture\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len());
                            let _ = socket.write_all(response.as_bytes());
                            let _ = socket.write_all(&body);
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
        "FFETCH_GITHUB_BASE",
        "FFETCH_GITHUB_TOKEN",
        "FFETCH_ARXIV_PDF_BASE",
        "FFETCH_YOUTUBE_SEARCH",
        "FFETCH_YOUTUBE_WATCH",
        "FFETCH_YOUTUBE_API_BASE",
        "FFETCH_YOUTUBE_API_KEY",
        "FFETCH_REDDIT_BASE",
        "FFETCH_REDDIT_TOKEN",
        "FFETCH_CROSSREF_SEARCH",
        "FFETCH_CROSSREF_WORKS",
        "FFETCH_EUROPEPMC_SEARCH",
        "FFETCH_EUROPEPMC_FULLTEXT",
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

#[test]
fn duckduckgo_header_profile_recovers_organic_results_without_changing_document_requests() {
    let server = Fixture::request(|path, request| {
        let request = request.to_ascii_lowercase();
        if path.starts_with("/search?") {
            if !request.contains("chrome/152.") {
                return (202, "text/html", b"<title>Captcha</title>".to_vec());
            }
            let host = request
                .lines()
                .find_map(|line| line.strip_prefix("host: "))
                .unwrap();
            (200,"text/html",format!("<div class='result'><a class='result__a' href='http://{host}/article'>Gripper CAD</a><p class='result__snippet'>Open research evidence</p></div>").into_bytes())
        } else if path == "/article" {
            if !request.contains("user-agent: ffetch/") || request.contains("chrome/152.") {
                return (
                    400,
                    "text/plain",
                    b"HTTP profile leaked to document reading".to_vec(),
                );
            }
            (
                200,
                "text/html",
                b"<article>Gripper CAD body</article>".to_vec(),
            )
        } else {
            (404, "text/plain", b"unexpected fixture request".to_vec())
        }
    });
    let output = cli(&[
        "search",
        "gripper",
        "--sources",
        "web",
        "--engines",
        "duckduckgo",
        "--search-mode",
        "balanced",
        "--read",
        "--no-store",
    ])
    .env("FFETCH_WEB_SEARCH", server.url("search"))
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let run = parsed(&output);
    assert_eq!(run["items"].as_array().unwrap().len(), 2);
    assert_eq!(run["items"][0]["title"], "Gripper CAD");
    assert!(
        run["items"][1]["text"]
            .as_str()
            .unwrap()
            .contains("Gripper CAD body")
    );
    assert_eq!(
        run["meta"]["coverage"][0]["meta"]["request_profile"],
        "chrome152-user-agent-http1"
    );
    assert_eq!(run["meta"]["http"]["requests_started"], 2);
    assert_eq!(run["meta"]["http"]["responses_by_protocol"]["http1"], 2);
    assert!(
        run["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| !a["source"].as_str().unwrap().contains("browser"))
    );
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
fn hn_thread_schedules_descendants_while_a_sibling_is_pending() {
    let descendant_started = Arc::new(AtomicBool::new(false));
    let slow_finished = Arc::new(AtomicBool::new(false));
    let overlapped = Arc::new(AtomicBool::new(false));
    let started = descendant_started.clone();
    let finished = slow_finished.clone();
    let overlap = overlapped.clone();
    let fixture = Fixture::new(move |path| {
        let data = match path {
            "/v0/item/1.json" => json!({"id":1,"type":"story","kids":[2,3,3,1]}),
            "/v0/item/2.json" => {
                // A batch scheduler cannot reach item 5 until this request finishes.
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while !started.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(2));
                }
                finished.store(true, Ordering::SeqCst);
                json!({"id":2,"type":"comment","parent":1})
            }
            "/v0/item/3.json" => json!({"id":3,"type":"comment","parent":1,"kids":[4,2]}),
            "/v0/item/4.json" => json!({"id":4,"type":"comment","parent":3,"kids":[5,3]}),
            "/v0/item/5.json" => {
                overlap.store(!finished.load(Ordering::SeqCst), Ordering::SeqCst);
                started.store(true, Ordering::SeqCst);
                json!({"id":5,"type":"comment","parent":4})
            }
            _ => panic!("unexpected or duplicate request {path}"),
        };
        (200, "application/json", data.to_string())
    });
    let output = cli(&[
        "thread",
        "1",
        "--limit",
        "4",
        "--concurrency",
        "2",
        "--no-store",
    ])
    .env("FFETCH_HN_BASE", fixture.url("v0/"))
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = parsed(&output);
    assert!(
        overlapped.load(Ordering::SeqCst),
        "descendant was held behind a slow sibling"
    );
    let mut ids: Vec<_> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, ["hn:1", "hn:2", "hn:3", "hn:4", "hn:5"]);
    assert_eq!(data["meta"]["comment_attempts"], 4);
    assert_eq!(data["meta"]["comments_coverage"], "complete");
    assert_eq!(data["meta"]["comments_truncated"], false);
}

#[test]
fn hn_thread_deadline_reports_pending_work_and_retains_fast_descendants() {
    let fixture = Fixture::new(|path| {
        let data = match path {
            "/v0/item/1.json" => json!({"id":1,"type":"story","kids":[2,3]}),
            "/v0/item/2.json" => {
                thread::sleep(Duration::from_millis(1500));
                json!({"id":2,"type":"comment","parent":1})
            }
            "/v0/item/3.json" => json!({"id":3,"type":"comment","parent":1,"kids":[4]}),
            "/v0/item/4.json" => json!({"id":4,"type":"comment","parent":3,"kids":[5]}),
            _ => panic!("request exceeded comment limit: {path}"),
        };
        (200, "application/json", data.to_string())
    });
    let output = cli(&[
        "thread",
        "1",
        "--limit",
        "3",
        "--concurrency",
        "2",
        "--timeout",
        "1",
        "--no-store",
    ])
    .env("FFETCH_HN_BASE", fixture.url("v0/"))
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let data = parsed(&output);
    assert_eq!(
        data["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["hn:1", "hn:3", "hn:4"]
    );
    assert_eq!(data["meta"]["comment_attempts"], 3);
    assert_eq!(data["meta"]["comments_returned"], 2);
    assert_eq!(data["meta"]["comment_failures"], 0);
    assert_eq!(data["meta"]["comments_in_flight"], 1);
    assert_eq!(data["meta"]["comments_pending"], 1);
    assert_eq!(data["meta"]["comments_truncated"], true);
    assert_eq!(data["meta"]["comments_coverage"], "incomplete");
    assert!(
        data["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("deadline")
    );
}

#[test]
fn hn_thread_rejects_mismatched_items_and_zero_limit() {
    let fixture = Fixture::new(|path| {
        let data = match path {
            "/v0/item/1.json" => json!({"id":1,"type":"story","kids":[2,3,2,1]}),
            "/v0/item/2.json" => json!({"id":999,"type":"comment","parent":1,"kids":[4]}),
            "/v0/item/3.json" => json!({"id":3,"type":"comment","parent":1}),
            _ => panic!("must not follow a mismatched response: {path}"),
        };
        (200, "application/json", data.to_string())
    });
    let output = cli(&["thread", "1", "--limit", "2", "--no-store"])
        .env("FFETCH_HN_BASE", fixture.url("v0/"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let data = parsed(&output);
    assert_eq!(data["items"].as_array().unwrap().len(), 2);
    assert_eq!(data["meta"]["comment_attempts"], 2);
    assert_eq!(data["meta"]["comment_failures"], 1);
    assert_eq!(data["meta"]["comments_coverage"], "incomplete");
    assert!(
        data["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("does not match")
    );
    let output = cli(&["thread", "1", "--limit", "0", "--no-store"])
        .env("FFETCH_HN_BASE", fixture.url("v0/"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("greater than zero"));
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
    for args in [
        vec!["search", "q", "--search-pages", "2"], // fast mode is set by cli()
        vec![
            "search",
            "q",
            "--search-mode",
            "balanced",
            "--search-pages",
            "0",
        ],
        vec![
            "search",
            "q",
            "--search-mode",
            "balanced",
            "--search-pages",
            "21",
        ],
        vec!["capabilities", "--search-pages", "2"],
    ] {
        let result = cli(&args).arg("--data-dir").arg(&absent).output().unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(!absent.exists());
        assert!(String::from_utf8_lossy(&result.stderr).contains("--search-pages"));
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
        "--origin-concurrency",
        "8",
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

fn github_repo(id: u64) -> Value {
    json!({"id":id,"full_name":format!("lab/gripper{id}"),"html_url":format!("https://github.com/lab/gripper{id}"),"description":"Open gripper CAD","owner":{"login":"lab"},"default_branch":"main","stargazers_count":42,"forks_count":7,"open_issues_count":3,"created_at":"2026-01-01T00:00:00Z","pushed_at":"2026-10-29T10:00:00Z","topics":["robotics","cad"],"license":{"spdx_id":"MIT"}})
}
fn github_readme(id: u64, text: &str) -> Value {
    use base64::Engine;
    json!({"type":"file","name":"README.md","path":"README.md","sha":"a".repeat(40),"size":text.len(),"encoding":"base64","content":base64::engine::general_purpose::STANDARD.encode(text),"html_url":format!("https://github.com/lab/gripper{id}/blob/main/README.md")})
}
fn github_issue() -> Value {
    json!({"id":91,"number":7,"html_url":"https://github.com/lab/gripper1/pull/7","title":"Improve force control","body":"Measured **4 N** tendon force.","pull_request":{"url":"https://api.github.com/repos/lab/gripper1/pulls/7"},"user":{"login":"ada"},"created_at":"2026-10-29T10:00:00Z","comments":150,"state":"open"})
}

#[test]
fn github_repository_search_paginates_filters_exact_times_and_reports_incomplete_results() {
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = requests.clone();
    let server = Fixture::new(move |path| {
        counted.fetch_add(1, Ordering::Relaxed);
        let url = url::Url::parse(&format!("http://fixture{path}")).unwrap();
        assert_eq!(url.path(), "/search/repositories");
        let args = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(args["per_page"], "3");
        assert!(args["q"].contains("gripper language:Rust pushed:>=2026-10-29"));
        let first = args["page"] == "1";
        let mut old = github_repo(2);
        old["pushed_at"] = json!("2026-10-29T08:00:00Z");
        let items = if first {
            vec![github_repo(1), old, github_repo(1)]
        } else {
            vec![github_repo(3), github_repo(4), github_repo(5)]
        };
        (
            200,
            "application/json",
            json!({"total_count":6,"incomplete_results":!first,"items":items}).to_string(),
        )
    });
    let output = cli(&[
        "search",
        "gripper language:Rust",
        "--sources",
        "github",
        "--limit",
        "3",
        "--since",
        "2026-10-29T09:00:00Z",
        "--no-store",
    ])
    .env("FFETCH_GITHUB_BASE", &server.base)
    .output()
    .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let run = parsed(&output);
    assert_eq!(requests.load(Ordering::Relaxed), 2);
    assert_eq!(run["items"].as_array().unwrap().len(), 3);
    assert_eq!(run["items"][0]["metrics"]["stars"], 42);
    assert_eq!(run["items"][0]["content"]["license"], "MIT");
    assert_eq!(run["items"][1]["content"]["repository"], "lab/gripper3");
    assert_eq!(run["errors"][0]["code"], "github_search_incomplete");
    assert_eq!(
        run["meta"]["coverage"][0]["meta"]["github_search"]["pages_completed"],
        2
    );
    assert_eq!(
        run["meta"]["coverage"][0]["meta"]["github_search"]["stop_reason"],
        "result_limit"
    );
}

#[test]
fn github_search_rejects_unknown_schemas_and_keeps_completed_pages_on_failure() {
    let server = Fixture::new(|path| {
        let url = url::Url::parse(&format!("http://fixture{path}")).unwrap();
        let args = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        let query = args["q"].as_ref();
        if query == "bad" {
            (
                200,
                "application/json",
                "{\"message\":\"Unexpected API shape\"}".into(),
            )
        } else if query == "empty" {
            (
                200,
                "application/json",
                json!({"total_count":0,"incomplete_results":false,"items":[]}).to_string(),
            )
        } else if args["page"] == "2" {
            (
                403,
                "application/json",
                json!({"message":"API rate limit exceeded"}).to_string(),
            )
        } else {
            (200,"application/json",json!({"total_count":10,"incomplete_results":false,"items":[github_repo(1),github_repo(1)]}).to_string())
        }
    });
    for (query, code, count) in [("bad", 1, 0), ("empty", 0, 0), ("later-failure", 3, 1)] {
        let output = cli(&[
            "search",
            query,
            "--sources",
            "github",
            "--limit",
            "2",
            "--no-store",
        ])
        .env("FFETCH_GITHUB_BASE", &server.base)
        .output()
        .unwrap();
        assert_eq!(output.status.code(), Some(code));
        let run = parsed(&output);
        assert_eq!(run["items"].as_array().unwrap().len(), count);
        if query == "later-failure" {
            assert_eq!(run["errors"][0]["code"], "rate_limited");
        }
    }
}

#[test]
fn github_native_repository_read_retains_readme_bytes_and_distinct_provenance() {
    let text = "# Open gripper\nTorque: 4 N m.\n";
    let server = Fixture::new(move |path| {
        if path.starts_with("/repos/lab/gripper1/readme") {
            (200, "application/json", github_readme(1, text).to_string())
        } else {
            assert_eq!(path, "/repos/lab/gripper1");
            (200, "application/json", github_repo(1).to_string())
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let output = cli(&["fetch", "https://github.com/lab/gripper1"])
        .env("FFETCH_GITHUB_BASE", &server.base)
        .arg("--data-dir")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let run = parsed(&output);
    assert_eq!(run["items"].as_array().unwrap().len(), 2);
    let readme = &run["items"][1];
    assert_eq!(readme["text"], text);
    assert_eq!(readme["kind"], "repository_readme");
    assert_eq!(readme["provenance"]["locator"], "file:README.md");
    assert_eq!(readme["content"]["parent_id"], run["items"][0]["id"]);
    assert_ne!(
        readme["provenance"]["artifact"],
        run["items"][0]["provenance"]["artifact"]
    );
    let raw = cli(&[
        "artifact",
        readme["provenance"]["artifact"].as_str().unwrap(),
    ])
    .arg("--data-dir")
    .arg(dir.path())
    .output()
    .unwrap();
    let raw: Value = serde_json::from_slice(&raw.stdout).unwrap();
    assert_eq!(raw["encoding"], "base64");
    let original = cli(&[
        "artifact",
        readme["content"]["text_artifact"].as_str().unwrap(),
    ])
    .arg("--data-dir")
    .arg(dir.path())
    .output()
    .unwrap();
    assert_eq!(original.stdout, text.as_bytes());
    assert_eq!(run["meta"]["http"]["requests_started"], 2);
}

#[test]
fn github_thread_paginates_bounded_issue_comments_and_preserves_pull_identity() {
    let server = Fixture::new(|path| {
        if path.starts_with("/repos/lab/gripper1/issues/7/comments?") {
            let ids = if path.contains("page=2") {
                vec![100, 101, 102]
            } else {
                (1..=100).collect()
            };
            let comments = ids.into_iter().map(|id|json!({"id":id,"html_url":format!("https://github.com/lab/gripper1/pull/7#issuecomment-{id}"),"body":format!("Measurement {id}"),"user":{"login":"ada"}})).collect::<Vec<_>>();
            (200, "application/json", json!(comments).to_string())
        } else {
            (200, "application/json", github_issue().to_string())
        }
    });
    let output = cli(&[
        "thread",
        "https://github.com/lab/gripper1/pull/7",
        "--limit",
        "101",
        "--no-store",
    ])
    .env("FFETCH_GITHUB_BASE", &server.base)
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let run = parsed(&output);
    let items = run["items"].as_array().unwrap();
    assert_eq!(items.len(), 102);
    assert_eq!(items[0]["kind"], "pull_request");
    assert_eq!(items[101]["text"], "Measurement 101");
    assert_eq!(items[101]["content"]["parent_id"], items[0]["id"]);
    assert_eq!(run["meta"]["targets"], Value::Null);
    assert_eq!(run["meta"]["comments_returned"], 101);
    assert_eq!(run["meta"]["stop_reason"], "comment_limit");
    assert_eq!(run["meta"]["http"]["requests_started"], 3);
}

#[test]
fn github_read_rejects_mismatched_identity_and_keeps_metadata_when_readme_is_unavailable() {
    for mode in ["identity", "readme-absent", "bad-base64"] {
        let server = Fixture::new(move |path| {
            if path.contains("/readme") {
                if mode == "readme-absent" {
                    (
                        404,
                        "application/json",
                        "{\"message\":\"Not Found\"}".into(),
                    )
                } else {
                    let mut data = github_readme(1, "text");
                    data["content"] = json!("invalid base64!");
                    (200, "application/json", data.to_string())
                }
            } else {
                (
                    200,
                    "application/json",
                    github_repo(if mode == "identity" { 2 } else { 1 }).to_string(),
                )
            }
        });
        let output = cli(&["fetch", "https://github.com/lab/gripper1", "--no-store"])
            .env("FFETCH_GITHUB_BASE", &server.base)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(if mode == "identity" {
                1
            } else if mode == "readme-absent" {
                0
            } else {
                3
            })
        );
        let run = parsed(&output);
        assert_eq!(
            run["items"].as_array().unwrap().len(),
            if mode == "identity" { 0 } else { 1 }
        );
        if mode == "identity" {
            assert_eq!(run["errors"][0]["code"], "identity_mismatch");
        }
    }
}

#[test]
fn github_credentials_are_not_sent_to_overrides_and_code_search_requires_explicit_auth() {
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = requests.clone();
    let server = Fixture::new(move |_| {
        observed.fetch_add(1, Ordering::Relaxed);
        (200, "application/json", "{}".into())
    });
    let output = cli(&[
        "search",
        "gripper",
        "--sources",
        "github-code",
        "--no-store",
    ])
    .env("FFETCH_GITHUB_BASE", &server.base)
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        parsed(&output)["errors"][0]["code"],
        "authentication_required"
    );
    let output = cli(&["search", "gripper", "--sources", "github", "--no-store"])
        .env("FFETCH_GITHUB_BASE", &server.base)
        .env("FFETCH_GITHUB_TOKEN", "fixture-secret-do-not-persist")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        parsed(&output)["errors"][0]["code"],
        "credential_origin_rejected"
    );
    assert_eq!(requests.load(Ordering::Relaxed), 0);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-secret-do-not-persist"));
}

#[test]
fn github_api_failure_uses_ordinary_retrieval_without_discarding_the_native_error() {
    let api = Fixture::new(|_| {
        (
            503,
            "application/json",
            json!({"message":"API unavailable"}).to_string(),
        )
    });
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = reads.clone();
    let proxy = Fixture::new(move |path| {
        assert_eq!(path, "http://github.com/lab/gripper1");
        observed.fetch_add(1, Ordering::Relaxed);
        (
            200,
            "text/html",
            "<article>Public repository recovery evidence</article>".into(),
        )
    });
    let output = cli(&[
        "fetch",
        "http://github.com/lab/gripper1",
        "--archive",
        "off",
        "--no-store",
    ])
    .env("FFETCH_GITHUB_BASE", &api.base)
    .env("HTTP_PROXY", &proxy.base)
    .env("http_proxy", &proxy.base)
    .output()
    .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let run = parsed(&output);
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    assert!(
        run["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("recovery evidence")
    );
    assert_eq!(run["items"][0]["kind"], "document");
    assert_eq!(run["errors"][0]["source"], "github");
    assert!(
        run["meta"]["targets"][0]["selected_representation"]
            .as_str()
            .unwrap()
            .contains("fallback")
    );
}

#[test]
fn github_readme_requests_overlap_metadata_and_preserve_it_at_the_shared_deadline() {
    let readme_started = Arc::new(AtomicBool::new(false));
    let release_readme = Arc::new(AtomicBool::new(false));
    let observed = readme_started.clone();
    let release = release_readme.clone();
    let server = Fixture::new(move |path| {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        if path.ends_with("/readme") {
            observed.store(true, Ordering::Relaxed);
            while !release.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            (
                200,
                "application/json",
                github_readme(1, "late README").to_string(),
            )
        } else {
            while !observed.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            (200, "application/json", github_repo(1).to_string())
        }
    });
    let output = cli(&[
        "fetch",
        "https://github.com/lab/gripper1",
        "--timeout",
        "1",
        "--no-store",
    ])
    .env("FFETCH_GITHUB_BASE", &server.base)
    .output()
    .unwrap();
    release_readme.store(true, Ordering::Relaxed);
    assert!(readme_started.load(Ordering::Relaxed));
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let run = parsed(&output);
    assert_eq!(run["items"].as_array().unwrap().len(), 1);
    assert_eq!(run["items"][0]["kind"], "repository");
    assert_eq!(run["errors"][0]["code"], "deadline_exceeded");
    assert_eq!(run["meta"]["http"]["requests_started"], 2);
}

#[test]
fn github_search_streams_pages_into_deduplicated_native_readme_reads() {
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = reads.clone();
    let server = Fixture::new(move |path| {
        if path.starts_with("/search/repositories?") {
            let parsed_url = url::Url::parse(&format!("http://fixture{path}")).unwrap();
            let second = parsed_url
                .query_pairs()
                .any(|(key, value)| key == "page" && value == "2");
            if second {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while observed.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < deadline
                {
                    thread::sleep(Duration::from_millis(2));
                }
                if observed.load(Ordering::Relaxed) == 0 {
                    return (
                        503,
                        "application/json",
                        "{\"message\":\"README reads waited for search completion\"}".into(),
                    );
                }
            }
            (200,"application/json",json!({"total_count":3,"incomplete_results":false,"items":if second {vec![github_repo(2)]}else{vec![github_repo(1),github_repo(1)]}}).to_string())
        } else if path.contains("/readme") {
            observed.fetch_add(1, Ordering::Relaxed);
            (
                200,
                "application/json",
                github_readme(
                    if path.contains("gripper2") { 2 } else { 1 },
                    "Gripper CAD body",
                )
                .to_string(),
            )
        } else {
            (
                200,
                "application/json",
                github_repo(if path.contains("gripper2") { 2 } else { 1 }).to_string(),
            )
        }
    });
    let output = cli(&[
        "search",
        "gripper",
        "--query",
        "tendon",
        "--sources",
        "github",
        "--limit",
        "2",
        "--read",
        "--format",
        "jsonl",
        "--no-store",
    ])
    .env("FFETCH_GITHUB_BASE", &server.base)
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(reads.load(Ordering::Relaxed), 2);
    let events = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let records = events
        .iter()
        .filter(|e| e["type"] == "record")
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 6);
    let ids = records
        .iter()
        .map(|e| e["item"]["id"].as_str().unwrap())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(ids.len(), records.len());
    assert_eq!(
        records
            .iter()
            .filter(|e| e["item"]["kind"] == "repository_readme")
            .count(),
        2
    );
    assert!(
        records
            .iter()
            .filter(|e| e["item"]["kind"] == "repository_readme")
            .all(|e| e["item"]["native"]["discovered_from"].is_string())
    );
}

fn pdf_fixture(texts: &[&str], compress: bool) -> Vec<u8> {
    use lopdf::{
        Document, Object, Stream,
        content::{Content, Operation},
        dictionary,
    };
    let mut doc = Document::with_version("1.5");
    let pages = doc.new_object_id();
    let font =
        doc.add_object(dictionary! {"Type"=>"Font","Subtype"=>"Type1","BaseFont"=>"Helvetica"});
    let resources = doc.add_object(dictionary! {"Font"=>dictionary!{"F1"=>font}});
    let mut kids = Vec::new();
    for &text in texts {
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![10.into(), 30.into()]),
                Operation::new("Tj", vec![Object::string_literal(text)]),
                Operation::new("ET", vec![]),
            ],
        };
        let stream = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page = doc.add_object(dictionary! {"Type"=>"Page","Parent"=>pages,"Contents"=>stream});
        kids.push(page.into());
    }
    doc.objects.insert(pages,dictionary!{"Type"=>"Pages","Kids"=>kids,"Count"=>texts.len() as i64,"Resources"=>resources,"MediaBox"=>vec![0.into(),0.into(),200.into(),200.into()]}.into());
    let catalog = doc.add_object(dictionary! {"Type"=>"Catalog","Pages"=>pages});
    doc.trailer.set("Root", catalog);
    if compress {
        doc.compress();
    }
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn fetch_reads_suffixless_pdfs_once_and_keeps_page_evidence() {
    let bytes = pdf_fixture(&["Torque 4 N m", "Mass 200 g"], false);
    let original = bytes.clone();
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = requests.clone();
    let server = Fixture::bytes(move |path| {
        observed.fetch_add(1, Ordering::Relaxed);
        // A misleading HTML MIME type must not cause HTML challenge parsing of PDF bytes.
        (
            200,
            if path == "/wrong-mime" {
                "text/html"
            } else {
                "application/octet-stream"
            },
            bytes.clone(),
        )
    });
    let dir = tempfile::tempdir().unwrap();
    for (index, path) in ["download?id=42", "wrong-mime"].into_iter().enumerate() {
        let url = server.url(path);
        let output = cli(&["fetch", &url, "--archive", "off"])
            .arg("--data-dir")
            .arg(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let run = parsed(&output);
        assert_eq!(requests.load(Ordering::Relaxed), index + 1);
        assert_eq!(run["meta"]["http"]["requests_started"], 1);
        assert_eq!(run["items"].as_array().unwrap().len(), 2);
        assert!(
            run["items"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Torque 4 N m")
        );
        let page = &run["items"][1];
        assert_eq!(page["kind"], "pdf_page");
        assert_eq!(page["url"], format!("{url}#page=2"));
        assert_eq!(page["provenance"]["locator"], "page:2");
        assert_eq!(page["provenance"]["source_url"], url);
        assert_eq!(page["provenance"]["method"], "lopdf-text-layer");
        let meta = &run["meta"]["targets"][0];
        assert_eq!(meta["selected_representation"], "pdf-text-layer");
        assert_eq!(meta["pdf"]["total_pages"], 2);
        assert_eq!(meta["artifact"], page["provenance"]["artifact"]);
        let artifact = cli(&["artifact", meta["artifact"].as_str().unwrap()])
            .arg("--data-dir")
            .arg(dir.path())
            .output()
            .unwrap();
        assert_eq!(artifact.stdout, original);
    }
}

#[test]
fn automatic_pdf_selection_is_bounded_and_errors_remain_visible() {
    let many = pdf_fixture(&["Evidence"; 21], false);
    let blank = pdf_fixture(&["Usable text", ""], false);
    let server = Fixture::bytes(move |path| {
        (
            200,
            "application/pdf",
            match path {
                "/many" => many.clone(),
                "/blank" => blank.clone(),
                _ => b"%PDF-1.7\ninvalid".to_vec(),
            },
        )
    });
    let output = cli(&["fetch", &server.url("many"), "--no-store"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let run = parsed(&output);
    assert_eq!(run["items"].as_array().unwrap().len(), 20);
    assert_eq!(
        run["meta"]["targets"][0]["pdf"]["pages_outside_selection"],
        1
    );
    assert!(run["warnings"].as_array().unwrap().iter().any(|w| {
        w.as_str()
            .unwrap()
            .contains("outside the returned selection")
    }));
    let output = cli(&["fetch", &server.url("blank"), "--no-store"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let run = parsed(&output);
    assert_eq!(run["errors"][0]["code"], "pdf_page_text_unavailable");
    assert_eq!(run["items"][1]["content"]["text_available"], false);
    let output = cli(&["fetch", &server.url("malformed"), "--no-store"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let run = parsed(&output);
    assert!(run["items"].as_array().unwrap().is_empty());
    assert_eq!(run["errors"][0]["code"], "pdf_parse_failed");
}

#[test]
fn automatic_pdf_rejects_unsupported_selectors_and_does_not_trust_url_suffixes() {
    let pdf = pdf_fixture(&["Evidence"], false);
    let server = Fixture::bytes(move |path| {
        if path == "/fake.pdf" {
            (
                200,
                "text/html",
                b"<html><article>Actual HTML document</article></html>".to_vec(),
            )
        } else if path == "/challenge" {
            (
                200,
                "application/pdf",
                b"<html><title>captcha</title><body>Challenge</body></html>".to_vec(),
            )
        } else {
            (200, "application/pdf", pdf.clone())
        }
    });
    for options in [vec!["--extract", "tables"], vec!["--css", "article"]] {
        let output = cli(&["fetch", &server.url("real"), "--no-store"])
            .args(options)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(
            parsed(&output)["errors"][0]["message"]
                .as_str()
                .unwrap()
                .contains("PDF supports")
        );
    }
    let output = cli(&["fetch", &server.url("fake.pdf"), "--no-store"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let run = parsed(&output);
    assert_eq!(run["items"][0]["kind"], "document");
    assert!(
        run["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Actual HTML document")
    );
    let output = cli(&[
        "fetch",
        &server.url("challenge"),
        "--archive",
        "off",
        "--no-store",
    ])
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let run = parsed(&output);
    assert_eq!(run["errors"][0]["code"], "retrieval_failed");
    assert_eq!(run["attempts"][0]["outcome"], "challenge");
}

#[test]
fn archive_pdf_skips_bad_captures_and_preserves_original_page_urls() {
    let pdf = pdf_fixture(&["Historical torque evidence"], false);
    let blank = pdf_fixture(&[""], false);
    let server = Fixture::bytes(move |path| {
        if path.starts_with("/cdx?") {
            (
                200,
                "application/json",
                json!([
                    ["timestamp", "original"],
                    ["20261003000000", "https://example.org/paper"],
                    ["20261001000000", "https://example.org/paper"]
                ])
                .to_string()
                .into_bytes(),
            )
        } else if path.starts_with("/cdxblank?") {
            (
                200,
                "application/json",
                json!([
                    ["timestamp", "original"],
                    ["20261002000000", "https://example.org/paper"],
                    ["20261001000000", "https://example.org/paper"]
                ])
                .to_string()
                .into_bytes(),
            )
        } else {
            (
                200,
                "application/pdf",
                if path.starts_with("/web/20261003") {
                    b"%PDF-1.7\ninvalid".to_vec()
                } else if path.starts_with("/web/20261002") {
                    blank.clone()
                } else {
                    pdf.clone()
                },
            )
        }
    });
    let output = cli(&[
        "fetch",
        "https://example.org/paper",
        "--archive",
        "latest-valid",
        "--no-store",
    ])
    .env("FFETCH_WAYBACK_CDX", server.url("cdx"))
    .env("FFETCH_WAYBACK_REPLAY", &server.base)
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = parsed(&output);
    let page = &run["items"][0];
    assert_eq!(page["url"], "https://example.org/paper#page=1");
    assert_eq!(page["source"], "wayback");
    assert_eq!(page["provenance"]["locator"], "page:1");
    assert_eq!(page["provenance"]["method"], "lopdf-text-layer");
    assert_eq!(
        page["provenance"]["source_url"],
        "https://example.org/paper"
    );
    assert_eq!(
        page["provenance"]["captured_at"],
        "2026-10-01T00:00:00+00:00"
    );
    assert!(
        page["provenance"]["archive_url"]
            .as_str()
            .unwrap()
            .contains("20261001000000id_")
    );
    assert_eq!(
        run["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["outcome"]
                .as_str()
                .unwrap()
                .starts_with("unusable_capture:"))
            .count(),
        1
    );
    assert!(run["errors"].as_array().unwrap().is_empty());
    // A structurally valid PDF without a text layer still exists. Keep this
    // capture with partial status rather than silently substituting older text.
    let output = cli(&[
        "fetch",
        "https://example.org/paper",
        "--archive",
        "latest-valid",
        "--no-store",
    ])
    .env("FFETCH_WAYBACK_CDX", server.url("cdxblank"))
    .env("FFETCH_WAYBACK_REPLAY", &server.base)
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let run = parsed(&output);
    assert_eq!(
        run["items"][0]["provenance"]["captured_at"],
        "2026-10-02T00:00:00+00:00"
    );
    assert_eq!(run["items"][0]["content"]["text_available"], false);
    assert_eq!(run["errors"][0]["code"], "pdf_page_text_unavailable");
}

#[test]
fn search_read_extracts_discovered_pdf_once_across_query_variants() {
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = reads.clone();
    let pdf = pdf_fixture(&["Gripper CAD measurements", "Tendon force 4 N"], false);
    let document = Fixture::bytes(move |_| {
        observed.fetch_add(1, Ordering::Relaxed);
        (200, "application/octet-stream", pdf.clone())
    });
    let link = document
        .url("download?id=42")
        .replace("127.0.0.1", "localhost");
    let server = Fixture::new(move |_| {
        (
            200,
            "text/html",
            format!(
                "<li class='b_algo'><h2><a href='{link}'>Gripper paper</a></h2><div class='b_caption'><p>Research</p></div></li>"
            ),
        )
    });
    let output = cli(&[
        "search",
        "gripper",
        "--query",
        "tendon",
        "--sources",
        "web",
        "--engines",
        "bing",
        "--search-mode",
        "balanced",
        "--read",
        "--no-store",
    ])
    .env("FFETCH_BING_SEARCH", server.url("bing"))
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = parsed(&output);
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    let items = run["items"].as_array().unwrap();
    let discovery = items.iter().find(|r| r["kind"] != "pdf_page").unwrap();
    let pages = items
        .iter()
        .filter(|r| r["kind"] == "pdf_page")
        .collect::<Vec<_>>();
    assert_eq!(pages.len(), 2);
    for page in pages {
        assert_eq!(page["native"]["discovered_from"], discovery["id"]);
    }
    assert_eq!(run["meta"]["read_coverage"].as_array().unwrap().len(), 1);
    assert_eq!(
        run["meta"]["read_coverage"][0]["meta"]["selected_representation"],
        "pdf-text-layer"
    );
    assert_eq!(run["meta"]["http"]["requests_started"], 3);
}

#[test]
fn arxiv_search_read_follows_versioned_pdf_without_repeating_metadata_lookup() {
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = requests.clone();
    let pdf = pdf_fixture(&["Version two research body"], false);
    let server = Fixture::bytes(move |path| {
        observed.fetch_add(1, Ordering::Relaxed);
        if path.starts_with("/query?") {
            assert!(!path.contains("id_list"), "unnecessary metadata lookup");
            (
                200,
                "application/atom+xml",
                arxiv_feed(&arxiv_entry("2501.12345v2"), 1).into_bytes(),
            )
        } else {
            assert_eq!(path, "/pdf/2501.12345v2");
            (200, "application/pdf", pdf.clone())
        }
    });
    let output = cli(&[
        "search",
        "gripper",
        "--sources",
        "arxiv",
        "--read",
        "--no-store",
    ])
    .env("FFETCH_ARXIV_SEARCH", server.url("query"))
    .env("FFETCH_ARXIV_PDF_BASE", server.url("pdf/"))
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = parsed(&output);
    assert_eq!(requests.load(Ordering::Relaxed), 2);
    assert_eq!(run["items"].as_array().unwrap().len(), 2);
    let metadata = run["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "paper")
        .unwrap();
    let body = run["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "pdf_page")
        .unwrap();
    assert!(
        body["text"]
            .as_str()
            .unwrap()
            .contains("Version two research body")
    );
    assert_eq!(body["native"]["discovered_from"], metadata["id"]);
    assert_eq!(
        run["meta"]["read_coverage"][0]["meta"]["arxiv_id"],
        "2501.12345v2"
    );
}

#[test]
fn pdf_decompressed_page_limit_rejects_small_compressed_bombs() {
    let large = "A".repeat(17 * 1024 * 1024);
    let bytes = pdf_fixture(&[&large], true);
    assert!(bytes.len() < 100_000);
    let server = Fixture::bytes(move |_| (200, "application/pdf", bytes.clone()));
    let output = cli(&["pdf", &server.url("bomb"), "--no-store"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let run = parsed(&output);
    assert_eq!(run["errors"][0]["code"], "pdf_page_text_unavailable");
    assert_eq!(run["items"][0]["content"]["text_available"], false);
}

#[test]
fn pdf_selected_pages_keep_text_locators_and_original_artifacts() {
    let bytes = pdf_fixture(&["Torque 4 N m", "Mass 200 g", ""], false);
    let expected = bytes.clone();
    let server = Fixture::bytes(move |_| (200, "application/pdf", bytes.clone()));
    let dir = tempfile::tempdir().unwrap();
    let output = cli(&["pdf", &server.url("paper"), "--limit", "2"])
        .arg("--data-dir")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = parsed(&output);
    assert_eq!(run["items"].as_array().unwrap().len(), 2);
    assert!(
        run["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Torque 4 N m")
    );
    assert!(
        run["items"][1]["text"]
            .as_str()
            .unwrap()
            .contains("Mass 200 g")
    );
    assert_eq!(run["items"][1]["provenance"]["locator"], "page:2");
    assert_eq!(run["meta"]["pages_outside_selection"], 1);
    let original = cli(&["artifact", run["meta"]["artifact"].as_str().unwrap()])
        .arg("--data-dir")
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(original.stdout, expected);
    let output = cli(&[
        "pdf",
        &server.url("paper"),
        "--page",
        "3",
        "--limit",
        "1",
        "--no-store",
    ])
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let run = parsed(&output);
    assert_eq!(run["items"][0]["content"]["text_available"], false);
    assert_eq!(run["errors"][0]["code"], "pdf_page_text_unavailable");
    let output = cli(&["pdf", &server.url("paper"), "--page", "4", "--no-store"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn pdf_rejects_html_and_malformed_documents_without_false_success() {
    let server = Fixture::new(|path| {
        (
            200,
            "application/pdf",
            if path == "/html" {
                "<html>Access denied</html>".into()
            } else {
                "%PDF-1.7\ninvalid".into()
            },
        )
    });
    for path in ["html", "bad"] {
        let output = cli(&["pdf", &server.url(path), "--no-store"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(parsed(&output)["items"].as_array().unwrap().is_empty());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn pdf_deadline_terminates_workers_for_explicit_and_automatic_reads() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("worker");
    let pidfile = dir.path().join("pid");
    let quoted = format!("'{}'", pidfile.to_string_lossy().replace('\'', "'\\''"));
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf '%s' \"$$\" > {quoted}\nwhile :; do :; done\n"),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = Fixture::new(|_| (200, "application/pdf", "%PDF-1.7\n".into()));
    let http = ffetch::net::Http::new(Duration::from_secs(2), 4096, 1).unwrap();
    let run = ffetch::pdf::fetch(
        &http,
        &server.url("pdf"),
        1,
        1,
        None,
        Duration::from_millis(200),
        &script,
    )
    .await;
    assert_eq!(run.errors[0].code, "deadline_exceeded");
    let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().parse().unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    let store = ffetch::store::Store::new(dir.path().join("evidence"));
    let mut fetcher = ffetch::fetch::Fetcher {
        http,
        browser: None,
        browser_mode: ffetch::browser::Mode::Off,
        store: Some(store.clone()),
        budget: Duration::from_millis(200),
        cdx: url::Url::parse(&server.url("cdx")).unwrap(),
        replay: url::Url::parse(&server.base).unwrap(),
        archive_limit: 1,
        pdf_worker: Some(script),
    };
    let run = fetcher
        .fetch(
            &server.url("pdf"),
            &ffetch::extract::Options::default(),
            ffetch::fetch::Archive::Off,
        )
        .await;
    assert_eq!(run.errors[0].code, "deadline_exceeded");
    assert_eq!(run.meta["selected_representation"], "pdf-text-layer");
    assert_eq!(
        store
            .read_blob(run.meta["artifact"].as_str().unwrap())
            .unwrap(),
        b"%PDF-1.7\n"
    );
    let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().parse().unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    // A library consumer must explicitly configure ffetch, never spawn its own host.
    fetcher.pdf_worker = None;
    let run = fetcher
        .fetch(
            &server.url("pdf"),
            &ffetch::extract::Options::default(),
            ffetch::fetch::Archive::Off,
        )
        .await;
    assert_eq!(run.errors[0].code, "extraction_error");
    assert!(
        run.errors[0]
            .message
            .contains("PDF worker is not configured")
    );
}

#[test]
fn search_read_uses_pmc_fulltext_once_across_query_variants() {
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = reads.clone();
    let server = Fixture::new(move |path| {
        if path.starts_with("/rest/") {
            counted.fetch_add(1, Ordering::Relaxed);
            (200,"application/xml","<article><front><article-meta><article-id pub-id-type='pmcid'>PMC123</article-id><article-id pub-id-type='doi'>10.1000/article</article-id><title-group><article-title>Research body</article-title></title-group></article-meta></front><body><sec><p>Evidence beyond the abstract.</p></sec></body></article>".into())
        } else {
            assert!(path.starts_with("/search"));
            (200,"application/json",json!({"hitCount":1,"resultList":{"result":[{"id":"1","source":"MED","doi":"10.1000/article","pmcid":"PMC123","title":"Metadata title","abstractText":"Abstract only"}]}}).to_string())
        }
    });
    let output = cli(&[
        "search",
        "gripper",
        "--query",
        "tendon",
        "--sources",
        "europepmc",
        "--limit",
        "1",
        "--read",
        "--no-store",
    ])
    .env("FFETCH_EUROPEPMC_SEARCH", server.url("search"))
    .env("FFETCH_EUROPEPMC_FULLTEXT", server.url("rest"))
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = parsed(&output);
    let items = run["items"].as_array().unwrap();
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    assert_eq!(items.len(), 2);
    let body = items
        .iter()
        .find(|r| r["content"]["full_text_retrieved"] == true)
        .unwrap();
    let metadata = items.iter().find(|r| r["source"] == "europepmc").unwrap();
    assert!(
        body["text"]
            .as_str()
            .unwrap()
            .contains("beyond the abstract")
    );
    assert_eq!(body["native"]["discovered_from"], metadata["id"]);
    assert_eq!(run["meta"]["read_coverage"].as_array().unwrap().len(), 1);
    assert_eq!(
        run["meta"]["read_coverage"][0]["meta"]["selected_representation"],
        "europepmc-jats"
    );
    assert_eq!(run["meta"]["http"]["requests_started"], 3);
}

#[test]
fn fulltext_preserves_jats_body_tables_math_and_original_xml() {
    let xml = "<?xml version='1.0' encoding='UTF-8'?><!DOCTYPE article SYSTEM 'https://invalid.example/article.dtd'><article xmlns:m='http://www.w3.org/1998/Math/MathML' article-type='research-article'><front><article-meta><article-id pub-id-type='pmcid'>PMC123</article-id><article-id pub-id-type='doi'>10.1000/grip</article-id><title-group><article-title>Robot <italic>gripper</italic></article-title></title-group><abstract><p>Abstract only</p></abstract></article-meta></front><body><sec id='s1'><title>Methods</title><p id='p1'>Soft <bold>gripper</bold> &amp; control.</p><table-wrap id='t1'><table><tr><th>Torque</th></tr><tr><td colspan='2'>4 N m</td></tr></table></table-wrap><disp-formula><m:math><m:mi>x</m:mi></m:math></disp-formula></sec></body><back><ref-list><ref id='r1'><mixed-citation>Source reference</mixed-citation></ref></ref-list></back></article>";
    let server = Fixture::new(move |_| (200, "application/xml", xml.into()));
    let dir = tempfile::tempdir().unwrap();
    let output = cli(&["fulltext", "https://pmc.ncbi.nlm.nih.gov/articles/PMC123/"])
        .arg("--data-dir")
        .arg(dir.path())
        .env("FFETCH_EUROPEPMC_FULLTEXT", server.url("rest"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = parsed(&output);
    let record = &run["items"][0];
    assert_eq!(record["title"], "Robot gripper");
    assert!(
        record["text"]
            .as_str()
            .unwrap()
            .contains("Soft gripper & control.")
    );
    assert!(!record["text"].as_str().unwrap().contains("Abstract only"));
    assert_eq!(
        record["content"]["tables"][0]["rows"][1][0]["text"],
        "4 N m"
    );
    assert_eq!(record["content"]["tables"][0]["rows"][1][0]["colspan"], "2");
    assert_eq!(record["content"]["references"][0]["id"], "r1");
    assert_eq!(record["content"]["math_present"], true);
    assert_eq!(record["content"]["external_entities_resolved"], false);
    assert!(!run["warnings"].as_array().unwrap().is_empty());
    let saved = cli(&[
        "artifact",
        record["provenance"]["artifact"].as_str().unwrap(),
    ])
    .arg("--data-dir")
    .arg(dir.path())
    .output()
    .unwrap();
    assert_eq!(saved.stdout, xml.as_bytes());
}

#[test]
fn fulltext_rejects_mismatched_identity_dtd_entities_and_non_article_responses() {
    let server = Fixture::new(|path| {
        let xml = if path.contains("PMC124/") {
            "<article><front><article-meta><article-id pub-id-type='pmc'>123</article-id></article-meta></front><body><p>Wrong paper</p></body></article>"
        } else if path.contains("PMC125/") {
            "<!DOCTYPE article [<!ENTITY steal SYSTEM 'file:///etc/passwd'>]><article>&steal;</article>"
        } else if path.contains("PMC126/") {
            "<article><front>"
        } else {
            "<html><body>Access denied</body></html>"
        };
        (200, "application/xml", xml.into())
    });
    for id in ["PMC124", "PMC125", "PMC126", "PMC127"] {
        let output = cli(&["fulltext", id, "--no-store"])
            .env("FFETCH_EUROPEPMC_FULLTEXT", server.url("rest"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let run = parsed(&output);
        assert!(run["items"].as_array().unwrap().is_empty());
        assert_eq!(run["errors"][0]["code"], "fulltext_unavailable");
    }
    for input in [
        "123",
        "https://evil.example/articles/PMC123/",
        "https://user:secret@europepmc.org/articles/PMC123/",
    ] {
        let output = cli(&["fulltext", input, "--no-store"]).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
}

#[test]
fn references_traverse_reported_dois_without_repeating_cycles_or_inventing_links() {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = requests.clone();
    let server = Fixture::new(move |path| {
        observed.lock().unwrap().push(path.to_owned());
        let decoded = path.replace("%2F", "/").replace("%2f", "/");
        let id = decoded.strip_prefix("/works/").unwrap();
        let refs = match id {
            "10.1000/root" => {
                json!([{"DOI":"10.1000/a"},{"DOI":"10.1000/a"},{"unstructured":"Unresolved historical source"},{"DOI":"not a doi"},{"DOI":"10.1000/b"}])
            }
            "10.1000/a" => json!([{"DOI":"10.1000/root"},{"DOI":"10.1000/leaf"}]),
            "10.1000/b" => json!([{"DOI":"10.1000/leaf"}]),
            "10.1000/leaf" => json!([]),
            _ => panic!("unexpected lookup {id}"),
        };
        (200,"application/json",json!({"status":"ok","message-type":"work","message":{"DOI":id,"title":[id],"type":"journal-article","reference":refs,"reference-count":refs.as_array().unwrap().len()}}).to_string())
    });
    let output = cli(&[
        "references",
        "https://doi.org/10.1000/ROOT",
        "--depth",
        "3",
        "--limit",
        "20",
        "--no-store",
    ])
    .env("FFETCH_CROSSREF_WORKS", server.url("works"))
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = parsed(&output);
    assert_eq!(run["items"].as_array().unwrap().len(), 4);
    assert_eq!(requests.lock().unwrap().len(), 4);
    assert_eq!(run["meta"]["edges"].as_array().unwrap().len(), 8);
    assert_eq!(
        run["meta"]["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["lookup_status"] == "unresolved_doi")
            .count(),
        2
    );
    assert_eq!(
        run["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["content"]["doi"] == "10.1000/leaf")
            .unwrap()["native"]["citation_depth"],
        2
    );
    let output = cli(&[
        "references",
        "10.1000/root",
        "--depth",
        "1",
        "--limit",
        "1",
        "--no-store",
    ])
    .env("FFETCH_CROSSREF_WORKS", server.url("works"))
    .output()
    .unwrap();
    assert!(output.status.success());
    let run = parsed(&output);
    assert_eq!(run["items"].as_array().unwrap().len(), 2);
    assert_eq!(run["meta"]["references_inspected"], 1);
    assert_eq!(run["meta"]["reference_budget_exhausted"], true);
    assert_eq!(
        run["items"][0]["content"]["references"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    assert_eq!(requests.lock().unwrap().len(), 6);
}

#[test]
fn references_retain_completed_work_and_distinguish_failed_pending_and_unknown_coverage() {
    let server = Fixture::new(|path| {
        let decoded = path.replace("%2F", "/").replace("%2f", "/");
        let id = decoded.strip_prefix("/works/").unwrap();
        if id == "10.1000/missing" {
            return (404, "text/plain", "missing".into());
        }
        if id == "10.1000/slow" {
            thread::sleep(Duration::from_millis(1500));
        }
        let actual = if id == "10.1000/mismatch" {
            "10.1000/unrelated"
        } else {
            id
        };
        let mut data = json!({"DOI":actual,"type":"journal-article","title":[actual]});
        if id == "10.1000/root" {
            data["reference"] = json!([{"DOI":"10.1000/good"},{"DOI":"10.1000/missing"},{"DOI":"10.1000/mismatch"},{"DOI":"10.1000/slow"}]);
        }
        (
            200,
            "application/json",
            json!({"status":"ok","message-type":"work","message":data}).to_string(),
        )
    });
    let output = cli(&[
        "references",
        "10.1000/root",
        "--depth",
        "2",
        "--timeout",
        "1",
        "--no-store",
    ])
    .env("FFETCH_CROSSREF_WORKS", server.url("works"))
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let run = parsed(&output);
    assert_eq!(run["meta"]["fetched_works"], 2);
    assert_eq!(run["meta"]["failed_works"], 2);
    assert_eq!(run["meta"]["pending_works"], 1);
    assert_eq!(run["meta"]["unavailable_reference_lists"], 1);
    assert!(
        run["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["message"].as_str().unwrap().contains("does not match"))
    );
    assert!(
        run["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["code"] == "deadline_exceeded")
    );
    let directory = tempfile::tempdir().unwrap();
    let absent = directory.path().join("absent");
    let output = cli(&["references", "not-a-doi"])
        .arg("--data-dir")
        .arg(&absent)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!absent.exists());
}

#[test]
fn multi_query_plan_deduplicates_and_rejects_over_budget_before_io() {
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = requests.clone();
    let server = Fixture::new(move |_| {
        observed.fetch_add(1, Ordering::Relaxed);
        (200, "text/html", String::new())
    });
    let directory = tempfile::tempdir().unwrap();
    let absent = directory.path().join("no-data");
    let output = cli(&[
        "search",
        "gripper",
        "--query",
        " gripper ",
        "--query",
        "tendon",
        "--sources",
        "web",
        "--engines",
        "bing,brave",
        "--search-mode",
        "balanced",
        "--max-searches",
        "4",
        "--dry-run",
        "--read",
    ])
    .arg("--data-dir")
    .arg(&absent)
    .env("FFETCH_BING_SEARCH", server.url("bing"))
    .env("FFETCH_BRAVE_SEARCH", server.url("brave"))
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan = parsed(&output);
    assert_eq!(plan["operation"], "search_plan");
    assert_eq!(plan["queries"], json!(["gripper", "tendon"]));
    assert_eq!(plan["jobs"].as_array().unwrap().len(), 4);
    for (primary, extra, budget) in [
        ("gripper", "tendon", "1"),
        (" ", "tendon", "32"),
        ("gripper", " ", "32"),
    ] {
        let output = cli(&[
            "search",
            primary,
            "--query",
            extra,
            "--sources",
            "web",
            "--engines",
            "bing",
            "--search-mode",
            "balanced",
            "--max-searches",
            budget,
        ])
        .arg("--data-dir")
        .arg(&absent)
        .env("FFETCH_BING_SEARCH", server.url("bing"))
        .output()
        .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
    assert_eq!(requests.load(Ordering::Relaxed), 0);
    assert!(!absent.exists());
}

#[test]
fn multi_query_web_stream_fuses_origins_and_reads_each_url_once() {
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = reads.clone();
    let article = Fixture::new(move |_| {
        counted.fetch_add(1, Ordering::Relaxed);
        (
            200,
            "text/html",
            "<article>Open gripper CAD and tendon design</article>".into(),
        )
    });
    let link = article.url("paper").replace("127.0.0.1", "localhost");
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let started = requests.clone();
    let server = Fixture::new(move |path| {
        started.fetch_add(1, Ordering::Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while started.load(Ordering::Relaxed) < 2 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            started.load(Ordering::Relaxed) >= 2,
            "query jobs were serialized"
        );
        let url = url::Url::parse(&format!("http://fixture{path}")).unwrap();
        let query = url
            .query_pairs()
            .find(|(k, _)| k == "q")
            .unwrap()
            .1
            .to_string();
        assert!(matches!(query.as_str(), "gripper" | "tendon"));
        (
            200,
            "text/html",
            format!(
                "<li class='b_algo'><h2><a href='{link}'>Gripper design</a></h2><div class='b_caption'><p>{query} evidence</p></div></li>"
            ),
        )
    });
    let output = cli(&[
        "search",
        "gripper",
        "--query",
        "tendon",
        "--sources",
        "web",
        "--engines",
        "bing",
        "--search-mode",
        "balanced",
        "--concurrency",
        "2",
        "--limit",
        "1",
        "--read",
        "--format",
        "jsonl",
        "--no-store",
    ])
    .env("FFETCH_BING_SEARCH", server.url("bing"))
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.iter().filter(|e| e["type"] == "record").count(), 2);
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == "record_update")
            .count(),
        1
    );
    let updated = &events
        .iter()
        .find(|e| e["type"] == "record_update")
        .unwrap()["item"];
    assert_eq!(updated["metrics"]["engine_count"], 1);
    assert_eq!(updated["metrics"]["query_count"], 2);
    assert_eq!(updated["metrics"]["observation_count"], 2);
    assert_eq!(
        updated["native"]["discovery_queries"],
        json!(["gripper", "tendon"])
    );
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    assert_eq!(requests.load(Ordering::Relaxed), 2);
    assert_eq!(
        events.last().unwrap()["result"]["meta"]["coverage"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let doc = &events
        .iter()
        .find(|e| e["item"]["kind"] == "document")
        .unwrap()["item"];
    assert_eq!(doc["native"]["discovered_from"], updated["id"]);
}

#[test]
fn multi_query_native_results_keep_hn_and_doi_search_observations() {
    let server = Fixture::new(|path| {
        let second = path.contains("tendon");
        let data = if path.starts_with("/hn") {
            json!({"hits":[{"objectID":"1","title":"Gripper design","points":if second {20}else{10}}],"nbPages":1,"nbHits":1})
        } else {
            json!({"message":{"items":[{"DOI":"10.1000/grip","title":["Gripper design"],"type":"journal-article","abstract":if second {"Tendon evidence"}else{"Gripper evidence"}}],"total-results":1,"next-cursor":"done"}})
        };
        (200, "application/json", data.to_string())
    });
    let output = cli(&[
        "search",
        "gripper",
        "--query",
        "tendon",
        "--sources",
        "hn,crossref",
        "--limit",
        "1",
        "--no-store",
    ])
    .env("FFETCH_HN_SEARCH", server.url("hn"))
    .env("FFETCH_CROSSREF_SEARCH", server.url("crossref"))
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = parsed(&output);
    let items = run["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    for item in items {
        assert_eq!(
            item["native"]["discovery_queries"],
            json!(["gripper", "tendon"])
        );
    }
    let hn = items.iter().find(|r| r["source"] == "hn").unwrap();
    assert_eq!(hn["id"], "hn:1");
    let mut scores: Vec<_> = hn["native"]["search_observations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["metrics"]["score"].as_u64().unwrap())
        .collect();
    scores.sort_unstable();
    assert_eq!(scores, [10, 20]);
    let paper = items.iter().find(|r| r["source"] == "crossref").unwrap();
    assert_eq!(paper["metrics"]["metadata_source_count"], 1);
    assert_eq!(
        paper["native"]["research_origins"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        run["meta"]["query_plan"]["jobs"].as_array().unwrap().len(),
        4
    );
}

#[test]
fn multi_query_failure_names_the_missing_query_and_keeps_successes() {
    let server = Fixture::new(|path| {
        if path.contains("q=unavailable") {
            (503, "text/html", "Busy".into())
        } else {
            (
                200,
                "text/html",
                "<li class='b_algo'><h2><a href='https://example.org/gripper'>Design</a></h2></li>"
                    .into(),
            )
        }
    });
    let output = cli(&[
        "search",
        "gripper",
        "--query",
        "unavailable",
        "--sources",
        "web",
        "--engines",
        "bing",
        "--search-mode",
        "balanced",
        "--no-store",
    ])
    .env("FFETCH_BING_SEARCH", server.url("bing"))
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let run = parsed(&output);
    assert_eq!(run["items"].as_array().unwrap().len(), 1);
    let missing = run["meta"]["coverage"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["status"] == "error")
        .unwrap();
    assert_eq!(missing["meta"]["query"], "unavailable");
    assert_eq!(missing["errors"][0]["code"], "service_unavailable");
}

#[test]
#[ignore = "controlled latency benchmark; set FFETCH_BENCH_BIN to a release binary"]
fn benchmark_paginated_first_record_latency() {
    use std::io::BufRead;
    let binary = std::env::var("FFETCH_BENCH_BIN").expect("set FFETCH_BENCH_BIN");
    let server = Fixture::new(|path| {
        let later = path.contains("first=");
        if later {
            thread::sleep(Duration::from_millis(250));
        }
        let link = if later { "second" } else { "first" };
        let mut html = format!(
            "<li class='b_algo'><h2><a href='https://example.org/{link}'>{link}</a></h2></li>"
        );
        if !later {
            html += "<a class='sb_pagN' href='/bing?q=gripper&amp;first=11'>Next</a>";
        }
        (200, "text/html", html)
    });
    let mut samples = Vec::new();
    for _ in 0..5 {
        let start = std::time::Instant::now();
        let mut child = Command::new(&binary)
            .args([
                "search",
                "gripper",
                "--sources",
                "web",
                "--engines",
                "bing",
                "--search-pages",
                "2",
                "--limit",
                "10",
                "--browser",
                "off",
                "--format",
                "jsonl",
                "--no-store",
            ])
            .env("FFETCH_BING_SEARCH", server.url("bing"))
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut first = None;
        let mut count = 0;
        let mut complete = false;
        for line in std::io::BufReader::new(child.stdout.take().unwrap()).lines() {
            let event: Value = serde_json::from_str(&line.unwrap()).unwrap();
            if event["type"] == "record" {
                first.get_or_insert_with(|| start.elapsed().as_secs_f64() * 1000.0);
                count += 1;
            }
            if event["type"] == "complete" {
                assert_eq!(event["result"]["status"], "ok");
                complete = true;
            }
        }
        assert!(child.wait().unwrap().success());
        assert!(complete);
        assert_eq!(count, 2);
        samples.push(json!({"first_record_ms":first.unwrap(),"completion_ms":start.elapsed().as_secs_f64()*1000.0}));
    }
    println!(
        "{}",
        json!({"benchmark":"paginated-first-record","binary":binary,"later_page_delay_ms":250,"storage":false,"samples":samples})
    );
}

#[test]
fn paginated_search_streams_and_reads_first_page_before_later_failure() {
    let release = Arc::new(AtomicBool::new(false));
    let later_finished = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let finished = later_finished.clone();
    let article = Fixture::new(|_| {
        (
            200,
            "text/html",
            "<article>First page research evidence</article>".into(),
        )
    });
    // Organic parsers exclude the engine host; use a different loopback name.
    let article_url = article.url("paper").replace("127.0.0.1", "localhost");
    let server = Fixture::new(move |path| {
        if path.contains("first=") {
            let deadline = std::time::Instant::now() + Duration::from_secs(8);
            while !gate.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            finished.store(true, Ordering::SeqCst);
            (503, "text/plain", "Later page unavailable".into())
        } else {
            (
                200,
                "text/html",
                format!(
                    "<li class='b_algo'><h2><a href='{article_url}'>First page paper</a></h2></li><a class='sb_pagN' href='/bing?q=gripper&amp;first=11'>Next</a>"
                ),
            )
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let mut child = cli(&[
        "search",
        "gripper",
        "--sources",
        "web",
        "--search-mode",
        "balanced",
        "--engines",
        "bing",
        "--search-pages",
        "3",
        "--limit",
        "10",
        "--concurrency",
        "2",
        "--read",
        "--format",
        "jsonl",
    ])
    .arg("--data-dir")
    .arg(directory.path())
    .env("FFETCH_BING_SEARCH", server.url("bing"))
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
    let mut read_before_later_page = false;
    while let Ok(line) = receive.recv_timeout(Duration::from_secs(4)) {
        let event: Value = serde_json::from_str(&line).unwrap();
        let document = event["item"]["kind"] == "document";
        events.push(event);
        if document {
            read_before_later_page = !later_finished.load(Ordering::SeqCst);
            break;
        }
        if events.last().unwrap()["type"] == "complete" {
            break;
        }
    }
    release.store(true, Ordering::SeqCst);
    if !read_before_later_page {
        let _ = child.kill();
    }
    let output = child.wait_with_output().unwrap();
    reader.join().unwrap();
    events.extend(
        receive
            .try_iter()
            .map(|line| serde_json::from_str::<Value>(&line).unwrap()),
    );
    assert!(
        read_before_later_page,
        "first document waited for page two: {events:?}"
    );
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(events.iter().filter(|e| e["type"] == "record").count(), 2);
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == "record_update")
            .count(),
        0
    );
    let complete = events.last().unwrap();
    assert_eq!(complete["type"], "complete");
    assert_eq!(complete["result"]["status"], "partial");
    assert_eq!(
        complete["result"]["meta"]["coverage"][0]["meta"]["web_pagination"]["stop_reason"],
        "page_failed"
    );
    let run_id = complete["result"]["run_id"].as_str().unwrap();
    let saved = cli(&["inspect", run_id])
        .arg("--data-dir")
        .arg(directory.path())
        .output()
        .unwrap();
    assert!(saved.status.success());
    let saved = parsed(&saved);
    assert_eq!(saved["items"].as_array().unwrap().len(), 2);
    for event in events.iter().filter(|e| e["type"] == "record") {
        let record = saved["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == event["item"]["id"])
            .unwrap();
        assert_eq!(record["provenance"], event["item"]["provenance"]);
        assert_eq!(record["native"], event["item"]["native"]);
        assert_eq!(event["run_id"], run_id);
    }
}

#[tokio::test]
async fn bounded_page_progress_stops_requests_and_retains_unsent_records_at_deadline() {
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = requests.clone();
    let server = Fixture::new(move |path| {
        let page = observed.fetch_add(1, Ordering::SeqCst) + 1;
        assert!(
            page <= 2,
            "must not request another page while progress is backpressured: {path}"
        );
        (
            200,
            "text/html",
            format!(
                "<div class='snippet' data-type='web'><a href='https://example.org/{page}'><div class='search-snippet-title'>Page {page}</div></a></div><div class='pagination'><a href='/brave?q=gripper&amp;offset={page}'>Next</a></div>"
            ),
        )
    });
    let base = url::Url::parse(&server.base).unwrap();
    let providers = ffetch::providers::Providers {
        http: ffetch::net::Http::new(Duration::from_secs(2), 1024 * 1024, 2).unwrap(),
        browser: None,
        browser_only: false,
        hn_base: base.clone(),
        hn_search: base.clone(),
        web_search: base,
        brave_search: url::Url::parse(&server.url("brave")).unwrap(),
        searxng: None,
        concurrency: 2,
        web_pages: 3,
        budget: Duration::from_millis(500),
        reddit_community: None,
        youtube_region: "US".into(),
    };
    let (send, mut receive) = tokio::sync::mpsc::channel(1);
    let run = providers
        .search_with_progress("engine:brave", "gripper", 10, None, send)
        .await;
    assert_eq!(run.status, "partial");
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(run.items.len(), 2);
    assert_eq!(run.meta["web_pagination"]["stop_reason"], "deadline");
    assert_eq!(run.meta["web_pagination"]["pages_completed"], 2);
    let first = receive.recv().await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].id, run.items[0].id);
    assert_eq!(first[0].native, run.items[0].native);
    assert!(receive.recv().await.is_none());
}

#[test]
fn web_pagination_follows_engine_links_deduplicates_and_obeys_both_limits() {
    let requested = Arc::new(std::sync::Mutex::new(Vec::new()));
    let requests = requested.clone();
    let fixture = Fixture::new(move |path| {
        requests.lock().unwrap().push(path.to_owned());
        let url = url::Url::parse(&format!("http://fixture{path}")).unwrap();
        assert!(url.query_pairs().any(|(k, v)| k == "q" && v == "gripper"));
        let engine = url.path().trim_start_matches('/');
        let cursor = match engine {
            "brave" => "offset",
            "bing" => "first",
            "google" => "start",
            _ => panic!("{path}"),
        };
        let second = url.query_pairs().any(|(k, _)| k == cursor);
        if second {
            assert!(
                url.query_pairs()
                    .any(|(k, v)| k == cursor && v == if engine == "brave" { "1" } else { "11" })
            );
        }
        let records = ["shared", if second {"second"} else {"first"}].iter().map(|label| {
            let link = format!("https://example.org/{engine}/{label}");
            match engine {
                "brave" => format!("<div class='snippet' data-type='web'><a href='{link}'><span class='search-snippet-title'>{label}</span></a></div>"),
                "bing" => format!("<li class='b_algo'><h2><a href='{link}'>{label}</a></h2></li>"),
                _ => format!("<div class='MjjYud'><a href='{link}'><h3>{label}</h3></a></div>"),
            }
        }).collect::<String>();
        let next = format!(
            "/{engine}?q=gripper&amp;{cursor}={}",
            if second {
                21
            } else if engine == "brave" {
                1
            } else {
                11
            }
        );
        let pagination = match engine {
            "brave" => format!("<div class='pagination'><a href='{next}'>Next</a></div>"),
            "bing" => format!("<a class='sb_pagN' href='{next}'>Next page</a>"),
            _ => format!("<a id='pnnext' href='{next}'>Next</a>"),
        };
        (200, "text/html", records + &pagination)
    });
    let output = cli(&[
        "search",
        "gripper",
        "--sources",
        "web",
        "--search-mode",
        "deep",
        "--engines",
        "brave,bing,google",
        "--search-pages",
        "3",
        "--limit",
        "3",
        "--no-store",
    ])
    .env("FFETCH_BRAVE_SEARCH", fixture.url("brave"))
    .env("FFETCH_BING_SEARCH", fixture.url("bing"))
    .env("FFETCH_GOOGLE_SEARCH", fixture.url("google"))
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = parsed(&output);
    assert_eq!(data["items"].as_array().unwrap().len(), 9);
    assert_eq!(requested.lock().unwrap().len(), 6);
    for source in data["meta"]["coverage"].as_array().unwrap() {
        let page = &source["meta"]["web_pagination"];
        assert_eq!(page["pages_completed"], 2);
        assert_eq!(page["stop_reason"], "result_limit");
    }
    for record in data["items"].as_array().unwrap() {
        if record["url"].as_str().unwrap().ends_with("/second") {
            assert_eq!(record["native"]["origins"][0]["rank"], 4);
            assert_eq!(
                record["native"]["origins"][0]["rank_basis"],
                "observed-result-order"
            );
            assert_eq!(
                record["native"]["provider_data"]["search_page"]["number"],
                2
            );
            assert!(
                record["provenance"]["source_url"]
                    .as_str()
                    .unwrap()
                    .contains("&")
            );
        }
    }
    // A smaller page budget stops even when the result limit is not satisfied.
    let output = cli(&[
        "search",
        "gripper",
        "--sources",
        "web",
        "--search-mode",
        "deep",
        "--engines",
        "bing",
        "--search-pages",
        "2",
        "--limit",
        "10",
        "--no-store",
    ])
    .env("FFETCH_BING_SEARCH", fixture.url("bing"))
    .output()
    .unwrap();
    assert!(output.status.success());
    let data = parsed(&output);
    assert_eq!(data["items"].as_array().unwrap().len(), 3);
    assert_eq!(
        data["meta"]["coverage"][0]["meta"]["web_pagination"]["stop_reason"],
        "page_budget"
    );
    assert_eq!(requested.lock().unwrap().len(), 8);
}

#[test]
fn web_pagination_retains_completed_pages_on_failure_or_deadline() {
    let fixture = Fixture::new(|path| {
        if path.contains("first=") {
            if path.contains("q=slow") {
                thread::sleep(Duration::from_millis(1500));
            }
            return (503, "text/html", "Unavailable".into());
        }
        let query = if path.contains("q=slow") {
            "slow"
        } else {
            "fail"
        };
        (
            200,
            "text/html",
            format!(
                "<li class='b_algo'><h2><a href='https://example.org/available'>Available</a></h2></li><a class='sb_pagN' href='/bing?q={query}&amp;first=11'>Next</a>"
            ),
        )
    });
    for (query, reason) in [("fail", "page_failed"), ("slow", "deadline")] {
        let output = cli(&[
            "search",
            query,
            "--sources",
            "web",
            "--search-mode",
            "deep",
            "--engines",
            "bing",
            "--search-pages",
            "3",
            "--limit",
            "10",
            "--timeout",
            "1",
            "--no-store",
        ])
        .env("FFETCH_BING_SEARCH", fixture.url("bing"))
        .output()
        .unwrap();
        assert_eq!(output.status.code(), Some(3));
        let data = parsed(&output);
        assert_eq!(data["items"].as_array().unwrap().len(), 1);
        let page = &data["meta"]["coverage"][0]["meta"]["web_pagination"];
        assert_eq!(page["pages_requested"], 2);
        assert_eq!(page["pages_completed"], 1);
        assert_eq!(page["stop_reason"], reason);
    }
}

#[test]
fn web_pagination_rejects_changed_origin_query_and_cursor() {
    let followed = Arc::new(AtomicBool::new(false));
    let trap_flag = followed.clone();
    let trap = Fixture::new(move |_| {
        trap_flag.store(true, Ordering::SeqCst);
        (200, "text/html", String::new())
    });
    let foreign = trap.url("bing?q=origin&first=11");
    let fixture = Fixture::new(move |path| {
        assert!(
            !path.contains("first="),
            "must not follow invalid pagination"
        );
        let next = if path.contains("q=origin") {
            foreign.clone()
        } else if path.contains("q=query") {
            "/bing?q=other&first=11".into()
        } else {
            "/bing?q=cursor&first=0".into()
        };
        (
            200,
            "text/html",
            format!(
                "<li class='b_algo'><h2><a href='https://example.org/available'>Available</a></h2></li><a class='sb_pagN' href='{next}'>Next</a>"
            ),
        )
    });
    for query in ["origin", "query", "cursor"] {
        let output = cli(&[
            "search",
            query,
            "--sources",
            "web",
            "--search-mode",
            "deep",
            "--engines",
            "bing",
            "--search-pages",
            "3",
            "--limit",
            "10",
            "--no-store",
        ])
        .env("FFETCH_BING_SEARCH", fixture.url("bing"))
        .output()
        .unwrap();
        assert_eq!(output.status.code(), Some(3));
        let data = parsed(&output);
        assert_eq!(data["items"].as_array().unwrap().len(), 1);
        assert_eq!(
            data["meta"]["coverage"][0]["meta"]["web_pagination"]["stop_reason"],
            "pagination_rejected"
        );
        assert_eq!(data["errors"][0]["code"], "pagination_rejected");
        assert_eq!(data["errors"][0]["retryable"], false);
        assert!(
            data["errors"][0]["message"]
                .as_str()
                .unwrap()
                .contains("pagination")
        );
    }
    assert!(!followed.load(Ordering::SeqCst));
}

#[test]
fn web_pagination_stops_on_missing_links_repeated_results_and_unsupported_engines() {
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = requests.clone();
    let fixture = Fixture::new(move |path| {
        observed.fetch_add(1, Ordering::SeqCst);
        assert!(
            !path.contains("first=21"),
            "must stop after a page with no new results"
        );
        let html = if path.contains("q=unsupported") {
            "<div class='result'><a class='result__a' href='https://example.org/paper'>Paper</a></div>".into()
        } else {
            let mut html =
                "<li class='b_algo'><h2><a href='https://example.org/paper'>Paper</a></h2></li>"
                    .to_owned();
            if path.contains("q=repeated") {
                let next = if path.contains("first=") { 21 } else { 11 };
                html += &format!(
                    "<a class='sb_pagN' href='/bing?q=repeated&amp;first={next}'>Next</a>"
                );
            }
            html
        };
        (200, "text/html", html)
    });
    for (engine, query, reason) in [
        ("bing", "missing", "no_recognized_next_link"),
        ("bing", "repeated", "no_new_results"),
        ("duckduckgo", "unsupported", "pagination_unsupported"),
    ] {
        let output = cli(&[
            "search",
            query,
            "--sources",
            "web",
            "--search-mode",
            "deep",
            "--engines",
            engine,
            "--search-pages",
            "3",
            "--limit",
            "10",
            "--no-store",
        ])
        .env("FFETCH_BING_SEARCH", fixture.url("bing"))
        .env("FFETCH_WEB_SEARCH", fixture.url("ddg"))
        .output()
        .unwrap();
        assert!(output.status.success());
        let data = parsed(&output);
        assert_eq!(data["items"].as_array().unwrap().len(), 1);
        let pagination = &data["meta"]["coverage"][0]["meta"]["web_pagination"];
        assert_eq!(pagination["coverage"], "bounded");
        assert_eq!(pagination["stop_reason"], reason);
    }
    assert_eq!(requests.load(Ordering::SeqCst), 4);
}

#[test]
#[cfg(unix)]
fn failed_browser_startup_is_diagnosed_once_per_search() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let browser = dir.path().join("broken-browser");
    let launches = dir.path().join("launches");
    std::fs::write(&browser, "#!/bin/sh\nprintf 'start\\n' >> \"$FFETCH_TEST_LAUNCH_COUNT\"\nprintf 'No usable sandbox!\\nprivate diagnostic https://secret.invalid/?token=private\\n' >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = Fixture::new(|path| {
        if path.starts_with("/bing") {
            (200, "text/html", "<li class='b_algo'><h2><a href='https://example.org/gripper'>Gripper design</a></h2></li>".into())
        } else {
            (200, "text/html", "<title>Captcha</title>".into())
        }
    });
    let output = cli(&[
        "search",
        "gripper",
        "--sources",
        "web",
        "--engines",
        "google,bing,duckduckgo",
        "--search-mode",
        "deep",
        "--browser",
        "auto",
        "--browser-path",
        browser.to_str().unwrap(),
        "--no-store",
    ])
    .env("FFETCH_GOOGLE_SEARCH", server.url("google"))
    .env("FFETCH_BING_SEARCH", server.url("bing"))
    .env("FFETCH_WEB_SEARCH", server.url("duckduckgo"))
    .env("FFETCH_TEST_LAUNCH_COUNT", &launches)
    .output()
    .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = parsed(&output);
    assert_eq!(data["items"].as_array().unwrap().len(), 1);
    assert_eq!(data["errors"].as_array().unwrap().len(), 2);
    for error in data["errors"].as_array().unwrap() {
        assert_eq!(error["code"], "browser_unavailable");
        assert_eq!(error["retryable"], false);
        assert!(error["next_action"].as_str().unwrap().contains("sandbox"));
        let message = error["message"].as_str().unwrap();
        assert!(
            message.contains("Chromium sandbox initialization failed"),
            "{message}"
        );
        assert!(message.contains("--browser off"));
        assert!(!message.contains("secret.invalid"));
        assert!(!message.contains("inspect /tmp/"));
    }
    assert_eq!(std::fs::read_to_string(&launches).unwrap(), "start\n");
    // Failure observations expire with the invocation, not permanently.
    let output = cli(&[
        "search",
        "gripper",
        "--sources",
        "web",
        "--engines",
        "google",
        "--search-mode",
        "deep",
        "--browser",
        "auto",
        "--browser-path",
        browser.to_str().unwrap(),
        "--no-store",
    ])
    .env("FFETCH_GOOGLE_SEARCH", server.url("google"))
    .env("FFETCH_TEST_LAUNCH_COUNT", &launches)
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        std::fs::read_to_string(&launches).unwrap(),
        "start\nstart\n"
    );
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
