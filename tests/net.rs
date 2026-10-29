use ffetch::net::Http;
use futures::{StreamExt, stream};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use url::Url;

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
    body_delay: Duration,
}
fn reply(status: u16, headers: &[(&'static str, &str)], body: &str) -> Reply {
    Reply {
        status,
        headers: headers.iter().map(|(k, v)| (*k, v.to_string())).collect(),
        body: body.into(),
        body_delay: Duration::ZERO,
    }
}
struct Site {
    base: Url,
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}
impl Site {
    fn new(route: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let quitting = stop.clone();
        let route = Arc::new(route);
        let task = thread::spawn(move || {
            let mut workers = Vec::new();
            while !quitting.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        let route = route.clone();
                        workers.push(thread::spawn(move || {
                            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                            let mut data = Vec::new(); let mut buffer = [0;4096];
                            while !data.windows(4).any(|w| w == b"\r\n\r\n") {
                                match socket.read(&mut buffer) { Ok(0) | Err(_) => return, Ok(n) => data.extend_from_slice(&buffer[..n]) }
                            }
                            let request = String::from_utf8_lossy(&data);
                            let path = request.lines().next().unwrap().split_whitespace().nth(1).unwrap();
                            let response = route(path);
                            let headers = response.headers.iter().map(|(k,v)| format!("{k}: {v}\r\n")).collect::<String>();
                            let _ = write!(socket, "HTTP/1.1 {} Fixture\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n", response.status, response.body.len());
                            thread::sleep(response.body_delay);
                            let _ = socket.write_all(response.body.as_bytes());
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
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

#[tokio::test]
async fn cloudflare_challenge_header_is_not_content_or_a_blind_retry() {
    for (status, delay) in [
        (200, None),
        (403, None),
        (503, None),
        (503, Some("0")),
        (429, Some("3600")),
    ] {
        let site = Site::new(move |_| {
            let mut headers = vec![("cf-mitigated", "challenge")];
            if let Some(delay) = delay {
                headers.push(("Retry-After", delay));
            }
            reply(
                status,
                &headers,
                "<article>Unrecognized challenge markup</article>",
            )
        });
        let http = Http::new(Duration::from_secs(1), 4096, 4).unwrap();
        let error = http.get(site.base.clone()).await.err().unwrap().to_string();
        assert!(
            error.contains(&format!("HTTP {status} challenge_detected")),
            "{error}"
        );
        assert_eq!(http.stats().requests_started, 1);
        assert_eq!(http.stats().retries_started, 0);
        if status == 429 {
            assert!(http.stats().origins[0].cooldown_remaining_ms > 0);
            assert!(error.contains("Retry-After"));
        }
    }
}

#[test]
fn challenged_engine_reports_an_action_without_losing_other_engines_results() {
    let site = Site::new(|path| {
        if path.starts_with("/google") {
            // Even plausible organic markup must not override the header.
            reply(
                200,
                &[("cf-mitigated", "challenge")],
                "<div class='MjjYud'><a href='https://example.org/fake'><h3>False research</h3></a></div>",
            )
        } else {
            reply(
                200,
                &[],
                "<li class='b_algo'><h2><a href='https://example.org/robot'>Real research</a></h2></li>",
            )
        }
    });
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ffetch"))
        .args([
            "search",
            "robot",
            "--sources",
            "web",
            "--engines",
            "google,bing",
            "--browser",
            "off",
            "--no-store",
        ])
        .env(
            "FFETCH_GOOGLE_SEARCH",
            site.base.join("google").unwrap().as_str(),
        )
        .env(
            "FFETCH_BING_SEARCH",
            site.base.join("bing").unwrap().as_str(),
        )
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let run: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(run["items"].as_array().unwrap().len(), 1);
    assert_eq!(run["items"][0]["title"], "Real research");
    assert_eq!(run["errors"][0]["code"], "challenge_unresolved");
    assert_eq!(run["errors"][0]["source"], "engine:google");
    assert!(
        run["errors"][0]["next_action"]
            .as_str()
            .unwrap()
            .contains("--session")
    );
}

#[tokio::test]
async fn retry_after_waits_then_recovers_with_one_retry() {
    let times = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = times.clone();
    let site = Site::new(move |_| {
        let mut times = recorded.lock().unwrap();
        times.push(Instant::now());
        if times.len() == 1 {
            reply(429, &[("Retry-After", "1")], "limited")
        } else {
            reply(200, &[], "recovered")
        }
    });
    let http = Http::new(Duration::from_secs(3), 4096, 4).unwrap();
    let response = http.get(site.base.clone()).await.unwrap();
    assert_eq!(response.bytes, b"recovered");
    let times = times.lock().unwrap();
    assert_eq!(times.len(), 2);
    assert!(times[1].duration_since(times[0]) >= Duration::from_secs(1));
    let stats = http.stats();
    assert_eq!(stats.requests_started, 2);
    assert_eq!(stats.retries_started, 1);
    assert_eq!(stats.backoff_responses, 1);
}

#[tokio::test]
async fn retry_is_bounded_and_requires_an_eligible_status_and_valid_delay() {
    for (status, header, expected) in [
        (503, "0", 2),
        (429, "invalid", 1),
        (403, "0", 1),
        (429, "-1", 1),
        (503, "", 1),
    ] {
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        let site = Site::new(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
            reply(status, &[("Retry-After", header)], "unavailable")
        });
        let http = Http::new(Duration::from_secs(1), 4096, 4).unwrap();
        assert_eq!(
            http.get(site.base.clone()).await.unwrap().status.as_u16(),
            status
        );
        assert_eq!(count.load(Ordering::Relaxed), expected);
        assert_eq!(http.stats().retries_started, (expected - 1) as u64);
    }
}

#[tokio::test]
async fn cooldown_is_shared_and_does_not_block_other_origins() {
    let fast_seen = Arc::new(AtomicBool::new(false));
    let seen = fast_seen.clone();
    let fast = Site::new(move |_| {
        seen.store(true, Ordering::SeqCst);
        reply(200, &[], "fast")
    });
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let slow = Site::new(move |_| {
        if observed.fetch_add(1, Ordering::Relaxed) == 0 {
            reply(429, &[("Retry-After", "1")], "limited")
        } else {
            assert!(
                fast_seen.load(Ordering::SeqCst),
                "cooldown monopolized the global permit"
            );
            reply(200, &[], "slow")
        }
    });
    let http = Http::with_limits(Duration::from_secs(3), 4096, 1, 1).unwrap();
    assert_eq!(
        http.get_once(slow.base.clone())
            .await
            .unwrap()
            .status
            .as_u16(),
        429
    );
    let (slow, fast) = tokio::join!(http.get(slow.base.clone()), http.get(fast.base.clone()));
    assert_eq!(slow.unwrap().bytes, b"slow");
    assert_eq!(fast.unwrap().bytes, b"fast");
    assert_eq!(count.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn excessive_retry_after_fails_fast_without_another_dispatch() {
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let site = Site::new(move |_| {
        observed.fetch_add(1, Ordering::Relaxed);
        reply(
            429,
            &[("Retry-After", "999999999999999999999999")],
            "limited",
        )
    });
    let http = Http::new(Duration::from_millis(100), 4096, 4).unwrap();
    assert_eq!(
        http.get(site.base.clone()).await.unwrap().status.as_u16(),
        429
    );
    let error = http.get(site.base.clone()).await.err().unwrap().to_string();
    assert!(error.contains("Retry-After"));
    assert_eq!(count.load(Ordering::Relaxed), 1);
    assert_eq!(http.stats().retries_started, 0);
    assert!(http.stats().origins[0].cooldown_remaining_ms > 100);
}

#[tokio::test]
async fn redirect_hops_share_destination_limits_with_direct_requests() {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let running = active.clone();
    let maximum = peak.clone();
    let destination = Site::new(move |_| {
        let current = running.fetch_add(1, Ordering::SeqCst) + 1;
        maximum.fetch_max(current, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(30));
        running.fetch_sub(1, Ordering::SeqCst);
        reply(200, &[], "destination")
    });
    let url = destination.base.to_string();
    let redirect = Site::new(move |_| reply(302, &[("Location", &url)], ""));
    let http = Http::with_limits(Duration::from_secs(3), 4096, 6, 2).unwrap();
    let mut work = stream::iter((0..8).map(|n| {
        http.get(if n % 2 == 0 {
            redirect.base.clone()
        } else {
            destination.base.clone()
        })
    }))
    .buffer_unordered(8);
    while let Some(response) = work.next().await {
        assert_eq!(response.unwrap().url, destination.base);
    }
    assert!(peak.load(Ordering::SeqCst) <= 2);
    let stats = http.stats();
    assert_eq!(stats.requests_started, 12);
    assert_eq!(stats.redirects_followed, 4);
    assert_eq!(stats.origins.len(), 2);
}

#[tokio::test]
async fn queue_wait_and_cancellation_are_inside_the_request_deadline() {
    let site = Site::new(|_| {
        thread::sleep(Duration::from_millis(300));
        reply(200, &[], "late")
    });
    let http = Http::with_limits(Duration::from_millis(100), 4096, 2, 1).unwrap();
    let start = Instant::now();
    let results = futures::future::join_all((0..5).map(|_| http.get(site.base.clone()))).await;
    assert!(results.iter().all(Result::is_err));
    assert!(
        start.elapsed() < Duration::from_millis(350),
        "request queue extended the deadline"
    );
    assert!(http.stats().requests_started <= 2);
}

#[tokio::test]
async fn redirects_reject_userinfo_and_bound_loops() {
    let site = Site::new(|path| {
        if path == "/userinfo" {
            reply(302, &[("Location", "http://user:secret@127.0.0.1/")], "")
        } else {
            reply(302, &[("Location", "/loop")], "")
        }
    });
    let http = Http::new(Duration::from_secs(2), 4096, 4).unwrap();
    let error = http
        .get(site.base.join("userinfo").unwrap())
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("userinfo"));
    assert!(!error.contains("secret"));
    let error = http
        .get(site.base.join("loop").unwrap())
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("redirect limit"));
    assert_eq!(http.stats().requests_started, 12);
}

#[tokio::test]
async fn redirect_retry_after_releases_capacity_and_cannot_exceed_deadline() {
    let fast_seen = Arc::new(AtomicBool::new(false));
    let seen = fast_seen.clone();
    let fast = Site::new(move |_| {
        seen.store(true, Ordering::SeqCst);
        reply(200, &[], "fast")
    });
    let target_count = Arc::new(AtomicUsize::new(0));
    let count = target_count.clone();
    let target = Site::new(move |_| {
        assert!(
            fast_seen.load(Ordering::SeqCst),
            "redirect delay held the global permit"
        );
        count.fetch_add(1, Ordering::SeqCst);
        reply(200, &[], "target")
    });
    let destination = target.base.to_string();
    let redirect =
        Site::new(move |_| reply(302, &[("Location", &destination), ("Retry-After", "1")], ""));
    let http = Http::with_limits(Duration::from_secs(3), 4096, 1, 1).unwrap();
    let (redirected, fast) =
        tokio::join!(http.get(redirect.base.clone()), http.get(fast.base.clone()));
    assert_eq!(redirected.unwrap().bytes, b"target");
    assert_eq!(fast.unwrap().bytes, b"fast");
    let short = Http::new(Duration::from_millis(100), 4096, 1).unwrap();
    assert!(
        short
            .get(redirect.base.clone())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("Retry-After")
    );
    assert_eq!(target_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn transient_status_survives_a_body_timeout() {
    let site = Site::new(|_| {
        let mut response = reply(503, &[("Retry-After", "1")], "too late");
        response.body_delay = Duration::from_millis(300);
        response
    });
    let http = Http::new(Duration::from_millis(100), 4096, 1).unwrap();
    let error = http.get(site.base.clone()).await.err().unwrap().to_string();
    assert!(error.contains("HTTP 503"), "{error}");
    assert_eq!(http.stats().requests_started, 1);
    assert_eq!(http.stats().retries_started, 0);
}

#[test]
fn cli_reports_transport_recovery_and_origin_cooldowns() {
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let site = Site::new(move |path| {
        if path.contains("q=limited") {
            return reply(429, &[("Retry-After", "9999")], "limited");
        }
        if observed.fetch_add(1, Ordering::SeqCst) == 0 {
            return reply(429, &[("Retry-After", "0")], "limited");
        }
        let second = path.contains("first=");
        let label = if second { "second" } else { "first" };
        let mut html = format!(
            "<li class='b_algo'><h2><a href='https://example.org/{label}'>{label}</a></h2></li>"
        );
        if !second {
            html += "<a class='sb_pagN' href='/bing?q=gripper&amp;first=11'>Next</a>";
        }
        reply(200, &[], &html)
    });
    for (query, code) in [("gripper", 0), ("limited", 1)] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_ffetch"))
            .args([
                "search",
                query,
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
                "--no-store",
            ])
            .env(
                "FFETCH_BING_SEARCH",
                site.base.join("bing").unwrap().as_str(),
            )
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let run: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let stats = &run["meta"]["http"];
        assert_eq!(stats["backoff_responses"], 1);
        assert_eq!(
            stats["origins"][0]["origin"],
            site.base.origin().ascii_serialization()
        );
        assert!(!stats.to_string().contains(query));
        if query == "gripper" {
            assert_eq!(run["items"].as_array().unwrap().len(), 2);
            assert_eq!(stats["requests_started"], 3);
            assert_eq!(stats["retries_started"], 1);
        } else {
            assert_eq!(run["errors"][0]["code"], "rate_limited");
            assert_eq!(stats["requests_started"], 1);
            assert_eq!(stats["retries_started"], 0);
            assert!(
                stats["origins"][0]["cooldown_remaining_ms"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
        }
    }
}

#[tokio::test]
async fn nested_budgets_narrow_deadlines_without_leaking_between_sources() {
    let short = Site::new(|_| reply(429, &[("Retry-After", "1")], "limited"));
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let long = Site::new(move |_| {
        if observed.fetch_add(1, Ordering::Relaxed) == 0 {
            reply(429, &[("Retry-After", "1")], "limited")
        } else {
            reply(200, &[], "recovered")
        }
    });
    let http = Http::new(Duration::from_secs(3), 4096, 4).unwrap();
    let short_work = ffetch::net::within_budget(Duration::from_millis(100), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        ffetch::net::within_budget(Duration::from_secs(10), http.get(short.base.clone()))
            .await
            .unwrap()
    });
    let long_work = ffetch::net::within_budget(Duration::from_secs(2), http.get(long.base.clone()));
    let (short, long) = tokio::join!(short_work, long_work);
    assert_eq!(short.unwrap().unwrap().status.as_u16(), 429);
    assert_eq!(long.unwrap().unwrap().bytes, b"recovered");
    assert_eq!(count.load(Ordering::Relaxed), 2);
    assert_eq!(http.stats().requests_started, 3);
    assert_eq!(http.stats().retries_started, 1);
}
