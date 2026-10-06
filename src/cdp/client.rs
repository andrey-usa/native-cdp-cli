//! Minimal async CDP JSON-RPC client over a single websocket.
//!
//! One background task owns both halves of the socket: it forwards queued
//! outgoing messages and dispatches incoming ones — `{id, result|error}` to
//! the pending caller, everything else to the event broadcast. `send` is a
//! plain request/response with a timeout; `subscribe` + `wait_event` cover
//! the few events we need (page load).

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};

type Responder = oneshot::Sender<Result<Value, String>>;

pub struct CdpClient {
    inner: Arc<Inner>,
}

struct Inner {
    next_id: AtomicU64,
    /// Shared with the pump task so responses dispatched from the read loop
    /// reach the `send` caller: every `send` inserts its responder here and the
    /// pump removes it in `dispatch`.
    pending: Arc<Mutex<HashMap<u64, Responder>>>,
    /// Outgoing JSON texts for the pump task. `shutdown` takes it, which
    /// makes the pump's `rx.recv()` return `None` and the task exits,
    /// closing the websocket.
    writer: Mutex<Option<mpsc::UnboundedSender<String>>>,
    events: broadcast::Sender<Value>,
}

impl Clone for CdpClient {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl CdpClient {
    /// Connect to the browser-level debugger URL and start the pump task.
    pub async fn connect(ws_url: &str) -> Result<Self> {
        // Bound the connect so a stale/unreachable DevTools URL fails in 15s
        // instead of hanging the CLI forever.
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(ws_url),
        )
        .await
        .map_err(|_| anyhow::anyhow!("CDP websocket connect to {ws_url} timed out"))?
        .with_context(|| format!("CDP websocket connect to {ws_url}"))?;
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let (event_tx, _) = broadcast::channel::<Value>(512);
        let pending: Arc<Mutex<HashMap<u64, Responder>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_pump = Arc::clone(&pending);
        let event_tx_pump = event_tx.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    outgoing = rx.recv() => {
                        match outgoing {
                            Some(text) => {
                                let msg = tokio_tungstenite::tungstenite::Message::Text(text.into());
                                if sink.send(msg).await.is_err() {
                                    break;
                                }
                            }
                            None => break, // client dropped: shut down
                        }
                    }
                    incoming = stream.next() => {
                        match incoming {
                            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                                dispatch(&text, &pending_pump, &event_tx_pump);
                            }
                            Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => break,
                            Some(Ok(_)) => {} // ping/pong/binary: ignore
                            Some(Err(_)) => break,
                        }
                    }
                }
            }
            // Connection lost: fail everything still pending.
            let mut pending = pending_pump.lock().unwrap();
            for (_, responder) in pending.drain() {
                let _ = responder.send(Err("CDP connection closed".to_string()));
            }
        });

        Ok(Self {
            inner: Arc::new(Inner {
                next_id: AtomicU64::new(1),
                pending,
                writer: Mutex::new(Some(tx)),
                events: event_tx,
            }),
        })
    }

    /// Send one CDP command and await its `{result}` (or `{error}`).
    pub async fn send(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);

        let mut msg = serde_json::json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(session) = session_id {
            msg["sessionId"] = Value::String(session.to_string());
        }
        if let Some(writer) = self.inner.writer.lock().unwrap().as_ref() {
            if writer.send(msg.to_string()).is_err() {
                self.inner.pending.lock().unwrap().remove(&id);
                anyhow::bail!("CDP client is shut down");
            }
        } else {
            self.inner.pending.lock().unwrap().remove(&id);
            anyhow::bail!("CDP client is shut down");
        }

        let response = tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| anyhow::anyhow!("CDP command timed out: {method}"))?
            .map_err(|_| anyhow::anyhow!("CDP connection closed during {method}"))?;
        response.map_err(|e| anyhow::anyhow!("CDP error in {method}: {e}"))
    }

    /// Subscribe to CDP events (method-keyed messages without `id`).
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.inner.events.subscribe()
    }

    /// Wait for the first event matching `pred`.
    pub async fn wait_event(
        &self,
        mut pred: impl FnMut(&Value) -> bool,
        timeout: Duration,
    ) -> Result<Value> {
        let mut rx = self.subscribe();
        tokio::time::timeout(timeout, async {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if pred(&event) {
                            return Ok(event);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(e) => anyhow::bail!("CDP event stream ended: {e}"),
                }
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for CDP event"))?
    }

    /// Stop the pump task; the websocket closes when the task drops it.
    /// After this, `send` fails fast.
    pub fn shutdown(&self) {
        self.inner.writer.lock().unwrap().take();
    }
}

fn dispatch(
    text: &str,
    pending: &Mutex<HashMap<u64, Responder>>,
    events: &broadcast::Sender<Value>,
) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return;
    };
    if let Some(id) = value.get("id").and_then(Value::as_u64) {
        let responder = pending.lock().unwrap().remove(&id);
        if let Some(responder) = responder {
            if let Some(error) = value.get("error") {
                let _ = responder.send(Err(error.to_string()));
            } else {
                let result = value.get("result").cloned().unwrap_or(Value::Null);
                let _ = responder.send(Ok(result));
            }
        }
        return;
    }
    if value.get("method").and_then(Value::as_str).is_some() {
        let _ = events.send(value);
    }
}
