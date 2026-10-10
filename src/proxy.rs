use axum::extract::ws::{Message as ClientMsg, WebSocket as ClientWs};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as ServerMsg;
use tracing::debug;

pub async fn bridge_websockets(
    client_ws: ClientWs,
    server_ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let (mut client_tx, mut client_rx) = client_ws.split();
    let (mut server_tx, mut server_rx) = server_ws.split();

    let client_to_server = async {
        while let Some(Ok(msg)) = client_rx.next().await {
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
        while let Some(Ok(msg)) = server_rx.next().await {
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
