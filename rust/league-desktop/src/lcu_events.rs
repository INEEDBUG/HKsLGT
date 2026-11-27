//! Authenticated, certificate-pinned LCU event notifications used to wake the normal monitor.

use crate::{client_discovery::LocalClient, lcu::LcuTransport};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;

const EVENT_TOPIC: &str = "OnJsonApiEvent";
const GAMEFLOW_PHASE: &str = "/lol-gameflow/v1/gameflow-phase";
const READY_CHECK: &str = "/lol-matchmaking/v1/ready-check";
const CHAMP_SELECT: &str = "/lol-champ-select/v1/session";
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const PROCESS_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const BENCH_EVENT_COALESCE: Duration = Duration::from_millis(50);

pub(super) async fn watch_ready_events(
    client: Arc<LocalClient>,
    transport: LcuTransport,
    wake: Arc<Notify>,
    rounds: Arc<Mutex<ReadyEventDeduper>>,
    selection_wake_enabled: Arc<AtomicBool>,
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

        let mut bench_deadline = None;
        loop {
            let deadline = bench_deadline
                .unwrap_or_else(|| tokio::time::Instant::now() + PROCESS_CHECK_INTERVAL);
            let result = tokio::time::timeout_at(deadline, socket.next()).await;
            let message = match result {
                Err(_) if bench_deadline.is_some() => {
                    bench_deadline = None;
                    if !client.alive() {
                        return;
                    }
                    if selection_wake_enabled.load(Ordering::Acquire) {
                        wake.notify_one();
                    }
                    continue;
                }
                Err(_) if !client.alive() => return,
                Err(_) => continue,
                Ok(Some(Ok(message))) => message,
                Ok(_) => break,
            };
            match message {
                Message::Text(text) => {
                    let signal = rounds
                        .lock()
                        .ok()
                        .and_then(|mut state| state.observe(&text));
                    match signal {
                        Some(EventWake::Immediate) => {
                            bench_deadline = None;
                            wake.notify_one();
                        }
                        Some(EventWake::BenchChanged)
                            if selection_wake_enabled.load(Ordering::Acquire) =>
                        {
                            if bench_deadline.is_none() {
                                bench_deadline =
                                    Some(tokio::time::Instant::now() + BENCH_EVENT_COALESCE);
                            }
                        }
                        _ => {}
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
        if let Ok(mut state) = rounds.lock() {
            state.socket_lost();
        }
        if client.alive() {
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ReadyRound {
    pub(super) number: u64,
    pub(super) closed: bool,
}

#[derive(Default)]
pub(super) struct ReadyEventDeduper {
    phase: Option<String>,
    ready_pending: bool,
    seen_ready: bool,
    reopen_after_disconnect: bool,
    round: ReadyRound,
    bench: Option<Vec<u32>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventWake {
    Immediate,
    BenchChanged,
}

impl ReadyEventDeduper {
    pub(super) fn current(&self) -> ReadyRound {
        self.round
    }

    fn socket_lost(&mut self) {
        self.phase = None;
        self.ready_pending = false;
        self.reopen_after_disconnect = self.round.closed;
        self.bench = None;
    }

    pub(super) fn seed_bench(&mut self, bench: Option<Vec<u32>>) {
        self.bench = bench;
    }

    pub(super) fn confirm_http(&mut self, started_in: ReadyRound) -> bool {
        if self.round != started_in || (self.round.closed && !self.reopen_after_disconnect) {
            return false;
        }
        self.enter();
        true
    }

    fn enter(&mut self) {
        if self.round.closed {
            self.round.number = self.round.number.wrapping_add(1);
            self.round.closed = false;
        }
        self.reopen_after_disconnect = false;
        self.seen_ready = true;
    }

    fn leave(&mut self) -> bool {
        let changed = !self.round.closed;
        self.round.closed = true;
        self.reopen_after_disconnect = false;
        self.ready_pending = false;
        changed
    }

    #[cfg(test)]
    fn should_wake(&mut self, text: &str) -> bool {
        self.observe(text).is_some()
    }

    fn observe(&mut self, text: &str) -> Option<EventWake> {
        let Some(signal) = parse_event(text) else {
            return None;
        };
        match signal {
            EventSignal::Phase(phase) => {
                let changed = self.phase.as_deref() != Some(phase.as_str());
                if !changed {
                    return None;
                }
                let wake = match phase.as_str() {
                    "ReadyCheck" => {
                        self.enter();
                        true
                    }
                    other => (self.seen_ready && self.leave()) || other == "ChampSelect",
                };
                self.phase = Some(phase);
                wake.then_some(EventWake::Immediate)
            }
            EventSignal::Closed => self.leave().then_some(EventWake::Immediate),
            EventSignal::Response { pending, created } => {
                // A resource recreation can occur without an intervening phase poll.
                // Duplicate Create/Update on the same open resource consumes no new round.
                if created || (!self.round.closed && pending) {
                    self.enter();
                }
                if self.round.closed {
                    return None;
                }
                let changed = self.ready_pending != pending;
                self.ready_pending = pending;
                (changed && pending).then_some(EventWake::Immediate)
            }
            EventSignal::Bench(bench) => {
                if self.bench.as_ref() == Some(&bench) {
                    return None;
                }
                let known = self.bench.replace(bench).is_some();
                known.then_some(EventWake::BenchChanged)
            }
        }
    }
}

enum EventSignal {
    Phase(String),
    Closed,
    Response { pending: bool, created: bool },
    Bench(Vec<u32>),
}

fn parse_event(text: &str) -> Option<EventSignal> {
    if !text.contains(GAMEFLOW_PHASE) && !text.contains(READY_CHECK) && !text.contains(CHAMP_SELECT)
    {
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
    let uri = payload.get("uri")?.as_str()?;
    let event_type = payload.get("eventType")?.as_str()?;
    if event != EVENT_TOPIC {
        return None;
    }
    if uri == READY_CHECK && event_type == "Delete" {
        return Some(EventSignal::Closed);
    }
    if uri == CHAMP_SELECT && event_type == "Delete" {
        return Some(EventSignal::Bench(Vec::new()));
    }
    if !matches!(event_type, "Create" | "Update") {
        return None;
    }
    let data = payload.get("data")?;
    match (event, uri) {
        (EVENT_TOPIC, GAMEFLOW_PHASE) => {
            data.as_str().map(|phase| EventSignal::Phase(phase.into()))
        }
        (EVENT_TOPIC, READY_CHECK) => match data.get("state").and_then(Value::as_str)? {
            "Invalid" | "EveryoneReady" => Some(EventSignal::Closed),
            "InProgress" => Some(EventSignal::Response {
                pending: data.get("playerResponse")?.as_str()? == "None",
                created: event_type == "Create",
            }),
            _ => None,
        },
        (EVENT_TOPIC, CHAMP_SELECT) => bench_signature(data).map(EventSignal::Bench),
        _ => None,
    }
}

fn bench_signature(data: &Value) -> Option<Vec<u32>> {
    let champions = data.get("benchChampions")?.as_array()?;
    if champions.len() > 20 {
        return None;
    }
    let mut ids = Vec::with_capacity(champions.len());
    for champion in champions {
        let Some(id) = champion
            .get("championId")
            .and_then(Value::as_u64)
            .and_then(|id| u32::try_from(id).ok())
            .filter(|id| (1..100_000).contains(id))
        else {
            continue;
        };
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    (champions.is_empty() || !ids.is_empty()).then_some(ids)
}

#[cfg(test)]
mod tests {
    use super::{ReadyEventDeduper, GAMEFLOW_PHASE, READY_CHECK};

    const CHAMP_SELECT: &str = "/lol-champ-select/v1/session";

    fn event(uri: &str, data: serde_json::Value) -> String {
        serde_json::json!([8,"OnJsonApiEvent",{"uri":uri,"eventType":"Update","data":data}])
            .to_string()
    }

    fn champ_select_event(event_type: &str, data: serde_json::Value) -> String {
        serde_json::json!([8,"OnJsonApiEvent",{"uri":CHAMP_SELECT,"eventType":event_type,"data":data}])
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
        assert!(
            !deduper.should_wake(&pending),
            "a stale pending update after ChampSelect is not a new resource"
        );
        assert!(deduper.should_wake(&ready_phase));
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
            r#"[5,"/lol-gameflow/v1/gameflow-phase"]"#,
            "not json",
        ] {
            assert!(
                !deduper.should_wake(ignored),
                "unexpected wake for {ignored}"
            );
        }
    }

    #[test]
    fn coalesced_close_and_reopen_identify_one_new_round_without_an_id() {
        let mut state = ReadyEventDeduper::default();
        let create = r#"[8,"OnJsonApiEvent",{"uri":"/lol-matchmaking/v1/ready-check","eventType":"Create","data":{"state":"InProgress","playerResponse":"None"}}]"#;
        let delete = r#"[8,"OnJsonApiEvent",{"uri":"/lol-matchmaking/v1/ready-check","eventType":"Delete","data":null}]"#;
        let answered = event(
            READY_CHECK,
            serde_json::json!({"state":"InProgress","playerResponse":"Accepted"}),
        );
        assert!(state.should_wake(create));
        let first = state.current();
        assert!(!state.should_wake(create));
        assert!(!state.should_wake(&answered));
        assert!(state.should_wake(delete));
        assert!(state.current().closed);
        assert!(!state.should_wake(delete));
        assert!(state.should_wake(create));
        let second = state.current();
        assert!(!second.closed);
        assert_ne!(second.number, first.number);
        assert!(!state.should_wake(create));
        assert_eq!(state.current().number, second.number);
    }

    #[test]
    fn reconnect_can_recover_an_ended_round_but_does_not_rearm_an_open_one() {
        let mut state = ReadyEventDeduper::default();
        let first = state.current();
        assert!(state.confirm_http(first));
        state.socket_lost();
        assert!(state.confirm_http(first));
        assert!(state.current() == first);
        assert!(state.leave());
        let closed = state.current();
        assert!(!state.confirm_http(first));
        assert!(!state.confirm_http(closed));
        state.socket_lost();
        assert!(state.confirm_http(closed));
        let second = state.current();
        assert!(!second.closed);
        assert_ne!(second.number, first.number);
        assert!(!state.confirm_http(closed));
        assert!(state.confirm_http(second));
        assert!(state.current() == second);
    }

    #[test]
    fn only_real_bench_id_sequence_changes_wake_for_selection_updates() {
        let mut state = ReadyEventDeduper::default();
        let session = |bench: serde_json::Value, remaining: u64| {
            serde_json::json!({
                "benchChampions": bench,
                "timer": {"adjustedTimeLeftInPhase": remaining},
                "myTeam": [{"cellId": 0, "championId": 22}]
            })
        };

        let initial = champ_select_event("Create", session(serde_json::json!([]), 20_000));
        assert!(
            !state.should_wake(&initial),
            "initial empty bench is not a change"
        );

        let added = champ_select_event(
            "Update",
            session(
                serde_json::json!([
                    {"championId": 103},
                    {"championId": 0},
                    {"championId": 103}
                ]),
                19_500,
            ),
        );
        assert!(
            state.should_wake(&added),
            "a newly benched champion wakes the monitor"
        );
        assert!(
            !state.should_wake(&champ_select_event(
                "Update",
                session(serde_json::json!([{"championId": 103}]), 19_000),
            )),
            "zero placeholders and duplicates do not change the visible ID sequence"
        );

        let timer_only = champ_select_event(
            "Update",
            session(serde_json::json!([{"championId": 103}]), 18_000),
        );
        assert!(
            !state.should_wake(&timer_only),
            "timer-only updates do not wake the monitor"
        );

        let reordered = champ_select_event(
            "Update",
            session(
                serde_json::json!([{"championId": 22}, {"championId": 103}]),
                17_000,
            ),
        );
        assert!(
            state.should_wake(&reordered),
            "bench ID order changes are observable"
        );

        let cleared = champ_select_event("Update", session(serde_json::json!([]), 16_000));
        assert!(
            state.should_wake(&cleared),
            "clearing the bench is observable"
        );
        assert!(
            !state.should_wake(&cleared),
            "duplicate clear updates are ignored"
        );
        assert!(state.should_wake(&champ_select_event(
            "Update",
            session(serde_json::json!([{"championId": 103}]), 15_000),
        )));
        assert!(state.should_wake(&champ_select_event("Delete", serde_json::Value::Null)));
        assert!(!state.should_wake(&champ_select_event("Delete", serde_json::Value::Null)));

        for ignored in [
            r#"[8,"wrong-topic",{"uri":"/lol-champ-select/v1/session","eventType":"Update","data":{"benchChampions":[{"championId":103}]}}]"#,
            r#"[8,"OnJsonApiEvent",{"uri":"/lol-champ-select/v1/session","eventType":"Delete","data":{"benchChampions":[{"championId":103}]}}]"#,
            r#"[8,"OnJsonApiEvent",{"uri":"/lol-champ-select/v1/session","eventType":"Update","data":{"benchChampions":"bad"}}]"#,
            r#"[8,"OnJsonApiEvent",{"uri":"/lol-champ-select/v1/session","eventType":"Update","data":{"benchChampions":[{"championId":0}]}}]"#,
        ] {
            assert!(
                !state.should_wake(ignored),
                "unexpected selection wake for {ignored}"
            );
        }
    }
}
