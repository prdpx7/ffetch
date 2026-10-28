//! Minimal typed-envelope CDP connection. All browser logic stays in Rust; no
//! Node driver is started. Notifications are retained while awaiting responses.
use anyhow::{Context, Result, anyhow, ensure};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{collections::VecDeque, time::Duration};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};

pub struct Cdp {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    timeout: Duration,
    pub session: Option<String>,
    pub events: VecDeque<Value>,
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
        Ok(Self {
            socket,
            next_id: 0,
            timeout,
            session: None,
            events: VecDeque::new(),
        })
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({"id":id,"method":method,"params":params});
        if let Some(session) = &self.session {
            message["sessionId"] = json!(session);
        }
        let timeout = self.timeout;
        tokio::time::timeout(timeout, async {
            self.socket
                .send(Message::Text(message.to_string().into()))
                .await?;
            while let Some(message) = self.socket.next().await {
                let message = message?;
                match message {
                    Message::Text(text) => {
                        let value: Value = serde_json::from_str(&text)?;
                        if value["id"].as_u64() == Some(id) {
                            if !value["error"].is_null() {
                                return Err(anyhow!("{method}: {}", value["error"]));
                            }
                            return Ok(value["result"].clone());
                        }
                        if value.get("method").is_some() {
                            if self.events.len() >= 2048 {
                                return Err(anyhow!("browser event queue overflow; retry inspection with a narrower operation"));
                            }
                            self.events.push_back(value);
                        }
                    }
                    Message::Ping(bytes) => self.socket.send(Message::Pong(bytes)).await?,
                    Message::Close(_) => break,
                    _ => (),
                }
            }
            Err(anyhow!("browser disconnected during {method}"))
        })
        .await
        .with_context(|| format!("browser command deadline exceeded: {method}"))?
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
