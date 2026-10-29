//! Loopback TLS fixtures. These generated credentials are public test material.
//! Only the test client trusts this CA; no machine trust store is changed.
use super::*;
use bytes::Bytes;
use std::{future::poll_fn, sync::atomic::AtomicUsize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{Barrier, watch},
    task::{JoinHandle, JoinSet},
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    },
    server::TlsStream,
};

const CA: &[u8] = include_bytes!("../tests/fixtures/http2-ca.pem");
const CERT: &[u8] = include_bytes!("../tests/fixtures/http2-server.pem");
const KEY: &[u8] = include_bytes!("../tests/fixtures/http2-server-key.pem");
const PAYLOAD: &[u8] = b"verified research evidence\n";
// gzip of 8192 'A' bytes, so the decoded size exceeds the 1024-byte test cap.
const GZIP: &[u8] = &[
    31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 237, 193, 1, 13, 0, 0, 0, 194, 160, 108, 239, 95, 202, 28,
    110, 64, 1, 0, 0, 0, 0, 0, 0, 0, 239, 6, 204, 59, 37, 50, 0, 32, 0, 0,
];

struct State {
    connections: AtomicUsize,
    handshakes: AtomicUsize,
    active: AtomicUsize,
    peak: AtomicUsize,
    held: watch::Sender<usize>,
    reset: watch::Sender<usize>,
    release: watch::Sender<bool>,
    parallel: Barrier,
    limited: AtomicUsize,
}

struct Active(Arc<State>);
impl Active {
    fn new(state: &Arc<State>) -> Self {
        let n = state.active.fetch_add(1, Ordering::SeqCst) + 1;
        state.peak.fetch_max(n, Ordering::SeqCst);
        Self(state.clone())
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Site {
    base: Url,
    state: Arc<State>,
    task: JoinHandle<()>,
}
impl Drop for Site {
    fn drop(&mut self) {
        // The accept task owns a JoinSet, which aborts connection tasks on drop.
        // Each connection in turn owns its response tasks.
        self.task.abort();
    }
}
impl Site {
    async fn new(h2: bool) -> Self {
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from_pem_slice(CERT).unwrap()],
                PrivateKeyDer::from_pem_slice(KEY).unwrap(),
            )
            .unwrap();
        config.alpn_protocols = if h2 {
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        } else {
            vec![b"http/1.1".to_vec()]
        };
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("https://{}/", listener.local_addr().unwrap())).unwrap();
        let state = Arc::new(State {
            connections: AtomicUsize::new(0),
            handshakes: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            held: watch::channel(0).0,
            reset: watch::channel(0).0,
            release: watch::channel(false).0,
            parallel: Barrier::new(4),
            limited: AtomicUsize::new(0),
        });
        let observed = state.clone();
        let task = tokio::spawn(async move {
            let mut workers = JoinSet::new();
            loop {
                tokio::select! {
                    socket = listener.accept() => {
                        let (socket, _) = socket.unwrap();
                        socket.set_nodelay(true).unwrap();
                        observed.connections.fetch_add(1, Ordering::SeqCst);
                        let acceptor = acceptor.clone();
                        let state = observed.clone();
                        workers.spawn(async move {
                            if let Ok(tls) = acceptor.accept(socket).await {
                                state.handshakes.fetch_add(1, Ordering::SeqCst);
                                if tls.get_ref().1.alpn_protocol() == Some(b"h2") {
                                    serve_h2(tls, state).await;
                                } else {
                                    serve_h1(tls, state).await;
                                }
                            }
                        });
                    }
                    result = workers.join_next(), if !workers.is_empty() => {
                        result.unwrap().unwrap();
                    }
                }
            }
        });
        Self { base, state, task }
    }

    fn url(&self, path: &str) -> Url {
        self.base.join(path).unwrap()
    }

    fn client(&self, h1: bool, max_bytes: usize, limit: usize, trust: bool) -> Http {
        let timeout = Duration::from_secs(5);
        let mut http = Http::with_limits(timeout, max_bytes, limit, limit).unwrap();
        let builder = || {
            let mut client = Http::client_builder(timeout).no_proxy();
            if trust {
                client = client.add_root_certificate(reqwest::Certificate::from_pem(CA).unwrap());
            }
            client
        };
        http.client = if h1 {
            builder().http1_only()
        } else {
            builder()
        }
        .build()
        .unwrap();
        *http.browser_http1.lock().unwrap() = Some(builder().http1_only().build().unwrap());
        http
    }

    fn assert_connections(&self, expected: usize) {
        assert_eq!(self.state.connections.load(Ordering::SeqCst), expected);
        assert_eq!(self.state.handshakes.load(Ordering::SeqCst), expected);
    }
}

async fn observed(sender: &watch::Sender<usize>, minimum: usize) {
    let mut receiver = sender.subscribe();
    tokio::time::timeout(Duration::from_secs(2), receiver.wait_for(|n| *n >= minimum))
        .await
        .expect("server did not observe the stream state")
        .unwrap();
}

async fn delay(path: &str, state: &State) {
    if path == "/parallel" {
        state.parallel.wait().await;
    }
    if path == "/bench" {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn serve_h2(tls: TlsStream<TcpStream>, state: Arc<State>) {
    let Ok(mut connection) = h2::server::handshake(tls).await else {
        return;
    };
    let mut workers = JoinSet::new();
    loop {
        tokio::select! {
            request = connection.accept() => {
                let Some(Ok((request, respond))) = request else { break };
                let path = request.uri().path().to_owned();
                let state = state.clone();
                workers.spawn(async move { respond_h2(&path, respond, state).await });
            }
            result = workers.join_next(), if !workers.is_empty() => {
                result.unwrap().unwrap();
            }
        }
    }
}

async fn respond_h2(path: &str, mut respond: h2::server::SendResponse<Bytes>, state: Arc<State>) {
    let _active = Active::new(&state);
    let mut response = http::Response::builder().header("content-type", "text/plain");
    if path == "/limited" && state.limited.fetch_add(1, Ordering::SeqCst) == 0 {
        response = response.status(429).header("retry-after", "0");
    }
    if path == "/cooldown" {
        response = response.status(429).header("retry-after", "3600");
    }
    if path == "/gzip" {
        response = response.header("content-encoding", "gzip");
    }
    let Ok(mut stream) = respond.send_response(response.body(()).unwrap(), false) else {
        return;
    };
    if path == "/held" || path == "/large" {
        let mut release = state.release.subscribe();
        if path == "/large"
            && stream
                .send_data(Bytes::from(vec![b'L'; 8192]), false)
                .is_err()
        {
            return;
        }
        state.held.send_modify(|n| *n += 1);
        tokio::select! {
            _ = release.wait_for(|released| *released) => (),
            reset = poll_fn(|cx| stream.poll_reset(cx)) => {
                assert_eq!(reset.unwrap(), h2::Reason::CANCEL);
                state.reset.send_modify(|n| *n += 1);
                return;
            }
        }
    }
    delay(path, &state).await;
    let body = if path == "/gzip" { GZIP } else { PAYLOAD };
    let _ = stream.send_data(Bytes::from_static(body), true);
}

async fn serve_h1(tls: TlsStream<TcpStream>, state: Arc<State>) {
    let mut io = BufReader::new(tls);
    loop {
        let mut line = String::new();
        if !matches!(io.read_line(&mut line).await, Ok(n) if n > 0) {
            return;
        }
        let path = line.split_whitespace().nth(1).unwrap().to_owned();
        loop {
            line.clear();
            if !matches!(io.read_line(&mut line).await, Ok(n) if n > 0) {
                return;
            }
            if line == "\r\n" {
                break;
            }
        }
        let _active = Active::new(&state);
        delay(&path, &state).await;
        if path.starts_with("/cross?") {
            let url = Url::parse(&format!("https://fixture{path}")).unwrap();
            let target = url
                .query_pairs()
                .find(|(key, _)| key == "target")
                .unwrap()
                .1
                .into_owned();
            let head =
                format!("HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\n\r\n");
            if io.get_mut().write_all(head.as_bytes()).await.is_err() {
                return;
            }
            if io.get_mut().flush().await.is_err() {
                return;
            }
            continue;
        }
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
            PAYLOAD.len()
        );
        if io.get_mut().write_all(head.as_bytes()).await.is_err() {
            return;
        }
        if io.get_mut().write_all(PAYLOAD).await.is_err() {
            return;
        }
        if io.get_mut().flush().await.is_err() {
            return;
        }
    }
}

async fn batch(site: &Site, http: &Http, path: &str) {
    let responses = tokio::time::timeout(
        Duration::from_secs(3),
        futures::future::join_all((0..8).map(|_| http.get(site.url(path)))),
    )
    .await
    .expect("batch stalled");
    for response in responses {
        assert_eq!(response.unwrap().bytes, PAYLOAD);
    }
}

#[tokio::test]
async fn multiplexing_reuses_one_tls_connection_with_the_same_origin_bound() {
    for h1 in [true, false] {
        let site = Site::new(true).await;
        let http = site.client(h1, 4096, 4, true);
        assert_eq!(http.get(site.url("/warm")).await.unwrap().bytes, PAYLOAD);
        batch(&site, &http, "/parallel").await;
        site.assert_connections(if h1 { 4 } else { 1 });
        assert_eq!(site.state.peak.load(Ordering::SeqCst), 4);
        assert_eq!(http.stats().requests_started, 9);
        assert_eq!(
            http.stats().responses_by_protocol.http1,
            if h1 { 9 } else { 0 }
        );
        assert_eq!(
            http.stats().responses_by_protocol.http2,
            if h1 { 0 } else { 9 }
        );
    }
}

#[tokio::test]
async fn multiplexed_fast_stream_completes_while_another_stream_waits() {
    let site = Site::new(true).await;
    let http = site.client(false, 4096, 4, true);
    let held = {
        let http = http.clone();
        let url = site.url("/held");
        tokio::spawn(async move { http.get(url).await })
    };
    observed(&site.state.held, 1).await;
    let fast = tokio::time::timeout(Duration::from_secs(1), http.get(site.url("/fast")))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fast.bytes, PAYLOAD);
    assert!(!held.is_finished());
    site.state.release.send_replace(true);
    assert_eq!(held.await.unwrap().unwrap().bytes, PAYLOAD);
    site.assert_connections(1);
}

#[tokio::test]
async fn cancelling_at_deadline_resets_one_stream_and_releases_all_permits() {
    let site = Site::new(true).await;
    let http = site.client(false, 4096, 1, true);
    let held = {
        let http = http.clone();
        let url = site.url("/held");
        tokio::spawn(async move { within_budget(Duration::from_millis(300), http.get(url)).await })
    };
    observed(&site.state.held, 1).await;
    // This request queues behind the held stream's global and origin permits.
    let fast = tokio::time::timeout(Duration::from_secs(2), http.get(site.url("/fast")))
        .await
        .unwrap()
        .unwrap();
    assert!(held.await.unwrap().is_err());
    observed(&site.state.reset, 1).await;
    assert_eq!(fast.bytes, PAYLOAD);
    site.assert_connections(1);
}

#[tokio::test]
async fn oversized_stream_without_content_length_is_cancelled_without_closing_connection() {
    let site = Site::new(true).await;
    let http = site.client(false, 1024, 1, true);
    let error = http
        .get(site.url("/large"))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("exceeds --max-bytes (1024)"), "{error}");
    observed(&site.state.reset, 1).await;
    assert_eq!(http.get(site.url("/fast")).await.unwrap().bytes, PAYLOAD);
    site.assert_connections(1);
}

#[tokio::test]
async fn compressed_http2_bodies_are_limited_after_decoding() {
    let site = Site::new(true).await;
    let limited = site.client(false, 1024, 1, true);
    assert!(
        limited
            .get(site.url("/gzip"))
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("max-bytes")
    );
    assert_eq!(limited.get(site.url("/fast")).await.unwrap().bytes, PAYLOAD);
    site.assert_connections(1);
    let larger = site.client(false, 16384, 1, true);
    assert_eq!(
        larger.get(site.url("/gzip")).await.unwrap().bytes,
        vec![b'A'; 8192]
    );
}

#[tokio::test]
async fn alpn_falls_back_to_http1_and_reuses_tls_when_server_has_no_http2() {
    let site = Site::new(false).await;
    let http = site.client(false, 4096, 4, true);
    for _ in 0..2 {
        assert_eq!(http.get(site.url("/fast")).await.unwrap().bytes, PAYLOAD);
    }
    site.assert_connections(1);
    assert_eq!(http.stats().responses_by_protocol.http1, 2);
    assert_eq!(http.stats().responses_by_protocol.http2, 0);
}

#[tokio::test]
async fn http2_negotiation_does_not_accept_an_untrusted_certificate() {
    let site = Site::new(true).await;
    let http = site.client(false, 4096, 1, false);
    assert!(http.get(site.url("/fast")).await.is_err());
    assert_eq!(site.state.handshakes.load(Ordering::SeqCst), 0);
    assert_eq!(http.stats().responses_by_protocol.http2, 0);
}

#[tokio::test]
async fn retry_after_and_cooldown_keep_http2_dispatch_bounded() {
    let site = Site::new(true).await;
    let http = site.client(false, 4096, 1, true);
    assert_eq!(
        http.get(site.url("/limited")).await.unwrap().status,
        StatusCode::OK
    );
    assert_eq!(http.stats().retries_started, 1);
    assert_eq!(
        http.get(site.url("/cooldown")).await.unwrap().status,
        StatusCode::TOO_MANY_REQUESTS
    );
    let error = within_budget(Duration::from_millis(50), http.get(site.url("/fast")))
        .await
        .expect("excessive Retry-After should fail before the deadline")
        .err()
        .unwrap();
    assert!(error.to_string().contains("Retry-After"));
    assert_eq!(http.stats().requests_started, 3);
    assert_eq!(http.stats().backoff_responses, 2);
    assert!(
        http.get_with_browser_user_agent(site.url("/fast"), false)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("Retry-After")
    );
    assert_eq!(http.stats().requests_started, 3);
    site.assert_connections(1);
}

#[tokio::test]
async fn browser_profile_keeps_http1_reusable_and_resets_protocol_on_cross_origin_redirect() {
    let site = Site::new(true).await;
    let destination = Site::new(true).await;
    let http = site.client(false, 4096, 4, true);
    for _ in 0..2 {
        assert_eq!(
            http.get_with_browser_user_agent(site.url("/fast"), false)
                .await
                .unwrap()
                .bytes,
            PAYLOAD
        );
    }
    site.assert_connections(1);
    assert_eq!(http.stats().responses_by_protocol.http1, 2);
    assert_eq!(http.get(site.url("/fast")).await.unwrap().bytes, PAYLOAD);
    site.assert_connections(2);
    let mut redirect = site.url("/cross");
    redirect
        .query_pairs_mut()
        .append_pair("target", destination.url("/fast").as_str());
    assert_eq!(
        http.get_with_browser_user_agent(redirect, false)
            .await
            .unwrap()
            .bytes,
        PAYLOAD
    );
    site.assert_connections(2);
    destination.assert_connections(1);
    assert_eq!(http.stats().responses_by_protocol.http1, 3);
    assert_eq!(http.stats().responses_by_protocol.http2, 2);
    assert_eq!(http.stats().redirects_followed, 1);
}

#[cfg(unix)]
fn cpu_ms() -> f64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // CLOCK_PROCESS_CPUTIME_ID measures this test process, including its server.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) },
        0
    );
    time.tv_sec as f64 * 1000.0 + time.tv_nsec as f64 / 1_000_000.0
}
#[cfg(not(unix))]
fn cpu_ms() -> f64 {
    0.0
}

#[tokio::test]
#[ignore = "controlled loopback HTTP/1 vs HTTP/2 resource benchmark"]
async fn benchmark_warm_http2_multiplexing() {
    let mut samples = Vec::new();
    for trial in 0..12 {
        for h1 in if trial % 2 == 0 {
            [true, false]
        } else {
            [false, true]
        } {
            let site = Site::new(true).await;
            let http = site.client(h1, 4096, 4, true);
            assert_eq!(http.get(site.url("/warm")).await.unwrap().bytes, PAYLOAD);
            let cpu = cpu_ms();
            let start = Instant::now();
            batch(&site, &http, "/bench").await;
            let wall = start.elapsed().as_secs_f64() * 1000.0;
            let cpu = cpu_ms() - cpu;
            site.assert_connections(if h1 { 4 } else { 1 });
            samples.push(serde_json::json!({"trial":trial,"protocol":if h1 {"http1"} else {"http2"},"wall_ms":wall,"cpu_ms":cpu,"tcp_connections":site.state.connections.load(Ordering::SeqCst),"tls_handshakes":site.state.handshakes.load(Ordering::SeqCst),"peak_requests":site.state.peak.load(Ordering::SeqCst),"verified_responses":9,"requests_in_batch":8,"origin_limit":4,"server_delay_ms":20}));
        }
    }
    let report = serde_json::json!({"scope":"loopback fixture; process CPU includes client and server; warm client, connections added during batch included; no public-web latency claim","samples":samples});
    if let Some(path) = std::env::var_os("FFETCH_HTTP2_BENCH_OUTPUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    println!("{report}");
}
