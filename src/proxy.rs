use axum::extract::ws::{Message as ClientMsg, WebSocket as ClientWs};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as ServerMsg;
use tracing::debug;

use crate::idle::Activity;
use crate::recorder::ProxyRecordingFrame;
use std::sync::Arc;

pub async fn bridge_websockets(
    client_ws: ClientWs,
    server_ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    frame_tx: Option<tokio::sync::mpsc::UnboundedSender<ProxyRecordingFrame>>,
    // The session's idle clock, and whether this client is a mere observer.
    activity: Option<(Arc<Activity>, bool)>,
    initial_url: Option<String>,
    initial_title: Option<String>,
) {
    let (mut client_tx, mut client_rx) = client_ws.split();
    let (mut server_tx, mut server_rx) = server_ws.split();

    let client_to_server = async {
        while let Some(Ok(msg)) = client_rx.next().await {
            if let (Some((activity, observer)), ClientMsg::Text(text)) = (&activity, &msg) {
                activity.note_command(text.as_str(), *observer);
            }
            let server_msg = match msg {
                ClientMsg::Text(text) => ServerMsg::Text(text.as_str().into()),
                ClientMsg::Binary(bin) => ServerMsg::Binary(bin),
                ClientMsg::Ping(p) => ServerMsg::Ping(p),
                ClientMsg::Pong(p) => ServerMsg::Pong(p),
                ClientMsg::Close(frame) => {
                    let _ = server_tx
                        .send(ServerMsg::Close(frame.map(|f| {
                            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                                code: f.code.into(),
                                reason: f.reason.as_str().into(),
                            }
                        })))
                        .await;
                    break;
                }
            };
            if let Err(e) = server_tx.send(server_msg).await {
                debug!(error = %e, "Forwarding to backend failed");
                break;
            }
        }
    };

    let server_to_client = async {
        let mut current_url = initial_url;
        let current_title = initial_title;

        while let Some(Ok(msg)) = server_rx.next().await {
            // Tap into screencast frames and navigation events for recording
            if let (Some(ref tx), ServerMsg::Text(ref text)) = (&frame_tx, &msg) {
                let s = text.as_str();
                if s.contains("\"Page.screencastFrame\"") {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(s) {
                        if val.get("method").and_then(|v| v.as_str())
                            == Some("Page.screencastFrame")
                        {
                            if let Some(data) = val.pointer("/params/data").and_then(|v| v.as_str())
                            {
                                let _ = tx.send(ProxyRecordingFrame {
                                    url: current_url.clone(),
                                    title: current_title.clone(),
                                    data: data.to_string(),
                                });
                            }
                        }
                    }
                } else if s.contains("\"Page.frameNavigated\"") {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(s) {
                        if let Some(url) = val.pointer("/params/frame/url").and_then(|v| v.as_str())
                        {
                            if !url.is_empty() && url != "about:blank" {
                                current_url = Some(url.to_string());
                            }
                        }
                    }
                } else if s.contains("\"Page.navigatedWithinDocument\"") {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(s) {
                        if let Some(url) = val.pointer("/params/url").and_then(|v| v.as_str()) {
                            if !url.is_empty() && url != "about:blank" {
                                current_url = Some(url.to_string());
                            }
                        }
                    }
                }
            }

            let client_msg = match msg {
                ServerMsg::Text(text) => ClientMsg::Text(text.as_str().into()),
                ServerMsg::Binary(bin) => ClientMsg::Binary(bin),
                ServerMsg::Ping(p) => ClientMsg::Ping(p),
                ServerMsg::Pong(p) => ClientMsg::Pong(p),
                ServerMsg::Close(frame) => {
                    let _ = client_tx
                        .send(ClientMsg::Close(frame.map(|f| {
                            axum::extract::ws::CloseFrame {
                                code: f.code.into(),
                                reason: f.reason.as_str().into(),
                            }
                        })))
                        .await;
                    break;
                }
                ServerMsg::Frame(_) => continue,
            };
            if let Err(e) = client_tx.send(client_msg).await {
                debug!(error = %e, "Forwarding to client failed");
                break;
            }
        }
    };

    tokio::select! {
        _ = client_to_server => {},
        _ = server_to_client => {},
    }
}
