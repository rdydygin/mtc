//! Local checks of the live fixture transport, independent of Mattermost.
mod support;

use anyhow::Result;
use futures::{SinkExt as _, StreamExt as _};
use mattermost::{Client, Event};
use support::{Wire, connected, event};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn recording_proxy_preserves_http_status_body_auth_and_token_header() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let wire = Wire::start(format!("http://{}", listener.local_addr()?)).await?;
    let server = tokio::spawn(async move {
        let (mut socket, head) = loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
            }
            let head = String::from_utf8(head).unwrap().to_lowercase();
            // Local tooling probes new ports, as in accept_websocket below.
            // A HEAD probe must not consume the expected POST exchange.
            if head.starts_with("head / http/1.1\r\n") {
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                continue;
            }
            break (socket, head);
        };
        assert!(
            head.starts_with("post /api/v4/example?test=true http/1.1"),
            "{head}"
        );
        assert!(
            head.contains("authorization: bearer fixture-token"),
            "{head}"
        );
        let mut body = [0; 7];
        socket.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, b"request");
        socket.write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 8\r\nToken: returned-token\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"ok\":1}").await.unwrap();
    });
    // Exercise the probe path deterministically, even without local tooling.
    let probe = reqwest::Client::new()
        .head(format!("{}/", wire.url()))
        .send()
        .await?;
    assert_eq!(probe.status(), 200);
    let response = reqwest::Client::new()
        .post(format!("{}/api/v4/example?test=true", wire.url()))
        .bearer_auth("fixture-token")
        .body("request")
        .send()
        .await?;
    assert_eq!(response.status(), 201);
    assert_eq!(response.headers()["token"], "returned-token");
    assert_eq!(response.text().await?, "{\"ok\":1}");
    server.await?;
    let transcript = wire.dump();
    assert!(transcript.contains("request: request"), "{transcript}");
    assert!(transcript.contains("status: 201"), "{transcript}");
    assert!(transcript.contains("response: {\"ok\":1}"), "{transcript}");
    Ok(())
}

#[tokio::test]
async fn recording_proxy_preserves_ws_and_forces_actual_reconnect() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let wire = Wire::start(format!("http://{}", listener.local_addr()?)).await?;
    let server = tokio::spawn(async move {
        let mut socket = accept_websocket(&listener).await;
        socket
            .send(Message::text(r#"{"event":"hello","data":{}}"#))
            .await
            .unwrap();
        loop {
            let message = socket.next().await.unwrap().unwrap();
            if let Message::Text(s) = message {
                let json: serde_json::Value = serde_json::from_str(&s).unwrap();
                if json["action"] == "user_typing" {
                    assert_eq!(json["data"]["channel_id"], "fixture-channel");
                    break;
                }
            }
        }
        // Wait for the proxy to drop the connection before accepting a new one.
        while matches!(socket.next().await, Some(Ok(_))) {}
        let mut socket = accept_websocket(&listener).await;
        socket
            .send(Message::text(r#"{"event":"hello","data":{}}"#))
            .await
            .unwrap();
        while matches!(socket.next().await, Some(Ok(_))) {}
    });
    let client = Client::new_unverified(wire.url(), "fixture-token")?;
    let (stream, mut events) = connected(&client).await;
    stream.send_typing(&"fixture-channel".into(), None);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !wire.dump().contains("user_typing") {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    wire.disconnect();
    event(&mut events, "forced disconnect", |e| {
        matches!(e, Event::Disconnected)
    })
    .await;
    wire.reconnect();
    event(&mut events, "reconnected hello", |e| {
        matches!(e, Event::Connected)
    })
    .await;
    assert_eq!(
        wire.frames()
            .iter()
            .filter(|v| v["event"] == "hello")
            .count(),
        2
    );
    drop(stream);
    server.abort();
    Ok(())
}

// Local tooling can probe newly opened ports with HEAD /. Ignore non-upgrade
// requests rather than mistaking a probe for the expected reconnect handshake.
async fn accept_websocket(
    listener: &TcpListener,
) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
    loop {
        let (socket, _) = listener.accept().await.unwrap();
        if let Ok(socket) = tokio_tungstenite::accept_async(socket).await {
            return socket;
        }
    }
}
