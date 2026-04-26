//! WebSocket upgrade + per-peer task. Owns one `tokio::spawn` per connection; a single
//! `select!` loop drains hub-bound outbound and parses inbound frames.

use crate::hub::{self, HubCmd};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use protocol::{decode_client, encode_server};
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct WsState {
    pub hub: mpsc::Sender<HubCmd>,
}

pub async fn upgrade(State(state): State<WsState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle(socket, state.hub))
}

async fn handle(socket: WebSocket, hub_tx: mpsc::Sender<HubCmd>) {
    let Some(joined) = hub::join(&hub_tx).await else {
        return;
    };
    let peer_id = joined.peer_id;
    let mut outbound = joined.outbound;

    let (mut sink, mut stream) = socket.split();

    // First send: Hello.
    let mut buf = Vec::new();
    encode_server(&joined.hello, &mut buf);
    if sink.send(Message::Binary(buf.into())).await.is_err() {
        let _ = hub_tx.send(HubCmd::Leave { peer_id }).await;
        return;
    }

    loop {
        tokio::select! {
            msg = outbound.recv() => {
                match msg {
                    Some(bytes) => {
                        if sink.feed(Message::Binary(bytes)).await.is_err() {
                            break;
                        }
                        let mut sink_err = false;
                        while let Ok(b) = outbound.try_recv() {
                            if sink.feed(Message::Binary(b)).await.is_err() {
                                sink_err = true;
                                break;
                            }
                        }
                        if sink_err || sink.flush().await.is_err() {
                            break;
                        }
                    }
                    None => break, // hub dropped us
                }
            }
            frame = stream.next() => {
                match frame {
                    Some(Ok(Message::Binary(b))) => match decode_client(&b) {
                        Ok(msg) => {
                            if hub_tx.send(HubCmd::Client { peer_id, msg }).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(peer = peer_id, err = %e, "bad client frame; closing connection");
                            break;
                        }
                    },
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    Some(Ok(_)) => {} // ignore text/ping/pong
                }
            }
        }
    }

    let _ = hub_tx.send(HubCmd::Leave { peer_id }).await;
}
