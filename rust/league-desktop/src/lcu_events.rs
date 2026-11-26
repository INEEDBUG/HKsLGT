//! Authenticated, certificate-pinned LCU event notifications used to wake the normal monitor.

use crate::{client_discovery::LocalClient, lcu::LcuTransport};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;

const EVENT_TOPIC: &str = "OnJsonApiEvent";
const GAMEFLOW_PHASE: &str = "/lol-gameflow/v1/gameflow-phase";
const READY_CHECK: &str = "/lol-matchmaking/v1/ready-check";
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const PROCESS_CHECK_INTERVAL: Duration = Duration::from_secs(2);

pub(super) async fn watch_ready_events(
    client: Arc<LocalClient>,
    transport: LcuTransport,
    wake: Arc<Notify>,
) {
    while client.alive() {
        let target = match client.target() {
            Ok(target) => target,
            Err(_) => {
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        let Ok(mut socket) = transport.connect_ready_events(&target).await else {
            tokio::time::sleep(RECONNECT_DELAY).await;
            continue;
        };

        if socket
            .send(Message::Text(
                serde_json::json!([5, EVENT_TOPIC]).to_string().into(),
            ))
            .await
            .is_err()
        {
            tokio::time::sleep(RECONNECT_DELAY).await;
            continue;
        }

        let mut deduper = ReadyEventDeduper::default();
        loop {
            let message = match tokio::time::timeout(PROCESS_CHECK_INTERVAL, socket.next()).await {
                Err(_) if !client.alive() => return,
                Err(_) => continue,
                Ok(Some(Ok(message))) => message,
                Ok(_) => break,
            };
            match message {
                Message::Text(text) => {
                    if deduper.should_wake(&text) {
                        wake.notify_one();
                    }
                }
                Message::Ping(payload) => {
                    if socket.send(Message::Pong(payload)).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        if client.alive() {
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    }
}

#[derive(Default)]
struct ReadyEventDeduper {
    phase: Option<String>,
    ready_pending: bool,
}

impl ReadyEventDeduper {
    fn should_wake(&mut self, text: &str) -> bool {
        let Some(signal) = parse_event(text) else {
            return false;
        };
        match signal {
            EventSignal::Phase(phase) => {
                let changed = self.phase.as_deref() != Some(phase.as_str());
                self.phase = Some(phase.clone());
                changed && matches!(phase.as_str(), "ReadyCheck" | "ChampSelect")
            }
            EventSignal::ReadyCheckPending(pending) => {
                let changed = self.ready_pending != pending;
                self.ready_pending = pending;
                changed && pending
            }
        }
    }
}

enum EventSignal {
    Phase(String),
    ReadyCheckPending(bool),
}

fn parse_event(text: &str) -> Option<EventSignal> {
    if !text.contains(GAMEFLOW_PHASE) && !text.contains(READY_CHECK) {
        return None;
    }
    let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(text) else {
        return None;
    };
    if parts.first().and_then(Value::as_i64) != Some(8) {
        return None;
    }
    let (Some(event), Some(payload)) = (
        parts.get(1).and_then(Value::as_str),
        parts.get(2).and_then(Value::as_object),
    ) else {
        return None;
    };
    if event != EVENT_TOPIC
        || !matches!(
            payload.get("eventType").and_then(Value::as_str),
            Some("Create" | "Update")
        )
    {
        return None;
    }
    let Some(uri) = payload.get("uri").and_then(Value::as_str) else {
        return None;
    };
    let Some(data) = payload.get("data") else {
        return None;
    };
    match (event, uri) {
        (EVENT_TOPIC, GAMEFLOW_PHASE) => {
            data.as_str().map(|phase| EventSignal::Phase(phase.into()))
        }
        (EVENT_TOPIC, READY_CHECK) => Some(EventSignal::ReadyCheckPending(
            data.get("state").and_then(Value::as_str) == Some("InProgress")
                && data.get("playerResponse").and_then(Value::as_str) == Some("None"),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{GAMEFLOW_PHASE, READY_CHECK, ReadyEventDeduper};

    fn event(uri: &str, data: serde_json::Value) -> String {
        serde_json::json!([8,"OnJsonApiEvent",{"uri":uri,"eventType":"Update","data":data}])
            .to_string()
    }

    #[test]
    fn wakes_only_for_exact_ready_phase_or_pending_ready_check_events() {
        let mut deduper = ReadyEventDeduper::default();
        let ready_phase = event(GAMEFLOW_PHASE, serde_json::json!("ReadyCheck"));
        assert!(deduper.should_wake(&ready_phase));
        assert!(!deduper.should_wake(&ready_phase));
        assert!(deduper.should_wake(&event(GAMEFLOW_PHASE, serde_json::json!("ChampSelect"))));
        let pending = event(
            READY_CHECK,
            serde_json::json!({"state":"InProgress","playerResponse":"None"}),
        );
        assert!(deduper.should_wake(&pending));
        assert!(!deduper.should_wake(&pending));
        let mut created_event = ReadyEventDeduper::default();
        assert!(created_event.should_wake(
            r#"[8,"OnJsonApiEvent",{"uri":"/lol-matchmaking/v1/ready-check","eventType":"Create","data":{"state":"InProgress","playerResponse":"None"}}]"#
        ));
        assert!(!deduper.should_wake(&event(
            READY_CHECK,
            serde_json::json!({"state":"InProgress","playerResponse":"Accepted"})
        )));
        for ignored in [
            r#"[8,"OnJsonApiEvent",{"uri":"/lol-chat/v1/me","eventType":"Update","data":{"state":"InProgress","playerResponse":"None"}}]"#,
            r#"[8,"wrong-topic",{"uri":"/lol-gameflow/v1/gameflow-phase","eventType":"Update","data":"ReadyCheck"}]"#,
            r#"[8,"OnJsonApiEvent",{"uri":"/lol-gameflow/v1/gameflow-phase","eventType":"Delete","data":"ReadyCheck"}]"#,
            r#"[8,"OnJsonApiEvent",{"uri":"/lol-matchmaking/v1/ready-check","eventType":"Delete","data":{"state":"InProgress","playerResponse":"None"}}]"#,
            r#"[5,"/lol-gameflow/v1/gameflow-phase"]"#,
            "not json",
        ] {
            assert!(
                !deduper.should_wake(ignored),
                "unexpected wake for {ignored}"
            );
        }
    }
}
