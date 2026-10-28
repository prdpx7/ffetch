//! Deterministic CDP transport checks without a browser installation.
use ffetch::browser::cdp::Cdp;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{future::Future, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

type Socket = WebSocketStream<TcpStream>;

async fn fixture<F, T>(serve: F) -> (Cdp, JoinHandle<()>)
where
    F: FnOnce(Socket) -> T + Send + 'static,
    T: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve(tokio_tungstenite::accept_async(stream).await.unwrap()).await;
    });
    (
        Cdp::connect(&endpoint, Duration::from_secs(2), 256 * 1024)
            .await
            .unwrap(),
        task,
    )
}

async fn receive(socket: &mut Socket) -> Value {
    loop {
        if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

async fn send(socket: &mut Socket, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

async fn stopped(task: JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("socket task leaked")
        .unwrap();
}

#[tokio::test]
async fn dispatches_idle_events_and_answers_ping_without_a_command() {
    let (acknowledged, received) = tokio::sync::oneshot::channel();
    let (mut cdp,server) = fixture(move |mut socket| async move {
        send(&mut socket,json!({"method":"Page.frameNavigated","params":{"frame":{"id":"main","loaderId":"new"}}})).await;
        socket.send(Message::Ping(vec![1,2,3].into())).await.unwrap();
        assert!(matches!(socket.next().await.unwrap().unwrap(), Message::Pong(bytes) if bytes.as_ref() == [1,2,3]));
        acknowledged.send(()).unwrap();
        let _ = socket.next().await;
    }).await;
    tokio::time::timeout(Duration::from_secs(1), received)
        .await
        .expect("idle socket was not being read")
        .unwrap();
    let events = cdp.drain_events().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["method"], "Page.frameNavigated");
    drop(cdp);
    stopped(server).await;
}

#[tokio::test]
async fn subresource_noise_cannot_overflow_navigation_events_or_hide_a_reply() {
    let (mut cdp,server) = fixture(|mut socket| async move {
        let command = receive(&mut socket).await;
        for _ in 0..5000 {
            send(&mut socket,json!({"method":"Network.dataReceived","params":{"requestId":"image","dataLength":1024},"sessionId":"selected"})).await;
        }
        send(&mut socket,json!({"method":"Network.responseReceived","params":{"type":"Document","frameId":"other","response":{"status":500}},"sessionId":"another-tab"})).await;
        send(&mut socket,json!({"method":"Network.responseReceived","params":{"type":"Document","frameId":"main","loaderId":"new","response":{"status":200}},"sessionId":"selected"})).await;
        send(&mut socket,json!({"id":command["id"],"result":{"product":"Chromium fixture"}})).await;
        let _ = socket.next().await;
    }).await;
    cdp.session = Some("selected".into());
    let result = cdp.call("Browser.getVersion", json!({})).await.unwrap();
    assert_eq!(result["product"], "Chromium fixture");
    let events = cdp.drain_events().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["params"]["response"]["status"], 200);
    drop(cdp);
    stopped(server).await;
}

#[tokio::test]
async fn cancelled_commands_are_not_replayed_and_late_replies_cannot_satisfy_the_next_call() {
    let (started, received) = tokio::sync::oneshot::channel();
    let (mut cdp, server) = fixture(move |mut socket| async move {
        let first = receive(&mut socket).await;
        assert_eq!(first["method"], "Input.insertText");
        started.send(()).unwrap();
        let second = receive(&mut socket).await;
        assert_eq!(second["method"], "Browser.getVersion");
        assert_ne!(first["id"], second["id"]);
        send(
            &mut socket,
            json!({"id":first["id"],"result":{"value":"late"}}),
        )
        .await;
        send(
            &mut socket,
            json!({"id":second["id"],"result":{"value":"current"}}),
        )
        .await;
        let _ = socket.next().await;
    })
    .await;
    {
        let call = cdp.call("Input.insertText", json!({"text":"exactly once"}));
        tokio::pin!(call);
        tokio::select! {
            result = &mut call => panic!("unexpected early response: {result:?}"),
            result = received => result.unwrap(),
        }
        // Dropping this future cancels observation. The already dispatched
        // action is uncertain and must not be automatically sent again.
    }
    let result = cdp.call("Browser.getVersion", json!({})).await.unwrap();
    assert_eq!(result["value"], "current");
    drop(cdp);
    stopped(server).await;
}

#[tokio::test]
async fn navigation_event_count_and_byte_limits_fail_explicitly() {
    for large in [false, true] {
        let (mut cdp,server) = fixture(move |mut socket| async move {
            let _ = receive(&mut socket).await;
            let event = json!({"method":"Page.frameNavigated","params":{"frame":{"id":"main","name":if large {"x".repeat(150_000)}else {"small".into()}}}});
            for _ in 0..129 {
                if socket.send(Message::Text(event.to_string().into())).await.is_err() {break;}
            }
        }).await;
        let error = cdp
            .call("Page.navigate", json!({"url":"https://example.org/"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(if large {
                "event byte budget"
            } else {
                "event queue overflow"
            }),
            "{error}"
        );
        assert!(
            cdp.drain_events().is_err(),
            "dropped navigation events must not appear complete"
        );
        drop(cdp);
        stopped(server).await;
    }
}

#[tokio::test]
async fn disconnect_fails_pending_calls_without_waiting_for_their_deadline() {
    let (mut cdp, server) = fixture(|mut socket| async move {
        let _ = receive(&mut socket).await;
        socket.close(None).await.unwrap();
    })
    .await;
    cdp.set_timeout(Duration::from_secs(30));
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        cdp.call("Page.getFrameTree", json!({})),
    )
    .await
    .expect("disconnect did not wake caller");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("browser disconnected")
    );
    assert!(cdp.call("Browser.getVersion", json!({})).await.is_err());
    drop(cdp);
    stopped(server).await;
}

#[tokio::test]
async fn selected_tab_detachment_is_reported_without_a_silent_navigation_gap() {
    let (mut cdp, server) = fixture(|mut socket| async move {
        let command = receive(&mut socket).await;
        send(
            &mut socket,
            json!({"method":"Target.detachedFromTarget","params":{"sessionId":"selected"}}),
        )
        .await;
        send(&mut socket, json!({"id":command["id"],"result":{}})).await;
        let _ = socket.next().await;
    })
    .await;
    cdp.session = Some("selected".into());
    cdp.call("Browser.getVersion", json!({})).await.unwrap();
    let error = cdp.drain_events().unwrap_err().to_string();
    assert!(error.contains("browser tab detached or closed"), "{error}");
    drop(cdp);
    stopped(server).await;
}
