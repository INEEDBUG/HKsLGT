//! Authenticated, certificate-pinned LCU event notifications used to wake the normal monitor.

use crate::{client_discovery::LocalClient, lcu::LcuTransport};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;

const EVENT_TOPIC: &str = "OnJsonApiEvent";
const GAMEFLOW_PHASE: &str = "/lol-gameflow/v1/gameflow-phase";
const READY_CHECK: &str = "/lol-matchmaking/v1/ready-check";
const CHAMP_SELECT: &str = "/lol-champ-select/v1/session";
const POST_GAME_BALLOT: &str = "/lol-honor-v2/v1/ballot/";
const POST_GAME_BALLOT_NO_SLASH: &str = "/lol-honor-v2/v1/ballot";
const POST_GAME_SEQUENCE: &str = "/lol-pre-end-of-game/v1/currentSequenceEvent";
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const PROCESS_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const BENCH_EVENT_COALESCE: Duration = Duration::from_millis(50);
const MAX_DRAFT_SESSION_BYTES: usize = 256 * 1024;

pub(super) async fn watch_ready_events(
    client: Arc<LocalClient>,
    transport: LcuTransport,
    wake: Arc<Notify>,
    rounds: Arc<Mutex<ReadyEventDeduper>>,
    selection_wake_enabled: Arc<AtomicBool>,
    post_game_wake_enabled: Arc<AtomicBool>,
    post_game_pending: Arc<AtomicBool>,
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
                    if let Some(signal) = signal {
                        let selection =
                            signal.selection && selection_wake_enabled.load(Ordering::Acquire);
                        let post_game =
                            signal.post_game && post_game_wake_enabled.load(Ordering::Acquire);
                        if post_game {
                            post_game_pending.store(true, Ordering::Release);
                        }
                        if (signal.immediate && (!signal.selection || selection)) || post_game {
                            bench_deadline = None;
                            wake.notify_one();
                        } else if signal.bench_changed && selection && bench_deadline.is_none() {
                            bench_deadline =
                                Some(tokio::time::Instant::now() + BENCH_EVENT_COALESCE);
                        }
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
        let cleared_draft = rounds
            .lock()
            .map(|mut state| state.socket_lost())
            .unwrap_or(false);
        if cleared_draft && selection_wake_enabled.load(Ordering::Acquire) {
            wake.notify_one();
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
    draft_session: Option<Arc<Value>>,
    draft_signature: Option<DraftSessionSignature>,
    draft_deadline: Option<Instant>,
    post_game_ballot: Option<u64>,
    post_game_sequence: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EventWake {
    immediate: bool,
    bench_changed: bool,
    post_game: bool,
    selection: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DraftSessionSignature {
    game_id: Option<u64>,
    queue_id: Option<u64>,
    local_player_cell_id: i64,
    player: Option<String>,
    champion_id: Option<u64>,
    champion_pick_intent: Option<u64>,
    timer_phase: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PostGameResource {
    Ballot,
    Sequence,
}

impl ReadyEventDeduper {
    pub(super) fn current(&self) -> ReadyRound {
        self.round
    }

    fn socket_lost(&mut self) -> bool {
        self.phase = None;
        self.ready_pending = false;
        self.reopen_after_disconnect = self.round.closed;
        let had_draft = self.clear_draft_session();
        self.post_game_ballot = None;
        self.post_game_sequence = None;
        had_draft
    }

    pub(super) fn draft_session(&self) -> Option<Arc<Value>> {
        self.draft_deadline
            .filter(|deadline| Instant::now() < *deadline)
            .and_then(|_| self.draft_session.as_ref().map(Arc::clone))
    }

    pub(super) fn has_draft_phase(&self) -> bool {
        self.phase.as_deref() == Some("ChampSelect")
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
                let selection_cleared = phase != "ChampSelect" && self.clear_draft_session();
                let post_game = is_post_game_phase(&phase)
                    || self.phase.as_deref().is_some_and(is_post_game_phase);
                let wake = match phase.as_str() {
                    "ReadyCheck" => {
                        self.enter();
                        true
                    }
                    other => (self.seen_ready && self.leave()) || other == "ChampSelect",
                };
                self.phase = Some(phase);
                (wake || post_game || selection_cleared).then_some(EventWake {
                    immediate: wake || selection_cleared,
                    bench_changed: false,
                    post_game,
                    selection: selection_cleared && !wake,
                })
            }
            EventSignal::Closed => self.leave().then_some(EventWake {
                immediate: true,
                bench_changed: false,
                post_game: false,
                selection: false,
            }),
            EventSignal::Response { pending, created } => {
                // A resource recreation can occur without an intervening phase poll.
                // Duplicate Create/Update on the same open resource consumes no new round.
                if created && pending && self.has_draft_phase() {
                    self.clear_draft_session();
                    self.phase = None;
                }
                if created || (!self.round.closed && pending) {
                    self.enter();
                }
                if self.round.closed {
                    return None;
                }
                let changed = self.ready_pending != pending;
                self.ready_pending = pending;
                (changed && pending).then_some(EventWake {
                    immediate: true,
                    bench_changed: false,
                    post_game: false,
                    selection: false,
                })
            }
            EventSignal::DraftSession {
                data,
                signature,
                bench,
                expires_in,
            } => {
                let first = self.draft_session().is_none();
                let significant_change = self.draft_signature.as_ref() != Some(&signature);
                self.draft_session = Some(Arc::new(data));
                self.draft_signature = Some(signature);
                self.draft_deadline =
                    (expires_in > Duration::ZERO).then(|| Instant::now() + expires_in);
                let bench_changed = match bench {
                    Some(bench) if self.bench.as_ref() != Some(&bench) => {
                        self.bench.replace(bench).is_some()
                    }
                    _ => false,
                };
                let immediate = first || significant_change;
                (immediate || bench_changed).then_some(EventWake {
                    immediate,
                    bench_changed,
                    post_game: false,
                    selection: true,
                })
            }
            EventSignal::DraftDeleted => {
                let had_draft = self.clear_draft_session();
                had_draft.then_some(EventWake {
                    immediate: true,
                    bench_changed: false,
                    post_game: false,
                    selection: true,
                })
            }
            EventSignal::PostGame {
                resource,
                signature,
            } => {
                let previous = match resource {
                    PostGameResource::Ballot => &mut self.post_game_ballot,
                    PostGameResource::Sequence => &mut self.post_game_sequence,
                };
                if *previous == Some(signature) {
                    return None;
                }
                *previous = Some(signature);
                Some(EventWake {
                    immediate: false,
                    bench_changed: false,
                    post_game: true,
                    selection: false,
                })
            }
        }
    }

    fn clear_draft_session(&mut self) -> bool {
        let had_draft = self.draft_session.take().is_some()
            || self.draft_signature.is_some()
            || self.bench.is_some();
        self.draft_signature = None;
        self.draft_deadline = None;
        self.bench = None;
        had_draft
    }
}

fn is_post_game_phase(phase: &str) -> bool {
    matches!(phase, "WaitingForStats" | "PreEndOfGame" | "EndOfGame")
}

enum EventSignal {
    Phase(String),
    Closed,
    Response {
        pending: bool,
        created: bool,
    },
    PostGame {
        resource: PostGameResource,
        signature: u64,
    },
    DraftSession {
        data: Value,
        signature: DraftSessionSignature,
        bench: Option<Vec<u32>>,
        expires_in: Duration,
    },
    DraftDeleted,
}

fn parse_event(text: &str) -> Option<EventSignal> {
    if !text.contains(GAMEFLOW_PHASE)
        && !text.contains(READY_CHECK)
        && !text.contains(CHAMP_SELECT)
        && !text.contains(POST_GAME_BALLOT_NO_SLASH)
        && !text.contains(POST_GAME_SEQUENCE)
    {
        return None;
    }
    if text.contains(CHAMP_SELECT) && text.len() > MAX_DRAFT_SESSION_BYTES {
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
        return Some(EventSignal::DraftDeleted);
    }
    if matches!(event_type, "Create" | "Update" | "Delete") {
        let resource = match uri {
            POST_GAME_BALLOT | POST_GAME_BALLOT_NO_SLASH => Some(PostGameResource::Ballot),
            POST_GAME_SEQUENCE => Some(PostGameResource::Sequence),
            _ => None,
        };
        if let Some(resource) = resource {
            let data = payload.get("data")?;
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            data.to_string().hash(&mut hasher);
            return Some(EventSignal::PostGame {
                resource,
                signature: hasher.finish(),
            });
        }
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
        (EVENT_TOPIC, CHAMP_SELECT) => {
            let (signature, bench, expires_in) = draft_session_signature(data)?;
            Some(EventSignal::DraftSession {
                data: data.clone(),
                signature,
                bench,
                expires_in,
            })
        }
        _ => None,
    }
}

fn draft_session_signature(
    data: &Value,
) -> Option<(DraftSessionSignature, Option<Vec<u32>>, Duration)> {
    let object = data.as_object()?;
    let local_player_cell_id = data["localPlayerCellId"]
        .as_i64()
        .filter(|&cell| cell >= 0)?;
    let team = data["myTeam"]
        .as_array()
        .filter(|team| !team.is_empty() && team.len() <= 10)?;
    let mut own_rows = team
        .iter()
        .filter(|row| row["cellId"].as_i64() == Some(local_player_cell_id));
    let own = own_rows.next()?;
    if own.as_object().is_none() || own_rows.next().is_some() {
        return None;
    }
    let timer = data["timer"].as_object()?;
    let timer_phase = timer.get("phase")?.as_str().filter(|phase| {
        matches!(
            *phase,
            "PLANNING" | "BAN_PICK" | "FINALIZATION" | "GAME_STARTING"
        )
    })?;
    if timer.get("isInfinite").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let remaining_ms = timer
        .get("adjustedTimeLeftInPhase")?
        .as_u64()
        .filter(|ms| *ms < 86_400_000)?;
    if remaining_ms == 0 {
        return None;
    }
    let bench = match object.get("benchChampions") {
        Some(Value::Array(champions)) if champions.len() <= 20 => {
            let signature = bench_signature(data);
            if !champions.is_empty() && signature.is_none() {
                return None;
            }
            Some(signature.unwrap_or_default())
        }
        Some(_) => return None,
        None => None,
    };
    Some((
        DraftSessionSignature {
            game_id: data.get("gameId").and_then(Value::as_u64),
            queue_id: data.get("queueId").and_then(Value::as_u64),
            local_player_cell_id,
            player: own.get("puuid").and_then(Value::as_str).map(str::to_owned),
            champion_id: own.get("championId").and_then(Value::as_u64),
            champion_pick_intent: own.get("championPickIntent").and_then(Value::as_u64),
            timer_phase: timer_phase.to_owned(),
        },
        bench,
        Duration::from_millis(remaining_ms),
    ))
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
    use super::{
        ReadyEventDeduper, GAMEFLOW_PHASE, POST_GAME_BALLOT, POST_GAME_SEQUENCE, READY_CHECK,
    };
    use std::time::{Duration, Instant};

    const CHAMP_SELECT: &str = "/lol-champ-select/v1/session";

    fn event(uri: &str, data: serde_json::Value) -> String {
        serde_json::json!([8,"OnJsonApiEvent",{"uri":uri,"eventType":"Update","data":data}])
            .to_string()
    }

    fn champ_select_event(event_type: &str, data: serde_json::Value) -> String {
        serde_json::json!([8,"OnJsonApiEvent",{"uri":CHAMP_SELECT,"eventType":event_type,"data":data}])
            .to_string()
    }

    fn typed_event(uri: &str, event_type: &str, data: serde_json::Value) -> String {
        serde_json::json!([8,"OnJsonApiEvent",{"uri":uri,"eventType":event_type,"data":data}])
            .to_string()
    }

    #[test]
    fn post_game_events_wake_only_on_exact_phase_and_resource_changes() {
        let mut state = ReadyEventDeduper::default();
        let end = typed_event(GAMEFLOW_PHASE, "Update", serde_json::json!("EndOfGame"));
        let wake = state.observe(&end).expect("end-game phase event");
        assert!(wake.post_game);
        assert!(state.observe(&end).is_none(), "duplicate phase is ignored");
        let lobby = typed_event(GAMEFLOW_PHASE, "Update", serde_json::json!("Lobby"));
        assert!(
            state.observe(&lobby).unwrap().post_game,
            "leaving end-game wakes"
        );
        assert!(state.observe(&lobby).is_none());

        let ballot = typed_event(
            POST_GAME_BALLOT,
            "Create",
            serde_json::json!({"gameId":42,"votePool":{"votes":2},"honoredPlayers":[]}),
        );
        assert!(state.observe(&ballot).unwrap().post_game);
        assert!(
            state.observe(&ballot).is_none(),
            "same ballot update is coalesced"
        );
        let changed_ballot = typed_event(
            "/lol-honor-v2/v1/ballot",
            "Update",
            serde_json::json!({"gameId":42,"votePool":{"votes":1},"honoredPlayers":["synthetic"]}),
        );
        assert!(state.observe(&changed_ballot).unwrap().post_game);

        let sequence = typed_event(
            POST_GAME_SEQUENCE,
            "Update",
            serde_json::json!({"name":"missions-celebration"}),
        );
        assert!(state.observe(&sequence).unwrap().post_game);
        assert!(state.observe(&sequence).is_none());

        for ignored in [
            typed_event("/lol-honor/v1/ballot", "Update", serde_json::json!({"gameId":42})),
            typed_event("/lol-honor-v2/v1/ballot-extra", "Update", serde_json::json!({"gameId":42})),
            r#"[8,"wrong-topic",{"uri":"/lol-honor-v2/v1/ballot/","eventType":"Update","data":{"gameId":42}}]"#.to_owned(),
            typed_event(GAMEFLOW_PHASE, "Delete", serde_json::json!("EndOfGame")),
        ] {
            assert!(state.observe(&ignored).is_none(), "unexpected wake for {ignored}");
        }
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
                "gameId": 7100,
                "queueId": 2400,
                "localPlayerCellId": 0,
                "benchChampions": bench,
                "timer": {"phase":"BAN_PICK","adjustedTimeLeftInPhase": remaining},
                "myTeam": [{"cellId": 0, "puuid":"synthetic-owner", "championId": 22}]
            })
        };

        let initial = champ_select_event("Create", session(serde_json::json!([]), 20_000));
        assert!(state.should_wake(&initial), "first full session wakes");
        assert_eq!(
            state.draft_session().as_deref(),
            Some(&session(serde_json::json!([]), 20_000))
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
        let wake = state
            .observe(&added)
            .expect("a newly benched champion wakes");
        assert!(wake.bench_changed);
        assert!(!wake.immediate, "bench-only changes remain coalesced");
        assert_eq!(
            state.draft_session().as_deref(),
            Some(&session(
                serde_json::json!([{"championId": 103}, {"championId": 0}, {"championId": 103}]),
                19_500,
            ))
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
        let delete = state
            .observe(&champ_select_event("Delete", serde_json::Value::Null))
            .expect("Delete clears a cached session even if its bench signature is unchanged");
        assert!(delete.immediate);
        assert!(state.draft_session().is_none());
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

    #[test]
    fn full_session_cache_wakes_for_identity_champion_or_timer_phase_but_not_timer_churn() {
        let session = |game_id, champion_id, timer_phase, remaining| {
            serde_json::json!({
                "gameId": game_id,
                "queueId": 2400,
                "localPlayerCellId": 0,
                "benchChampions": [{"championId": 103}],
                "timer": {"phase": timer_phase, "adjustedTimeLeftInPhase": remaining},
                "myTeam": [{"cellId": 0, "puuid": "synthetic-owner", "championId": champion_id}]
            })
        };
        let mut state = ReadyEventDeduper::default();

        assert!(state.should_wake(&champ_select_event(
            "Create",
            session(7100, 22, "BAN_PICK", 20_000),
        )));
        let timer_only = state.observe(&champ_select_event(
            "Update",
            session(7100, 22, "BAN_PICK", 19_500),
        ));
        assert!(timer_only.is_none(), "countdown churn does not wake");
        assert_eq!(
            state.draft_session().as_deref(),
            Some(&session(7100, 22, "BAN_PICK", 19_500)),
            "cache still tracks the latest full valid event"
        );

        for changed in [
            session(7101, 22, "BAN_PICK", 19_400),
            session(7101, 55, "BAN_PICK", 19_300),
            session(7101, 55, "FINALIZATION", 19_200),
        ] {
            let wake = state
                .observe(&champ_select_event("Update", changed.clone()))
                .expect("identity, champion and timer phase changes wake");
            assert!(wake.immediate);
            assert_eq!(state.draft_session().as_deref(), Some(&changed));
        }
    }

    #[test]
    fn malformed_or_oversized_session_never_replaces_the_last_valid_cache() {
        let valid = serde_json::json!({
            "gameId":7100,"queueId":2400,"localPlayerCellId":0,
            "benchChampions":[],
            "timer":{"phase":"BAN_PICK","adjustedTimeLeftInPhase":20_000},
            "myTeam":[{"cellId":0,"puuid":"synthetic-owner","championId":22}]
        });
        let mut state = ReadyEventDeduper::default();
        assert!(state.should_wake(&champ_select_event("Create", valid.clone())));

        for malformed in [
            serde_json::json!({"benchChampions":[],"timer":{},"myTeam":[]}),
            serde_json::json!({
                "localPlayerCellId":4,"benchChampions":[],
                "timer":{"phase":"BAN_PICK","adjustedTimeLeftInPhase":10},
                "myTeam":[{"cellId":0,"championId":55}]
            }),
            serde_json::json!({
                "localPlayerCellId":0,"benchChampions":[],
                "timer":{"phase":"UNKNOWN","adjustedTimeLeftInPhase":10},
                "myTeam":[{"cellId":0,"championId":55}]
            }),
        ] {
            assert!(state
                .observe(&champ_select_event("Update", malformed))
                .is_none());
            assert_eq!(state.draft_session().as_deref(), Some(&valid));
        }

        let oversized = format!(
            "[8,\"OnJsonApiEvent\",{{\"uri\":\"{CHAMP_SELECT}\",\"eventType\":\"Update\",\"data\":{{\"padding\":\"{}\"}}}}]",
            "x".repeat(256 * 1024),
        );
        assert!(state.observe(&oversized).is_none());
        assert_eq!(state.draft_session().as_deref(), Some(&valid));
    }

    #[test]
    fn expired_cached_session_is_hidden_and_same_identity_event_wakes_again() {
        let session = serde_json::json!({
            "gameId":7100,"queueId":2400,"localPlayerCellId":0,
            "benchChampions":[],
            "timer":{"phase":"BAN_PICK","adjustedTimeLeftInPhase":20_000},
            "myTeam":[{"cellId":0,"puuid":"synthetic-owner","championId":22}]
        });
        let mut state = ReadyEventDeduper::default();
        assert!(state.should_wake(&champ_select_event("Create", session.clone())));
        state.draft_deadline = Some(Instant::now() - Duration::from_millis(1));
        assert!(
            state.draft_session().is_none(),
            "expired presentation data is hidden"
        );

        let wake = state
            .observe(&champ_select_event("Update", session))
            .expect("a fresh session after expiry wakes even without identity changes");
        assert!(wake.immediate);
        assert!(state.draft_session().is_some());
    }

    #[test]
    fn phase_departure_and_socket_loss_clear_the_cached_session() {
        let session = serde_json::json!({
            "gameId":7100,"queueId":2400,"localPlayerCellId":0,
            "benchChampions":[],
            "timer":{"phase":"BAN_PICK","adjustedTimeLeftInPhase":20_000},
            "myTeam":[{"cellId":0,"puuid":"synthetic-owner","championId":22}]
        });
        let mut state = ReadyEventDeduper::default();
        assert!(state.should_wake(&champ_select_event("Create", session.clone())));
        assert!(state.should_wake(&event(GAMEFLOW_PHASE, serde_json::json!("ChampSelect"))));
        assert!(state.has_draft_phase());
        let left = state
            .observe(&event(GAMEFLOW_PHASE, serde_json::json!("EndOfGame")))
            .expect("departing ChampSelect wakes the monitor");
        assert!(left.immediate);
        assert!(!state.has_draft_phase());
        assert!(state.draft_session().is_none());

        assert!(state.should_wake(&champ_select_event("Create", session.clone())));
        let ready = state
            .observe(&event(GAMEFLOW_PHASE, serde_json::json!("ReadyCheck")))
            .expect("a new ready check wakes even while clearing draft data");
        assert!(ready.immediate && !ready.selection);
        assert!(state.draft_session().is_none());

        assert!(state.should_wake(&event(GAMEFLOW_PHASE, serde_json::json!("ChampSelect"))));
        assert!(state.should_wake(&champ_select_event("Create", session.clone())));
        let new_round = typed_event(
            READY_CHECK,
            "Create",
            serde_json::json!({"state":"InProgress","playerResponse":"None"}),
        );
        assert!(state.should_wake(&new_round));
        assert!(!state.has_draft_phase());
        assert!(state.draft_session().is_none());

        assert!(state.should_wake(&champ_select_event("Create", session)));
        assert!(
            state.socket_lost(),
            "socket loss reports a cleared draft cache"
        );
        assert!(!state.has_draft_phase());
        assert!(state.draft_session().is_none());
        assert!(!state.socket_lost(), "repeat cleanup is idempotent");
    }
}
