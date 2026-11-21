use super::*;
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use tokio::time::Instant;

#[derive(uniffi::Record)]
pub struct ChatMessage {
    pub id: String,
    pub text: String,
    pub author: String,
}
#[uniffi::export(callback_interface)]
pub trait LiveChatOutput: Send + Sync {
    fn connected(&self);
    fn messages(&self, rows: Vec<ChatMessage>);
}
fn state(v: &Value) -> Result<Value> {
    if !v.is_object()
        || serde_json::to_vec(v)
            .map_err(|_| SourceFailure::Unsupported)?
            .len()
            > 131072
    {
        return Err(SourceFailure::Unsupported);
    }
    Ok(v.clone())
}
#[uniffi::export]
impl SourceService {
    pub fn listen_live_chat(
        &self,
        source: String,
        work: String,
        request: Arc<Request>,
        output: Box<dyn LiveChatOutput>,
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(25);
        let call = || {
            Invocation::child(
                deadline.saturating_duration_since(Instant::now()),
                &request.cancelled,
            )
        };
        let manifest = self.catalog.manifest(&source, call())?;
        if manifest["capabilities"]["liveChat"] != true || manifest["category"] != "live" {
            return Err(SourceFailure::Unsupported);
        }
        let (_, raw) = self
            .catalog
            .invoke(&source, "liveChat", &[work.into()], call())?;
        let desc = downloads::checked(raw)?;
        let mut saved = state(&desc["state"])?;
        let epoch = self
            .routes
            .sessions
            .partition(&source, Purpose::Source)
            .map_err(|_| SourceFailure::Storage)?;
        let mut socket = self
            .routes
            .runtime
            .block_on(async {
                tokio::time::timeout_at(
                    deadline,
                    park_net::chat::Chat::open(
                        &self.routes.transport,
                        &epoch,
                        &manifest["chat"]["domains"],
                        &desc,
                        &request.cancelled,
                    ),
                )
                .await
            })
            .map_err(|_| SourceFailure::Unavailable)?
            .map_err(|_| SourceFailure::Unavailable)?;
        let tick = if desc["heartbeatIntervalMs"].is_null() {
            None
        } else {
            Some(Duration::from_millis(
                desc["heartbeatIntervalMs"]
                    .as_u64()
                    .filter(|n| (10000..=60000).contains(n))
                    .ok_or(SourceFailure::Unsupported)?,
            ))
        };
        let ws = desc["transport"] == "websocket";
        let interval = tick.unwrap_or(Duration::from_secs(30));
        let mut next_tick = Instant::now() + interval;
        let mut awaiting_pong = false;
        let mut last_input = Instant::now();
        let mut event = json!({"type":"open"});
        let mut connected = false;
        let mut order = VecDeque::new();
        let mut seen = HashSet::new();
        loop {
            if request.cancelled.is_cancelled() {
                return Err(SourceFailure::Cancelled);
            }
            match event["type"].as_str() {
                Some("heartbeat") => (),
                Some("pong") => awaiting_pong = false,
                Some("ping") => {
                    if awaiting_pong {
                        return Err(SourceFailure::Unavailable);
                    }
                    self.routes
                        .runtime
                        .block_on(socket.send(None, &request.cancelled))
                        .map_err(|_| SourceFailure::Unavailable)?;
                    awaiting_pong = true;
                }
                _ => {
                    let budget = if connected {
                        plugins::budget(&request)
                    } else {
                        call()
                    };
                    let (_, raw) =
                        self.catalog
                            .invoke(&source, "liveChatEvent", &[saved, event], budget)?;
                    let data = downloads::checked(raw)?;
                    if data["error"] == true {
                        return Err(SourceFailure::Unavailable);
                    }
                    saved = state(&data["state"])?;
                    if let Some(frames) = data["send"].as_array() {
                        if frames.len() > 10 {
                            return Err(SourceFailure::Unsupported);
                        }
                        for frame in frames {
                            let msg = frame.as_str().ok_or(SourceFailure::Unsupported)?;
                            self.routes
                                .runtime
                                .block_on(socket.send(Some(msg), &request.cancelled))
                                .map_err(|_| SourceFailure::Unavailable)?;
                        }
                    }
                    if request.cancelled.is_cancelled() {
                        return Err(SourceFailure::Cancelled);
                    }
                    if !connected && data["connected"] == true {
                        connected = true;
                        output.connected()
                    }
                    if let Some(rows) = data["messages"].as_array() {
                        if rows.len() > 100 {
                            return Err(SourceFailure::Unsupported);
                        }
                        let mut batch = Vec::new();
                        for row in rows {
                            let Some(id) = row["id"]
                                .as_str()
                                .filter(|s| !s.is_empty() && s.encode_utf16().count() <= 4096)
                            else {
                                continue;
                            };
                            let Some(text) = row["text"].as_str().filter(|s| {
                                !s.trim().is_empty() && s.encode_utf16().count() <= 2000
                            }) else {
                                continue;
                            };
                            if !seen.insert(id.to_owned()) {
                                continue;
                            }
                            order.push_back(id.to_owned());
                            if order.len() > 512 {
                                seen.remove(&order.pop_front().unwrap());
                            }
                            if row["isHistory"] != true {
                                batch.push(ChatMessage {
                                    id: id.into(),
                                    text: text.into(),
                                    author: row["author"]
                                        .as_str()
                                        .filter(|s| s.encode_utf16().count() <= 100)
                                        .unwrap_or("")
                                        .into(),
                                })
                            }
                        }
                        if !batch.is_empty() {
                            output.messages(batch)
                        }
                    }
                }
            }
            let timer = if !connected {
                Some(deadline)
            } else if ws {
                Some(next_tick)
            } else {
                let expiry = last_input + Duration::from_secs(90);
                Some(if tick.is_some() {
                    next_tick.min(expiry)
                } else {
                    expiry
                })
            };
            event = self.routes.runtime.block_on(async {
                match tokio::time::timeout_at(timer.unwrap(), socket.next(&request.cancelled)).await
                {
                    Ok(Ok(v)) => {
                        last_input = Instant::now();
                        Ok(v)
                    }
                    Ok(Err(park_net::Error::Cancelled)) => Err(SourceFailure::Cancelled),
                    Ok(Err(_)) => Err(SourceFailure::Unavailable),
                    Err(_)
                        if !connected || !ws && last_input.elapsed() >= Duration::from_secs(90) =>
                    {
                        Err(SourceFailure::Unavailable)
                    }
                    Err(_) => {
                        next_tick = Instant::now() + interval;
                        Ok(json!({"type":if tick.is_some(){"tick"}else{"ping"}}))
                    }
                }
            })?;
        }
    }
}
