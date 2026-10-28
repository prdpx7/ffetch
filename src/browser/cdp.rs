//! Continuous CDP dispatch. Command replies and bounded navigation notifications
//! have separate routes, so idle periods and noisy subresources cannot stall IO.
use anyhow::{Context, Result, anyhow, ensure};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::Instant,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};

pub const EVENT_COUNT: usize = 128;
pub const EVENT_BYTES: usize = 4 * 1024 * 1024;

struct Request {
    message: Value,
    deadline: Instant,
    reply: oneshot::Sender<Result<Value, String>>,
}

struct Event {
    value: Value,
    bytes: usize,
}

pub struct Cdp {
    commands: mpsc::Sender<Request>,
    events: mpsc::Receiver<Event>,
    event_bytes: Arc<AtomicUsize>,
    failure: watch::Receiver<Option<String>>,
    driver: JoinHandle<()>,
    next_id: u64,
    timeout: Duration,
    pub session: Option<String>,
}

impl Drop for Cdp {
    fn drop(&mut self) {
        // Aborting also drops the socket and every pending reply. A cancelled
        // operation never leaves a detached WebSocket task running.
        self.driver.abort();
    }
}

impl Cdp {
    pub async fn connect(endpoint: &str, timeout: Duration, max_bytes: usize) -> Result<Self> {
        let url = url::Url::parse(endpoint)?;
        ensure!(
            url.scheme() == "ws"
                && url.host_str().is_some_and(|h| h == "localhost"
                    || h.parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())),
            "managed browser endpoint must be a loopback WebSocket"
        );
        let config = WebSocketConfig::default()
            .max_message_size(Some(
                max_bytes.saturating_mul(6).saturating_add(1024 * 1024),
            ))
            .max_frame_size(Some(
                max_bytes.saturating_mul(6).saturating_add(1024 * 1024),
            ));
        let (socket, _) = tokio::time::timeout(
            timeout,
            tokio_tungstenite::connect_async_with_config(endpoint, Some(config), false),
        )
        .await
        .context("browser connection deadline exceeded")??;
        let (commands, requests) = mpsc::channel(8);
        let (event_sender, events) = mpsc::channel(EVENT_COUNT);
        let (failure_sender, failure) = watch::channel(None);
        let event_bytes = Arc::new(AtomicUsize::new(0));
        let driver = tokio::spawn(dispatch(
            socket,
            requests,
            event_sender,
            event_bytes.clone(),
            max_bytes.clamp(64 * 1024, EVENT_BYTES),
            failure_sender,
        ));
        Ok(Self {
            commands,
            events,
            event_bytes,
            failure,
            driver,
            next_id: 0,
            timeout,
            session: None,
        })
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({"id":id,"method":method,"params":params});
        if let Some(session) = &self.session {
            message["sessionId"] = json!(session);
        }
        let deadline = Instant::now() + self.timeout;
        tokio::time::timeout_at(deadline, async {
            self.check_connection()?;
            let (reply, response) = oneshot::channel();
            self.commands
                .send(Request {
                    message,
                    deadline,
                    reply,
                })
                .await
                .map_err(|_| self.disconnected())?;
            let mut value = response
                .await
                .map_err(|_| self.disconnected())?
                .map_err(|error| anyhow!(error))?;
            if !value["error"].is_null() {
                return Err(anyhow!("{method}: {}", value["error"]));
            }
            Ok(value["result"].take())
        })
        .await
        .with_context(|| format!("browser command deadline exceeded: {method}"))?
    }

    fn disconnected(&self) -> anyhow::Error {
        anyhow!(
            self.failure
                .borrow()
                .as_deref()
                .unwrap_or("browser dispatcher disconnected")
                .to_owned()
        )
    }

    fn check_connection(&self) -> Result<()> {
        if self.failure.borrow().is_some() {
            return Err(self.disconnected());
        }
        Ok(())
    }

    fn consume_event(&self, event: Event) -> Result<Option<Value>> {
        self.event_bytes.fetch_sub(event.bytes, Ordering::Relaxed);
        let selected = event.value["sessionId"].as_str() == self.session.as_deref();
        if event.value["method"] == "Inspector.detached" && selected
            || event.value["method"] == "Target.detachedFromTarget"
                && self.session.is_some()
                && event.value["params"]["sessionId"].as_str() == self.session.as_deref()
        {
            anyhow::bail!(
                "browser tab detached or closed; inspect the session before retrying an action"
            );
        }
        Ok(selected.then_some(event.value))
    }

    pub fn drain_events(&mut self) -> Result<Vec<Value>> {
        self.check_connection()?;
        let mut result = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            if let Some(event) = self.consume_event(event)? {
                result.push(event);
            }
        }
        Ok(result)
    }

    pub async fn next_event(&mut self) -> Result<Value> {
        tokio::time::timeout(self.timeout, async {
            loop {
                self.check_connection()?;
                let event = self
                    .events
                    .recv()
                    .await
                    .ok_or_else(|| self.disconnected())?;
                if let Some(event) = self.consume_event(event)? {
                    return Ok(event);
                }
            }
        })
        .await
        .context("browser navigation event deadline exceeded")?
    }

    pub async fn attach(&mut self, target: &str) -> Result<()> {
        self.session = None;
        let result = self
            .call(
                "Target.attachToTarget",
                json!({"targetId":target,"flatten":true}),
            )
            .await?;
        self.session = Some(
            result["sessionId"]
                .as_str()
                .context("missing CDP session ID")?
                .into(),
        );
        self.call("Page.enable", json!({})).await?;
        self.call(
            "Network.enable",
            json!({"maxTotalBufferSize":16777216,"maxResourceBufferSize":4194304}),
        )
        .await?;
        Ok(())
    }

    pub async fn evaluate(&mut self, expression: &str, context: Option<i64>) -> Result<Value> {
        let mut params = json!({"expression":expression,"returnByValue":true,"awaitPromise":true});
        if let Some(context) = context {
            params["contextId"] = json!(context);
        }
        let result = self.call("Runtime.evaluate", params).await?;
        if !result["exceptionDetails"].is_null() {
            let description = result["exceptionDetails"]["exception"]["description"]
                .as_str()
                .or_else(|| result["exceptionDetails"]["text"].as_str())
                .unwrap_or("page evaluation failed");
            return Err(anyhow!("{description}"));
        }
        Ok(result["result"]["value"].clone())
    }

    pub async fn isolated_world(&mut self) -> Result<i64> {
        let tree = self.call("Page.getFrameTree", json!({})).await?;
        let frame = tree["frameTree"]["frame"]["id"]
            .as_str()
            .context("missing main frame")?;
        let world = self
            .call(
                "Page.createIsolatedWorld",
                json!({"frameId":frame,"worldName":"ffetch-inspection"}),
            )
            .await?;
        world["executionContextId"]
            .as_i64()
            .context("missing isolated context")
    }
}

fn navigation_event(value: &Value) -> bool {
    matches!(
        value["method"].as_str(),
        Some(
            "Page.frameNavigated"
                | "Page.navigatedWithinDocument"
                | "Inspector.detached"
                | "Target.detachedFromTarget"
        )
    ) || value["method"] == "Network.responseReceived" && value["params"]["type"] == "Document"
}

async fn dispatch(
    mut socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut requests: mpsc::Receiver<Request>,
    events: mpsc::Sender<Event>,
    event_bytes: Arc<AtomicUsize>,
    event_limit: usize,
    failure: watch::Sender<Option<String>>,
) {
    let mut pending: HashMap<u64, oneshot::Sender<Result<Value, String>>> = HashMap::new();
    let result: Result<()> = async {
        loop {
            // A timed-out/cancelled caller drops its receiver. Late replies are
            // ignored, and cancelled queued commands are never dispatched.
            pending.retain(|_,reply|!reply.is_closed());
            tokio::select! {
                request = requests.recv() => {
                    let request = request.context("browser command channel closed")?;
                    if request.reply.is_closed() || Instant::now() >= request.deadline {continue;}
                    let id = request.message["id"].as_u64().context("missing browser command ID")?;
                    pending.insert(id,request.reply);
                    tokio::time::timeout_at(request.deadline,socket.send(Message::Text(request.message.to_string().into())))
                        .await.context("browser command write deadline exceeded")??;
                }
                message = socket.next() => {
                    match message.context("browser disconnected")?? {
                        Message::Text(text) => {
                            let value: Value = serde_json::from_str(&text)?;
                            if let Some(id) = value["id"].as_u64() {
                                if let Some(reply) = pending.remove(&id) { let _ = reply.send(Ok(value)); }
                            } else if navigation_event(&value) {
                                let bytes = text.len();
                                ensure!(bytes <= event_limit.saturating_sub(event_bytes.load(Ordering::Relaxed)),
                                    "browser navigation event byte budget exceeded; no navigation status was silently discarded");
                                event_bytes.fetch_add(bytes,Ordering::Relaxed);
                                if events.try_send(Event{value,bytes}).is_err() {
                                    event_bytes.fetch_sub(bytes,Ordering::Relaxed);
                                    anyhow::bail!("browser navigation event queue overflow; narrow the operation or retry with another source");
                                }
                            }
                        }
                        Message::Ping(bytes) => socket.send(Message::Pong(bytes)).await?,
                        Message::Close(_) => anyhow::bail!("browser disconnected"),
                        _ => (),
                    }
                }
            }
        }
    }.await;
    let reason = result
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_else(|| "browser dispatcher stopped".into());
    let _ = failure.send(Some(reason.clone()));
    for (_, reply) in pending {
        let _ = reply.send(Err(reason.clone()));
    }
}
