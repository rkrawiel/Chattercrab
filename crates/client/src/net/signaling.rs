//! Signaling WebSocket task: one task per connection attempt.

use chattercrab_protocol::{ClientMsg, DisplayName, MAX_WS_MESSAGE_BYTES, ServerMsg};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::{
    CloseFrame, WebSocketConfig, frame::coding::CloseCode,
};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use super::rest::basic_authorization_value;

/// Capacity of the outgoing message queue of one connection.
pub const OUTGOING_CAPACITY: usize = 256;

/// Why a signaling connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    /// The server answered the upgrade with 404: the room does not exist.
    RoomNotFound,
    /// Network failure or server-side close.
    Failed(String),
}

/// Events from a signaling task to the backend, tagged with the connection generation.
#[derive(Debug, Clone)]
pub enum SignalEvent {
    /// A server message.
    Msg(u64, ServerMsg),
    /// The connection ended.
    Closed {
        /// Connection generation.
        generation: u64,
        /// Cause.
        reason: CloseReason,
    },
}

/// Handle to a running signaling connection. Dropping it closes the socket.
#[derive(Debug)]
pub struct SignalHandle {
    tx: mpsc::Sender<ClientMsg>,
    task: JoinHandle<()>,
}

impl SignalHandle {
    /// Queues a message; returns `false` if the queue is full or the task ended.
    pub fn send(&self, msg: ClientMsg) -> bool {
        self.tx.try_send(msg).is_ok()
    }
}

impl Drop for SignalHandle {
    fn drop(&mut self) {
        // Dropping `tx` lets the task send a close frame; abort is the backstop if it
        // is stuck in connect.
        if let (false, Ok(rt)) = (
            self.task.is_finished(),
            tokio::runtime::Handle::try_current(),
        ) {
            let task = self.task.abort_handle();
            rt.spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                task.abort();
            });
        }
    }
}

/// Connects to `url`, sends `join`, and pumps messages until the socket closes.
pub fn spawn(
    generation: u64,
    url: String,
    display_name: DisplayName,
    auth_user: String,
    password: String,
    events: mpsc::Sender<SignalEvent>,
) -> SignalHandle {
    let (tx, rx) = mpsc::channel(OUTGOING_CAPACITY);
    let task = tokio::spawn(async move {
        let reason = run(
            generation,
            &url,
            display_name,
            &auth_user,
            &password,
            rx,
            &events,
        )
        .await;
        if let Some(reason) = reason {
            let _ = events
                .send(SignalEvent::Closed { generation, reason })
                .await;
        }
    });
    SignalHandle { tx, task }
}

/// Returns `None` when the backend asked to close (nothing to report).
async fn run(
    generation: u64,
    url: &str,
    display_name: DisplayName,
    auth_user: &str,
    password: &str,
    mut outgoing: mpsc::Receiver<ClientMsg>,
    events: &mpsc::Sender<SignalEvent>,
) -> Option<CloseReason> {
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(MAX_WS_MESSAGE_BYTES);
    config.max_frame_size = Some(MAX_WS_MESSAGE_BYTES);
    let request = match client_request(url, auth_user, password) {
        Ok(request) => request,
        Err(reason) => return Some(reason),
    };
    let connect = tokio_tungstenite::connect_async_with_config(request, Some(config), false);
    let (ws, _response) = match connect.await {
        Ok(ok) => ok,
        Err(WsError::Http(resp)) if resp.status().as_u16() == 401 => {
            return Some(CloseReason::Failed("authentication failed".to_owned()));
        }
        Err(WsError::Http(resp)) if resp.status().as_u16() == 404 => {
            return Some(CloseReason::RoomNotFound);
        }
        Err(e) => return Some(CloseReason::Failed(format!("connect failed: {e}"))),
    };
    tracing::info!(generation, url, "signaling connected");
    let (mut sink, mut stream) = ws.split();

    if let Err(e) = send(&mut sink, &ClientMsg::Join { display_name }).await {
        return Some(CloseReason::Failed(e));
    }

    loop {
        tokio::select! {
            out = outgoing.recv() => match out {
                Some(msg) => {
                    if let Err(e) = send(&mut sink, &msg).await {
                        return Some(CloseReason::Failed(e));
                    }
                }
                None => {
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame {
                            code: CloseCode::Normal,
                            reason: "leaving".into(),
                        })))
                        .await;
                    let _ = sink.close().await;
                    return None;
                }
            },
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ServerMsg>(&text) {
                    Ok(msg) => {
                        if events.send(SignalEvent::Msg(generation, msg)).await.is_err() {
                            return None;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "ignoring malformed server message"),
                },
                Some(Ok(Message::Close(frame))) => {
                    let why = frame
                        .map(|f| format!("server closed: {} {}", u16::from(f.code), f.reason))
                        .unwrap_or_else(|| "server closed".to_owned());
                    return Some(CloseReason::Failed(why));
                }
                // Pings are answered by tungstenite; binary frames are not used.
                Some(Ok(_)) => {}
                Some(Err(e)) => return Some(CloseReason::Failed(format!("connection lost: {e}"))),
                None => return Some(CloseReason::Failed("connection lost".to_owned())),
            },
        }
    }
}

fn client_request(
    url: &str,
    auth_user: &str,
    password: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, CloseReason> {
    let mut request = url
        .into_client_request()
        .map_err(|e| CloseReason::Failed(format!("connect failed: {e}")))?;
    if !password.is_empty() {
        let value = basic_authorization_value(auth_user, password);
        let header = HeaderValue::from_str(&value).map_err(|_| {
            CloseReason::Failed("connect failed: invalid authorization header".to_owned())
        })?;
        request.headers_mut().insert(AUTHORIZATION, header);
    }
    Ok(request)
}

async fn send<S>(sink: &mut S, msg: &ClientMsg) -> Result<(), String>
where
    S: SinkExt<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let json = serde_json::to_string(msg).map_err(|e| e.to_string())?;
    sink.send(Message::text(json))
        .await
        .map_err(|e| format!("send failed: {e}"))
}
