//! WebSocket upgrade + per-peer task. Owns one `tokio::spawn` per connection; a single
//! `select!` loop drains hub-bound outbound and parses inbound frames.

use crate::auth::{validate_session, SharedAuthState};
use crate::claim_store::ClaimStore;
use crate::hub::{self, HubCmd, PeerUser};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRef, State};
use axum::response::IntoResponse;
use axum_extra::extract::CookieJar;
use futures_util::{SinkExt, StreamExt};
use protocol::{decode_client, encode_server, ClientMsg, ServerMsg};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct WsState {
    pub hub: mpsc::Sender<HubCmd>,
    pub auth: SharedAuthState,
    pub claim_store: Arc<ClaimStore>,
}

/// Allow auth route handlers (State<SharedAuthState>) to be added to a WsState router.
impl FromRef<WsState> for SharedAuthState {
    fn from_ref(s: &WsState) -> Self { Arc::clone(&s.auth) }
}

pub async fn upgrade(
    State(state): State<WsState>,
    jar: CookieJar,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let session = validate_session(&jar, &state.auth.cfg.jwt_secret);
    ws.on_upgrade(move |socket| handle(socket, state, session))
}

async fn handle(
    socket: WebSocket,
    state: WsState,
    session: Option<crate::auth::SessionClaims>,
) {
    let Some(joined) = hub::join(&state.hub).await else { return };
    let peer_id = joined.peer_id;
    let mut outbound = joined.outbound;

    let (mut sink, mut stream) = socket.split();

    macro_rules! send_msg {
        ($msg:expr) => {{
            let mut buf = Vec::new();
            encode_server(&$msg, &mut buf);
            if sink.send(Message::Binary(buf.into())).await.is_err() {
                let _ = state.hub.send(HubCmd::Leave { peer_id }).await;
                return;
            }
        }};
    }

    send_msg!(joined.hello);

    // Send AuthState. If authenticated, register with hub and load claim.
    if let Some(ref claims) = session {
        let _ = state.hub.send(HubCmd::AuthPeer {
            peer_id,
            user: PeerUser { uid: claims.uid, email_key: claims.sub.clone() },
        }).await;
        let (name, email, claim) = load_auth_info(&state, &claims.sub).await;
        send_msg!(ServerMsg::AuthState { uid: claims.uid, claim, name, email, providers: available_providers(&state) });
    } else {
        send_msg!(ServerMsg::AuthState { uid: 0, claim: None, name: String::new(), email: String::new(), providers: available_providers(&state) });
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
                    None => break,
                }
            }
            frame = stream.next() => {
                match frame {
                    Some(Ok(Message::Binary(b))) => match decode_client(&b) {
                        Ok(ClientMsg::ClaimCreate(coord)) => {
                            let _ = state.hub.send(HubCmd::ClaimCreate { peer_id, coord }).await;
                        }
                        Ok(ClientMsg::ClaimDelete) => {
                            let _ = state.hub.send(HubCmd::ClaimDelete { peer_id }).await;
                        }
                        Ok(msg) => {
                            if state.hub.send(HubCmd::Client { peer_id, msg }).await.is_err() {
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
                    Some(Ok(_)) => {}
                }
            }
        }
    }

    let _ = state.hub.send(HubCmd::Leave { peer_id }).await;
}

fn available_providers(state: &WsState) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = state.auth.active_provider_slugs()
        .filter_map(|slug| {
            let p = state.auth.cfg.oidc_providers.get(slug)?;
            Some((slug.clone(), p.display(slug).to_string()))
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

async fn load_auth_info(
    state: &WsState,
    email_key: &str,
) -> (String, String, Option<(i32, i32)>) {
    let user = match state.auth.user_store.get(email_key).await {
        Ok(Some(u)) => u,
        _ => return (String::new(), String::new(), None),
    };
    let claim = match state.claim_store.get(email_key).await {
        Ok(Some(c)) => Some((c.cx, c.cy)),
        _ => None,
    };
    (user.name, user.email, claim)
}
