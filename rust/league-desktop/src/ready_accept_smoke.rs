//! Production monitor path acceptance checks for ready-check auto-accept.

use super::*;
use crate::{
    client_discovery::LocalClient,
    desktop::aux_window::AuxWindow,
    opgg_window::{OpggPreferences, OpggWindow},
    resg_window::ResgWindow,
    room_flow::RoomOptions,
};
use rustls::{
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
    ServerConfig, ServerConnection, StreamOwned,
};
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, Cursor, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tauri::Manager;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_tungstenite::tungstenite::{self, Message};

const PLAYER: &str = "11111111-2222-4333-8444-555555555555";
const HONOR_TARGET: &str = "11111111-2222-4333-8444-555555555556";
const FIXTURE_AUTH: &str = "Basic cmlvdDpmaXh0dXJlLW9ubHk=";

struct ReadyFixture {
    port: u16,
    root: CertificateDer<'static>,
    scene: Arc<Mutex<Scene>>,
    event_senders: Arc<Mutex<Vec<mpsc::Sender<WsCommand>>>>,
    subscribers: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    posts: UnboundedReceiver<Instant>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Scene {
    phase: String,
    queue_id: u64,
    game_id: u64,
    response: String,
    direct_ready_response: String,
    direct_ready_available: bool,
    slow_search_ms: u64,
    slow_sequence_once_ms: u64,
    request_trace: Vec<String>,
    request_times: Vec<(String, Instant)>,
    posts: Vec<Instant>,
    post_game_ballot_available: bool,
    post_game_ballot_submitted: bool,
    post_game_honors: Vec<String>,
    post_game_writes: Vec<(String, Instant)>,
    future_session_reads_until_missing: Option<usize>,
    session_observations: Vec<(String, Option<u64>, Instant)>,
}

#[derive(Clone)]
struct WsEvent {
    event_type: &'static str,
    uri: &'static str,
    data: Value,
}

#[derive(Clone)]
enum WsCommand {
    Event(WsEvent),
    Close,
}

impl ReadyFixture {
    fn start(queue_id: u64) -> Self {
        let pair = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let root = pair.cert.der().clone();
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![root.clone()],
                    PrivatePkcs8KeyDer::from(pair.signing_key.serialize_der()).into(),
                )
                .unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let scene = Arc::new(Mutex::new(Scene {
            phase: "Lobby".into(),
            queue_id,
            response: "None".into(),
            direct_ready_response: "None".into(),
            direct_ready_available: true,
            ..Scene::default()
        }));
        let event_senders = Arc::new(Mutex::new(Vec::new()));
        let subscribers = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (post_tx, posts) = tokio::sync::mpsc::unbounded_channel();
        let worker = {
            let scene = Arc::clone(&scene);
            let event_senders = Arc::clone(&event_senders);
            let subscribers = Arc::clone(&subscribers);
            let stop = Arc::clone(&stop);
            let config = Arc::new(config);
            thread::spawn(move || {
                listener.set_nonblocking(true).unwrap();
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((socket, _)) => {
                            let scene = Arc::clone(&scene);
                            let event_senders = Arc::clone(&event_senders);
                            let subscribers = Arc::clone(&subscribers);
                            let config = Arc::clone(&config);
                            let post_tx = post_tx.clone();
                            thread::spawn(move || {
                                let _ = serve_connection(
                                    socket,
                                    config,
                                    scene,
                                    event_senders,
                                    subscribers,
                                    post_tx,
                                );
                            });
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            port,
            root,
            scene,
            event_senders,
            subscribers,
            stop,
            posts,
            worker: Some(worker),
        }
    }

    fn use_slow_search(&self, milliseconds: u64) {
        self.scene.lock().unwrap().slow_search_ms = milliseconds;
    }

    fn ready_check(&self) {
        let mut scene = self.scene.lock().unwrap();
        scene.phase = "ReadyCheck".into();
        scene.response = "None".into();
        scene.direct_ready_response = "None".into();
    }

    fn requeue_ready_check_without_phase_change(&self) {
        let mut scene = self.scene.lock().unwrap();
        scene.phase = "ReadyCheck".into();
        // The search projection may lag a new ready check. The dedicated endpoint is current.
        scene.response = "Accepted".into();
        scene.direct_ready_response = "None".into();
    }

    fn restart_pending_ready_check_without_phase_change(&self) {
        let mut scene = self.scene.lock().unwrap();
        scene.phase = "ReadyCheck".into();
        scene.response = "None".into();
        scene.direct_ready_response = "None".into();
    }

    fn hide_direct_ready_check(&self) {
        self.scene.lock().unwrap().direct_ready_available = false;
    }

    fn set_classic(&self) {
        let mut scene = self.scene.lock().unwrap();
        scene.queue_id = 440;
        scene.phase = "ReadyCheck".into();
        scene.response = "None".into();
    }

    fn emit_phase(&self, phase: &'static str) {
        self.emit(WsEvent {
            event_type: "Update",
            uri: "/lol-gameflow/v1/gameflow-phase",
            data: json!(phase),
        });
    }

    fn start_post_game(&self, phase: &'static str, game_id: u64) {
        let mut scene = self.scene.lock().unwrap();
        scene.phase = phase.into();
        scene.queue_id = 450;
        scene.game_id = game_id;
        scene.post_game_ballot_available = false;
        scene.post_game_ballot_submitted = false;
        scene.post_game_honors.clear();
        scene.post_game_writes.clear();
    }

    fn slow_next_sequence_read(&self, milliseconds: u64) {
        self.scene.lock().unwrap().slow_sequence_once_ms = milliseconds;
    }

    fn make_nth_future_session_incomplete(&self, nth: usize) {
        self.scene
            .lock()
            .unwrap()
            .future_session_reads_until_missing = Some(nth);
    }

    fn session_observations(&self) -> Vec<(String, Option<u64>, Instant)> {
        self.scene.lock().unwrap().session_observations.clone()
    }

    async fn wait_for_incomplete_end_game_session(&self, timeout: Duration) -> Option<Instant> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some((_, _, at)) = self
                .session_observations()
                .iter()
                .find(|(phase, game_id, _)| phase == "EndOfGame" && game_id.is_none())
            {
                return Some(*at);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.session_observations()
            .into_iter()
            .find(|(phase, game_id, _)| phase == "EndOfGame" && game_id.is_none())
            .map(|(_, _, at)| at)
    }

    async fn wait_for_session_quiet(&self, quiet: Duration, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut last_count = self.session_observations().len();
        let mut quiet_since = Instant::now();
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let count = self.session_observations().len();
            if count != last_count {
                last_count = count;
                quiet_since = Instant::now();
            } else if quiet_since.elapsed() >= quiet {
                return true;
            }
        }
        false
    }

    fn publish_post_game_ballot(&self) {
        let game_id = {
            let mut scene = self.scene.lock().unwrap();
            scene.post_game_ballot_available = true;
            scene.game_id
        };
        self.emit(WsEvent {
            event_type: "Create",
            uri: "/lol-honor-v2/v1/ballot/",
            data: json!({"gameId":game_id,"votePool":{"votes":1},"honoredPlayers":[]}),
        });
    }

    fn post_game_writes(&self) -> Vec<(String, Instant)> {
        self.scene.lock().unwrap().post_game_writes.clone()
    }

    async fn wait_for_request_after(
        &self,
        fragment: &str,
        after: Instant,
        timeout: Duration,
    ) -> Option<Instant> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some((_, at)) = self
                .scene
                .lock()
                .unwrap()
                .request_times
                .iter()
                .find(|(request, at)| request.contains(fragment) && *at >= after)
            {
                return Some(*at);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.scene
            .lock()
            .unwrap()
            .request_times
            .iter()
            .find(|(request, at)| request.contains(fragment) && *at >= after)
            .map(|(_, at)| *at)
    }

    async fn wait_for_post_game_write(
        &self,
        path: &str,
        after: Instant,
        timeout: Duration,
    ) -> Option<Instant> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some((_, at)) = self
                .scene
                .lock()
                .unwrap()
                .post_game_writes
                .iter()
                .find(|(request, at)| request == path && *at >= after)
            {
                return Some(*at);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.scene
            .lock()
            .unwrap()
            .post_game_writes
            .iter()
            .find(|(request, at)| request == path && *at >= after)
            .map(|(_, at)| *at)
    }

    fn emit_ready(&self, response: &'static str) {
        self.emit_ready_event("Update", response);
    }

    fn emit_ready_event(&self, event_type: &'static str, response: &'static str) {
        self.emit(WsEvent {
            event_type,
            uri: "/lol-matchmaking/v1/ready-check",
            data: json!({"state":"InProgress","playerResponse":response}),
        });
    }

    fn emit_ready_delete_create(&self) {
        self.emit(WsEvent {
            event_type: "Delete",
            uri: "/lol-matchmaking/v1/ready-check",
            data: Value::Null,
        });
        self.emit(WsEvent {
            event_type: "Create",
            uri: "/lol-matchmaking/v1/ready-check",
            // Riot's pinned ReadyCheck has no ID field.
            data: json!({"state":"InProgress","playerResponse":"None"}),
        });
    }

    fn phase(&self) -> String {
        self.scene.lock().unwrap().phase.clone()
    }

    fn readiness(&self) -> (String, String) {
        let scene = self.scene.lock().unwrap();
        (scene.response.clone(), scene.direct_ready_response.clone())
    }

    fn emit(&self, event: WsEvent) {
        if let Ok(mut senders) = self.event_senders.lock() {
            senders.retain(|sender| sender.send(WsCommand::Event(event.clone())).is_ok());
        }
    }

    fn close_events(&self) {
        if let Ok(mut senders) = self.event_senders.lock() {
            for sender in senders.iter() {
                let _ = sender.send(WsCommand::Close);
            }
            senders.clear();
        }
    }

    fn post_count(&self) -> usize {
        self.scene.lock().unwrap().posts.len()
    }

    fn request_trace(&self) -> Vec<String> {
        self.scene.lock().unwrap().request_trace.clone()
    }

    fn request_count(&self, fragment: &str) -> usize {
        self.scene
            .lock()
            .unwrap()
            .request_trace
            .iter()
            .filter(|request| request.contains(fragment))
            .count()
    }

    async fn wait_subscribed(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.subscribers.load(Ordering::Acquire) > 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.subscribers.load(Ordering::Acquire) > 0
    }

    async fn receive_post(&mut self, timeout: Duration) -> Option<Instant> {
        tokio::time::timeout(timeout, self.posts.recv())
            .await
            .ok()
            .flatten()
    }
}

impl Drop for ReadyFixture {
    fn drop(&mut self) {
        self.close_events();
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        self.worker.take().unwrap().join().unwrap();
    }
}

struct PrefixStream<S> {
    prefix: Cursor<Vec<u8>>,
    inner: S,
}

impl<S: Read> Read for PrefixStream<S> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.prefix.position() < self.prefix.get_ref().len() as u64 {
            self.prefix.read(buffer)
        } else {
            self.inner.read(buffer)
        }
    }
}

impl<S: Write> Write for PrefixStream<S> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn serve_connection(
    socket: TcpStream,
    config: Arc<ServerConfig>,
    scene: Arc<Mutex<Scene>>,
    event_senders: Arc<Mutex<Vec<mpsc::Sender<WsCommand>>>>,
    subscribers: Arc<AtomicUsize>,
    post_tx: tokio::sync::mpsc::UnboundedSender<Instant>,
) -> io::Result<()> {
    // Windows may carry the listener's nonblocking mode into accepted sockets.
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(Duration::from_secs(5))).ok();
    socket.set_write_timeout(Some(Duration::from_secs(2))).ok();
    let mut stream = StreamOwned::new(ServerConnection::new(config).unwrap(), socket);
    let mut bytes = Vec::new();
    let mut one = [0; 1];
    while !bytes.ends_with(b"\r\n\r\n") && bytes.len() < 16 * 1024 {
        match stream.read_exact(&mut one) {
            Ok(()) => bytes.push(one[0]),
            Err(error) => {
                eprintln!(
                    "fixture TLS/HTTP header read stopped after {} bytes: {:?}",
                    bytes.len(),
                    error.kind()
                );
                return Err(error);
            }
        }
    }
    // Keep the HTTP/WebSocket poll timeout short, but let local TLS finish first.
    stream
        .sock
        .set_read_timeout(Some(Duration::from_millis(60)))
        .ok();
    let header = String::from_utf8_lossy(&bytes).into_owned();
    let prefixed = PrefixStream {
        prefix: Cursor::new(bytes),
        inner: stream,
    };
    if header.to_ascii_lowercase().contains("upgrade: websocket") {
        let authenticated = header.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("authorization") && value.trim() == FIXTURE_AUTH
            })
        });
        if !authenticated {
            eprintln!("fixture rejected unauthenticated LCU WebSocket request");
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "fixture rejected unauthenticated event socket",
            ));
        }
        serve_websocket(prefixed, event_senders, subscribers)
    } else {
        serve_http(prefixed, scene, post_tx)
    }
}

fn serve_websocket(
    stream: PrefixStream<StreamOwned<ServerConnection, TcpStream>>,
    event_senders: Arc<Mutex<Vec<mpsc::Sender<WsCommand>>>>,
    subscribers: Arc<AtomicUsize>,
) -> io::Result<()> {
    let mut socket = tungstenite::accept(stream).map_err(io::Error::other)?;
    let (event_tx, event_rx) = mpsc::channel();
    if let Ok(mut senders) = event_senders.lock() {
        senders.push(event_tx);
    }
    let mut subscribed = false;
    let mut counted = false;
    loop {
        loop {
            match event_rx.try_recv() {
                Ok(WsCommand::Event(event)) if subscribed => {
                    let payload = json!([8,"OnJsonApiEvent",{"uri":event.uri,"eventType":event.event_type,"data":event.data}]);
                    if socket
                        .send(Message::Text(payload.to_string().into()))
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Ok(WsCommand::Event(_)) => {}
                Ok(WsCommand::Close) | Err(mpsc::TryRecvError::Disconnected) => {
                    let _ = socket.close(None);
                    if counted {
                        subscribers.fetch_sub(1, Ordering::AcqRel);
                    }
                    return Ok(());
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(&text) {
                    if parts.first() == Some(&json!(5)) {
                        if let Some(topic) = parts.get(1).and_then(Value::as_str) {
                            subscribed |= topic == "OnJsonApiEvent";
                            if !counted && subscribed {
                                counted = true;
                                subscribers.fetch_add(1, Ordering::AcqRel);
                            }
                        }
                    }
                }
            }
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => {
                if counted {
                    subscribers.fetch_sub(1, Ordering::AcqRel);
                }
                return Ok(());
            }
            Ok(Message::Ping(data)) => {
                let _ = socket.send(Message::Pong(data));
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => {
                if counted {
                    subscribers.fetch_sub(1, Ordering::AcqRel);
                }
                return Ok(());
            }
        }
    }
}

fn serve_http<S: Read + Write>(
    stream: S,
    scene: Arc<Mutex<Scene>>,
    post_tx: tokio::sync::mpsc::UnboundedSender<Instant>,
) -> io::Result<()> {
    let mut reader = io::BufReader::new(stream);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let mut request = first.split_whitespace();
    let method = request.next().unwrap_or("");
    let path = request.next().unwrap_or("");
    let mut length = 0usize;
    let mut authenticated = false;
    for line in reader.by_ref().lines() {
        let line = line?;
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
            if name.eq_ignore_ascii_case("authorization") {
                authenticated = value.trim() == FIXTURE_AUTH;
            }
        }
    }
    if length > 16 * 1024 {
        return Err(io::Error::other("fixture request body too large"));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let started_at = Instant::now();
    let mut post_time = None;
    if authenticated && method == "GET" && path == "/lol-pre-end-of-game/v1/currentSequenceEvent" {
        let delay = {
            let mut scene = scene.lock().unwrap();
            std::mem::take(&mut scene.slow_sequence_once_ms)
        };
        if delay > 0 {
            thread::sleep(Duration::from_millis(delay));
        }
    }
    let (status, response) = if !authenticated {
        (401, Value::Null)
    } else {
        let mut scene = scene.lock().unwrap();
        let mode = if scene.queue_id == 450 {
            "ARAM"
        } else {
            "CLASSIC"
        };
        if method == "POST" {
            match path {
                "/lol-matchmaking/v1/ready-check/accept" => {
                    scene.posts.push(started_at);
                    scene.response = "Accepted".into();
                    scene.direct_ready_response = "Accepted".into();
                    post_time = Some(started_at);
                    (200, json!({}))
                }
                "/lol-honor/v1/honor" => {
                    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let recipient = body["recipientPuuid"].as_str().unwrap_or_default();
                    let valid = matches!(scene.phase.as_str(), "PreEndOfGame" | "EndOfGame")
                        && scene.post_game_ballot_available
                        && !scene.post_game_ballot_submitted
                        && body["honorType"] == "HEART"
                        && recipient == HONOR_TARGET
                        && !scene.post_game_honors.iter().any(|old| old == recipient);
                    if valid {
                        scene.post_game_honors.push(recipient.to_owned());
                    }
                    scene.post_game_writes.push((path.to_owned(), started_at));
                    (if valid { 204 } else { 409 }, json!({}))
                }
                "/lol-honor/v1/ballot" => {
                    let valid = matches!(scene.phase.as_str(), "PreEndOfGame" | "EndOfGame")
                        && scene.post_game_ballot_available
                        && !scene.post_game_ballot_submitted
                        && scene.post_game_honors.contains(&HONOR_TARGET.to_owned());
                    if valid {
                        scene.post_game_ballot_submitted = true;
                    }
                    scene.post_game_writes.push((path.to_owned(), started_at));
                    (if valid { 204 } else { 409 }, json!({}))
                }
                "/lol-lobby/v2/play-again" => {
                    let valid = scene.phase == "EndOfGame" && scene.post_game_ballot_submitted;
                    if valid {
                        scene.phase = "Lobby".into();
                    }
                    scene.post_game_writes.push((path.to_owned(), started_at));
                    (if valid { 204 } else { 409 }, json!({}))
                }
                "/lol-pre-end-of-game/v1/complete/missions-celebration" => {
                    scene.post_game_writes.push((path.to_owned(), started_at));
                    (204, json!({}))
                }
                _ => (404, Value::Null),
            }
        } else if method == "GET" {
            if path == "/lol-matchmaking/v1/search" && scene.slow_search_ms > 0 {
                thread::sleep(Duration::from_millis(scene.slow_search_ms));
            }
            let response = match path {
                "/lol-summoner/v1/current-summoner" => {
                    json!({"puuid":PLAYER,"summonerId":41,"displayName":"Fixture"})
                }
                "/lol-chat/v1/me" => json!({"puuid":PLAYER,"platformId":"NA1","name":"Fixture"}),
                "/lol-gameflow/v1/gameflow-phase" => json!(scene.phase),
                "/lol-gameflow/v1/session" => {
                    let incomplete =
                        scene
                            .future_session_reads_until_missing
                            .take()
                            .is_some_and(|remaining| {
                                if remaining <= 1 {
                                    true
                                } else {
                                    scene.future_session_reads_until_missing = Some(remaining - 1);
                                    false
                                }
                            });
                    let game_id = (!incomplete).then_some(scene.game_id);
                    let phase = scene.phase.clone();
                    scene.session_observations.push((phase, game_id, started_at));
                    if incomplete {
                        json!({"phase":scene.phase,"gameData":{"isCustomGame":false,"queue":{"id":scene.queue_id,"gameMode":mode,"type":mode}}})
                    } else {
                        json!({"phase":scene.phase,"gameData":{"gameId":scene.game_id,"isCustomGame":false,"queue":{"id":scene.queue_id,"gameMode":mode,"type":mode}}})
                    }
                }
                "/lol-lobby/v2/lobby" => {
                    json!({"partyId":"fixture-party","gameConfig":{"queueId":scene.queue_id,"gameMode":mode,"isCustom":false},"localMember":{"puuid":PLAYER,"isLeader":true},"members":[{"puuid":PLAYER,"isLeader":true}],"invitations":[]})
                }
                "/lol-matchmaking/v1/search" => {
                    json!({"queueId":scene.queue_id,"readyCheck":{"state":"InProgress","playerResponse":scene.response}})
                }
                "/lol-matchmaking/v1/ready-check" if scene.direct_ready_available => {
                    json!({"state":"InProgress","playerResponse":scene.direct_ready_response})
                }
                "/lol-honor-v2/v1/ballot/" | "/lol-honor-v2/v1/ballot"
                    if scene.post_game_ballot_available && !scene.post_game_ballot_submitted =>
                {
                    json!({
                        "gameId":scene.game_id,
                        "eligibleAllies":[{"puuid":PLAYER,"botPlayer":false},{"puuid":HONOR_TARGET,"botPlayer":false}],
                        "eligibleOpponents":[],
                        "honoredPlayers":[],
                        "votePool":{"fromGamePlayed":1,"fromHighHonor":0,"fromRecentHonors":0,"fromRollover":0,"votes":1}
                    })
                }
                "/lol-lobby/v2/party/eog-status" => {
                    json!({"eogPlayers":[PLAYER],"readyPlayers":[],"leftPlayers":[],"partySize":1})
                }
                "/lol-pre-end-of-game/v1/currentSequenceEvent" => {
                    json!({"name":"missions-celebration"})
                }
                _ => Value::Null,
            };
            (if response.is_null() { 404 } else { 200 }, response)
        } else {
            (404, Value::Null)
        }
    };
    {
        let mut scene = scene.lock().unwrap();
        let request = format!("{method} {path}");
        scene.request_times.push((request.clone(), started_at));
        scene.request_trace.push(format!(
            "{request} -> {status} (authenticated={authenticated})"
        ));
    }
    if let Some(time) = post_time {
        let _ = post_tx.send(time);
    }
    let response = serde_json::to_vec(&response).map_err(io::Error::other)?;
    let mut stream = reader.into_inner();
    write!(
        stream,
        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
    )?;
    stream.write_all(&response)?;
    stream.flush()
}

fn app_with_runtime(fixture: &ReadyFixture) -> (tauri::App, Arc<LocalClient>) {
    app_with_runtime_options(
        fixture,
        RoomOptions {
            accept: true,
            ..RoomOptions::default()
        },
    )
}

fn app_with_runtime_options(
    fixture: &ReadyFixture,
    options: RoomOptions,
) -> (tauri::App, Arc<LocalClient>) {
    let client =
        Arc::new(LocalClient::fixture_at(fixture.port).expect("fixture listener identity"));
    assert!(client.alive(), "fixture process handle must remain live");
    assert!(
        client.target().is_ok(),
        "verified current-process listener must authorize the fixture port"
    );
    let mut runtime = ClientRuntime::new(false, Default::default()).expect("fixture runtime");
    runtime.use_fixture_transport(fixture.root.clone());
    runtime.use_fixture_clients(vec![Arc::clone(&client)]);
    let app = tauri::Builder::default()
        .any_thread()
        .manage(runtime)
        .manage(AuxWindow::new(true, false))
        .manage(OpggWindow::new(OpggPreferences {
            enabled: false,
            auto_show: false,
        }))
        .manage(ResgWindow::new(json!({"enabled":false,"autoShow":false})))
        .build(tauri::generate_context!())
        .expect("build isolated monitor app");
    let runtime = app.state::<ClientRuntime>();
    {
        runtime.state.lock().unwrap().snapshot.known_clients = vec![client.instance];
    }
    runtime
        .select(&app.handle(), Some(client.instance))
        .unwrap();
    runtime
        .replace_room_options(&app.handle(), options)
        .unwrap();
    (app, client)
}

async fn wait_for_phase(app: &tauri::App, phase: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let current = app
            .state::<ClientRuntime>()
            .state
            .lock()
            .ok()
            .and_then(|state| {
                state
                    .snapshot
                    .active
                    .as_ref()
                    .map(|active| active.phase.as_str().to_owned())
            });
        if current.as_deref() == Some(phase) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

async fn consume_startup_refresh(app: &tauri::App) {
    // select + initial option setup coalesce to one Notify permit. Drain it so the test
    // measures the event-triggered wake, not a setup refresh.
    app.state::<ClientRuntime>().wake.notified().await;
}

async fn wait_for_lobby(app: &tauri::App, fixture: &ReadyFixture) -> bool {
    if wait_for_phase(app, "Lobby", Duration::from_secs(4)).await {
        true
    } else {
        eprintln!(
            "Lobby sample absent; fixture routes: {:?}",
            fixture.request_trace()
        );
        false
    }
}

async fn wait_for_request_count(
    fixture: &ReadyFixture,
    fragment: &str,
    minimum: usize,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if fixture.request_count(fragment) >= minimum {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    fixture.request_count(fragment) >= minimum
}

#[test]
#[ignore = "requires a disposable Windows desktop runner; exercises the production monitor and loopback LCU"]
fn ready_check_event_wakes_monitor_and_accepts_once_with_slow_lcu_reads() {
    let mut fixture = ReadyFixture::start(450);
    fixture.use_slow_search(150);
    let (app, _client) = app_with_runtime(&fixture);
    let app_handle = app.handle().clone();
    tauri::async_runtime::block_on(async {
        consume_startup_refresh(&app).await;
        let monitor = tauri::async_runtime::spawn(monitor(app_handle));
        assert!(wait_for_lobby(&app, &fixture).await);
        assert!(fixture.wait_subscribed(Duration::from_secs(3)).await);
        let started = Instant::now();
        fixture.ready_check();
        fixture.emit_phase("ReadyCheck");
        fixture.emit_ready("None");
        fixture.emit_ready("None");

        let accepted_at = match fixture.receive_post(Duration::from_millis(900)).await {
            Some(time) => time,
            None => {
                let late = fixture.receive_post(Duration::from_secs(3)).await;
                panic!(
                    "ReadyCheck event missed the fast acceptance bound; first POST arrived at {:?}",
                    late.map(|time| time.duration_since(started))
                );
            }
        };
        let latency = accepted_at.duration_since(started);
        assert!(
            latency >= Duration::from_millis(150),
            "the configured fixture read delay was not exercised: {latency:?}"
        );
        assert!(
            latency < Duration::from_millis(900),
            "ready-check event to LCU POST took {latency:?}"
        );
        eprintln!("WebSocket ReadyCheck-to-first-POST latency: {latency:?}");

        fixture.emit_phase("ReadyCheck");
        fixture.emit_ready("None");
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(
            fixture.post_count(),
            1,
            "duplicate LCU events must not accept twice"
        );
        monitor.abort();
    });
}

#[test]
#[ignore = "requires a disposable Windows desktop runner; measures the unchanged polling path"]
fn polling_baseline_accept_waits_for_next_two_second_monitor_tick() {
    let mut fixture = ReadyFixture::start(450);
    fixture.use_slow_search(150);
    let (app, _client) = app_with_runtime(&fixture);
    app.state::<ClientRuntime>().disable_fixture_events();
    let app_handle = app.handle().clone();
    tauri::async_runtime::block_on(async {
        consume_startup_refresh(&app).await;
        let monitor = tauri::async_runtime::spawn(monitor(app_handle));
        assert!(wait_for_lobby(&app, &fixture).await);
        tokio::time::sleep(Duration::from_millis(350)).await;
        let started = Instant::now();
        fixture.ready_check();
        let accepted_at = fixture
            .receive_post(Duration::from_secs(4))
            .await
            .expect("polling baseline should eventually accept");
        let latency = accepted_at.duration_since(started);
        assert!(
            latency >= Duration::from_millis(1200),
            "polling baseline unexpectedly avoided its monitor interval: {latency:?}"
        );
        eprintln!("polling ReadyCheck-to-POST baseline: {latency:?}");
        assert_eq!(fixture.post_count(), 1);
        monitor.abort();
    });
}

#[test]
#[ignore = "requires a disposable Windows desktop runner; exercises disconnect fallback"]
fn monitor_falls_back_to_normal_polling_after_event_socket_closes() {
    let mut fixture = ReadyFixture::start(450);
    let (app, _client) = app_with_runtime(&fixture);
    let app_handle = app.handle().clone();
    tauri::async_runtime::block_on(async {
        consume_startup_refresh(&app).await;
        let monitor = tauri::async_runtime::spawn(monitor(app_handle));
        assert!(wait_for_lobby(&app, &fixture).await);
        assert!(fixture.wait_subscribed(Duration::from_secs(3)).await);
        fixture.close_events();
        fixture.ready_check();
        let post = fixture.receive_post(Duration::from_secs(4)).await;
        assert!(
            post.is_some(),
            "polling must still accept after the event socket closes"
        );
        assert!(
            fixture.post_count() <= 1,
            "one ready-check may produce one accept POST"
        );
        monitor.abort();
    });
}

#[test]
#[ignore = "requires a disposable Windows desktop runner; exercises an unrelated queue"]
fn ready_check_event_does_not_accept_non_aram_queue() {
    let fixture = ReadyFixture::start(440);
    let (app, _client) = app_with_runtime(&fixture);
    let app_handle = app.handle().clone();
    tauri::async_runtime::block_on(async {
        consume_startup_refresh(&app).await;
        let monitor = tauri::async_runtime::spawn(monitor(app_handle));
        assert!(wait_for_lobby(&app, &fixture).await);
        assert!(fixture.wait_subscribed(Duration::from_secs(3)).await);
        fixture.set_classic();
        fixture.emit_phase("ReadyCheck");
        fixture.emit_ready("None");
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
            fixture.post_count(),
            0,
            "non-ARAM state must not invoke the accept route"
        );
        monitor.abort();
    });
}

#[test]
#[ignore = "requires a disposable Windows desktop runner; exercises authenticated post-game WSS wake and deadline scheduling"]
fn post_game_ballot_event_wakes_fast_and_return_observes_upstream_deadline() {
    let fixture = ReadyFixture::start(450);
    let options = RoomOptions {
        honor: true,
        play_again: true,
        ..RoomOptions::default()
    };
    let (app, _client) = app_with_runtime_options(&fixture, options);
    let app_handle = app.handle().clone();
    tauri::async_runtime::block_on(async {
        consume_startup_refresh(&app).await;
        let monitor = tauri::async_runtime::spawn(monitor(app_handle));
        assert!(wait_for_lobby(&app, &fixture).await);
        assert!(fixture.wait_subscribed(Duration::from_secs(3)).await);

        let phase_started = Instant::now();
        fixture.start_post_game("EndOfGame", 7711);
        fixture.emit_phase("EndOfGame");
        assert!(wait_for_phase(&app, "EndOfGame", Duration::from_secs(1)).await);
        assert!(fixture
            .wait_for_request_after(
                "GET /lol-honor-v2/v1/ballot/",
                phase_started,
                Duration::from_secs(1),
            )
            .await
            .is_some());
        // Allow the initial no-ballot sample and its writer guard to finish before the
        // synthetic resource Create. The test then measures the WSS-triggered work itself.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let event_started = Instant::now();
        fixture.publish_post_game_ballot();
        let ballot_read = fixture
            .wait_for_request_after(
                "GET /lol-honor-v2/v1/ballot/",
                event_started,
                Duration::from_millis(500),
            )
            .await
            .expect("ballot Create should trigger an authenticated fresh ballot read");
        let honor = fixture
            .wait_for_post_game_write(
                "/lol-honor/v1/honor",
                event_started,
                Duration::from_millis(500),
            )
            .await
            .expect("the event-triggered run should submit the eligible human honor");
        let submit = fixture
            .wait_for_post_game_write(
                "/lol-honor/v1/ballot",
                event_started,
                Duration::from_millis(500),
            )
            .await
            .expect("successful honor should submit the ballot once");
        let ballot_latency = ballot_read.duration_since(event_started);
        let honor_latency = honor.duration_since(event_started);
        let submit_latency = submit.duration_since(event_started);
        assert!(
            ballot_latency < Duration::from_millis(500),
            "ballot read: {ballot_latency:?}"
        );
        assert!(
            honor_latency < Duration::from_millis(500),
            "honor POST: {honor_latency:?}"
        );
        assert!(
            submit_latency < Duration::from_millis(500),
            "ballot POST: {submit_latency:?}"
        );

        let stage_deadline = phase_started + Duration::from_millis(1_575);
        assert!(
            fixture
                .post_game_writes()
                .iter()
                .all(|(path, at)| path != "/lol-lobby/v2/play-again" || *at >= stage_deadline),
            "Play Again must not precede the upstream EndOfGame buffer"
        );
        let return_timeout =
            (stage_deadline + Duration::from_millis(500)).saturating_duration_since(Instant::now());
        let play_again = fixture
            .wait_for_post_game_write("/lol-lobby/v2/play-again", phase_started, return_timeout)
            .await
            .expect("return should follow the upstream deadline without another monitor tick");
        assert!(
            play_again >= stage_deadline,
            "Play Again preceded the EndOfGame buffer"
        );
        let return_lag = play_again.duration_since(stage_deadline);
        assert!(
            return_lag < Duration::from_millis(500),
            "due return lag: {return_lag:?}"
        );

        let writes = fixture.post_game_writes();
        assert_eq!(
            writes
                .iter()
                .filter(|(path, _)| path == "/lol-honor/v1/honor")
                .count(),
            1
        );
        assert_eq!(
            writes
                .iter()
                .filter(|(path, _)| path == "/lol-honor/v1/ballot")
                .count(),
            1
        );
        assert_eq!(
            writes
                .iter()
                .filter(|(path, _)| path == "/lol-lobby/v2/play-again")
                .count(),
            1
        );
        assert_eq!(fixture.phase(), "Lobby");
        eprintln!(
            "WSS post-game event -> ballot GET {ballot_latency:?}, honor POST {honor_latency:?}, ballot submit {submit_latency:?}; EndOfGame deadline -> Play Again {return_lag:?}"
        );
        monitor.abort();
    });
}

#[test]
#[ignore = "requires a disposable Windows desktop runner; protects an event received while an optional post-game read holds the writer"]
fn post_game_ballot_event_is_not_lost_during_slow_optional_sequence_read() {
    let fixture = ReadyFixture::start(450);
    fixture.slow_next_sequence_read(600);
    let options = RoomOptions {
        honor: true,
        play_again: true,
        ..RoomOptions::default()
    };
    let (app, _client) = app_with_runtime_options(&fixture, options);
    let app_handle = app.handle().clone();
    tauri::async_runtime::block_on(async {
        consume_startup_refresh(&app).await;
        let monitor = tauri::async_runtime::spawn(monitor(app_handle));
        assert!(wait_for_lobby(&app, &fixture).await);
        assert!(fixture.wait_subscribed(Duration::from_secs(3)).await);

        let stage_event = Instant::now();
        fixture.start_post_game("PreEndOfGame", 7722);
        fixture.emit_phase("PreEndOfGame");
        assert!(wait_for_phase(&app, "PreEndOfGame", Duration::from_secs(1)).await);
        assert!(fixture
            .wait_for_request_after(
                "GET /lol-pre-end-of-game/v1/currentSequenceEvent",
                stage_event,
                Duration::from_secs(1),
            )
            .await
            .is_some());

        let ballot_event = Instant::now();
        fixture.publish_post_game_ballot();
        fixture.publish_post_game_ballot();
        let honor = fixture
            .wait_for_post_game_write(
                "/lol-honor/v1/honor",
                ballot_event,
                Duration::from_millis(1_200),
            )
            .await
            .expect(
                "ballot Create received during sequence read must be retried after writer release",
            );
        let submit = fixture
            .wait_for_post_game_write(
                "/lol-honor/v1/ballot",
                ballot_event,
                Duration::from_millis(1_400),
            )
            .await
            .expect("deferred event must submit the ballot without waiting for the 2s fallback");
        assert!(honor.duration_since(ballot_event) < Duration::from_millis(1_200));
        assert!(submit.duration_since(ballot_event) < Duration::from_millis(1_400));
        let writes = fixture.post_game_writes();
        assert_eq!(
            writes
                .iter()
                .filter(|(path, _)| path == "/lol-honor/v1/honor")
                .count(),
            1
        );
        assert_eq!(
            writes
                .iter()
                .filter(|(path, _)| path == "/lol-honor/v1/ballot")
                .count(),
            1
        );
        assert!(
            writes
                .iter()
                .all(|(path, _)| path != "/lol-lobby/v2/play-again"),
            "PreEndOfGame return is not due before its pinned 3.25s buffer"
        );
        eprintln!(
            "slow sequence fixture: ballot event -> honor {:?}, submit {:?}",
            honor.duration_since(ballot_event),
            submit.duration_since(ballot_event)
        );
        monitor.abort();
    });
}

#[test]
#[ignore = "requires a disposable Windows desktop runner; blocks return when the immediate revalidation omits the game ID"]
fn post_game_incomplete_successful_revalidation_requires_a_verified_ballot() {
    let fixture = ReadyFixture::start(450);
    let options = RoomOptions {
        play_again: true,
        ..RoomOptions::default()
    };
    let (app, _client) = app_with_runtime_options(&fixture, options);
    let app_handle = app.handle().clone();
    tauri::async_runtime::block_on(async {
        consume_startup_refresh(&app).await;
        let monitor = tauri::async_runtime::spawn(monitor(app_handle));
        assert!(wait_for_lobby(&app, &fixture).await);
        assert!(fixture.wait_subscribed(Duration::from_secs(3)).await);

        let stage_event = Instant::now();
        fixture.start_post_game("EndOfGame", 7733);
        fixture.emit_phase("EndOfGame");
        assert!(wait_for_phase(&app, "EndOfGame", Duration::from_secs(1)).await);
        assert!(fixture
            .wait_for_request_after(
                "GET /lol-honor-v2/v1/ballot/",
                stage_event,
                Duration::from_secs(1),
            )
            .await
            .is_some());
        assert!(
            fixture
                .wait_for_session_quiet(Duration::from_millis(100), Duration::from_millis(600))
                .await
        );

        let before = fixture.session_observations();
        let known_before = before
            .iter()
            .filter(|(phase, _, _)| phase == "EndOfGame")
            .count();
        assert!(
            known_before >= 2,
            "sample and initial action both used the matching ARAM ID: {before:?}"
        );
        assert!(before
            .iter()
            .filter(|(phase, _, _)| phase == "EndOfGame")
            .all(|(_, game_id, _)| *game_id == Some(7733)));

        // At the due sample, the 200 session read and initial action confirmation remain
        // complete; only return_if_current receives a body whose gameData has no gameId.
        fixture.make_nth_future_session_incomplete(3);
        let incomplete_at = fixture
            .wait_for_incomplete_end_game_session(Duration::from_secs(2))
            .await
            .expect("the immediate return revalidation must receive the incomplete 200 session");
        let after = fixture.session_observations();
        let last_three: Vec<_> = after
            .iter()
            .filter(|(phase, _, at)| phase == "EndOfGame" && *at >= before.last().unwrap().2)
            .map(|(_, game_id, _)| *game_id)
            .collect();
        assert!(
            last_three.ends_with(&[Some(7733), Some(7733), None]),
            "unexpected session sequence: {last_three:?}"
        );
        assert!(fixture
            .request_trace()
            .iter()
            .any(|entry| entry == "GET /lol-gameflow/v1/session -> 200 (authenticated=true)"));

        // Observe the immediate attempt, before the next normal retry tick can occur.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            fixture.post_game_writes().is_empty(),
            "an incomplete 200 return revalidation without any ballot must not POST Play Again"
        );
        assert!(incomplete_at >= stage_event);
        monitor.abort();
    });
}

#[test]
#[ignore = "requires a disposable Windows desktop runner; reproduces consecutive id-less ready checks"]
fn second_ready_check_after_accept_is_accepted_again_without_intermediate_champ_select() {
    let mut fixture = ReadyFixture::start(450);
    let (app, _client) = app_with_runtime(&fixture);
    let app_handle = app.handle().clone();
    tauri::async_runtime::block_on(async {
        consume_startup_refresh(&app).await;
        let monitor = tauri::async_runtime::spawn(monitor(app_handle));
        assert!(wait_for_lobby(&app, &fixture).await);
        assert!(fixture.wait_subscribed(Duration::from_secs(3)).await);

        fixture.ready_check();
        fixture.emit_phase("ReadyCheck");
        fixture.emit_ready("None");
        fixture.emit_ready("None");
        fixture
            .receive_post(Duration::from_millis(900))
            .await
            .expect("the first pending ready check should be accepted");
        assert_eq!(fixture.post_count(), 1);

        // Model the first response update, then a fresh id-less Delete/Create pair in the
        // same queue/party/phase. Search remains stale Accepted while the direct read is Pending.
        fixture.emit_ready("Accepted");
        fixture.requeue_ready_check_without_phase_change();
        assert_eq!(fixture.readiness(), ("Accepted".into(), "None".into()));
        fixture.emit_ready_delete_create();
        fixture.emit_ready("None");
        fixture.emit_ready("None");

        let started = Instant::now();
        let second_post = fixture
            .receive_post(Duration::from_millis(900))
            .await
            .expect(
                "a new Pending direct ready-check must be accepted despite stale search Accepted",
            );
        assert!(
            second_post.duration_since(started) < Duration::from_millis(900),
            "second ready-check acceptance missed the event wake bound"
        );
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(
            fixture.post_count(),
            2,
            "duplicate events must not accept twice"
        );
        assert_eq!(
            fixture.phase(),
            "ReadyCheck",
            "the scenario must have no intervening ChampSelect"
        );
        monitor.abort();
    });
}

#[test]
#[ignore = "requires a disposable Windows desktop runner; verifies a new pending round restarts its delay"]
fn new_pending_ready_check_restarts_accept_delay_after_delete_create() {
    let mut fixture = ReadyFixture::start(450);
    let (app, _client) = app_with_runtime(&fixture);
    app.state::<ClientRuntime>()
        .replace_room_options(
            &app.handle(),
            RoomOptions {
                accept: true,
                accept_delay: Duration::from_millis(1200),
                ..RoomOptions::default()
            },
        )
        .unwrap();
    let app_handle = app.handle().clone();
    tauri::async_runtime::block_on(async {
        consume_startup_refresh(&app).await;
        let monitor = tauri::async_runtime::spawn(monitor(app_handle));
        assert!(wait_for_lobby(&app, &fixture).await);
        assert!(fixture.wait_subscribed(Duration::from_secs(3)).await);

        let ready_reads_before = fixture.request_count("GET /lol-matchmaking/v1/ready-check");
        fixture.ready_check();
        fixture.emit_phase("ReadyCheck");
        fixture.emit_ready_event("Create", "None");
        assert!(
            wait_for_request_count(
                &fixture,
                "GET /lol-matchmaking/v1/ready-check",
                ready_reads_before + 1,
                Duration::from_secs(1),
            )
            .await
        );

        tokio::time::sleep(Duration::from_millis(450)).await;
        fixture.restart_pending_ready_check_without_phase_change();
        let requeued_at = Instant::now();
        fixture.emit_ready_delete_create();
        fixture.emit_ready("None");
        fixture.emit_ready("None");
        assert!(
            fixture
                .receive_post(Duration::from_millis(900))
                .await
                .is_none(),
            "the previous round's accept timer must not fire early for the new ready check"
        );
        let post = fixture
            .receive_post(Duration::from_millis(700))
            .await
            .expect("the new pending round should accept after its full delay");
        assert!(
            post.duration_since(requeued_at) >= Duration::from_millis(900),
            "new ready-check was accepted before its delay elapsed: {:?}",
            post.duration_since(requeued_at)
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            fixture.post_count(),
            1,
            "the new round must produce one accept POST"
        );
        monitor.abort();
    });
}

#[test]
#[ignore = "requires a disposable Windows desktop runner; rejects stale search readiness"]
fn missing_direct_ready_check_never_accepts_stale_search_pending() {
    let mut fixture = ReadyFixture::start(450);
    let (app, _client) = app_with_runtime(&fixture);
    let app_handle = app.handle().clone();
    tauri::async_runtime::block_on(async {
        consume_startup_refresh(&app).await;
        let monitor = tauri::async_runtime::spawn(monitor(app_handle));
        assert!(wait_for_lobby(&app, &fixture).await);
        assert!(fixture.wait_subscribed(Duration::from_secs(3)).await);

        fixture.ready_check();
        fixture.hide_direct_ready_check();
        let ready_reads_before = fixture.request_count("GET /lol-matchmaking/v1/ready-check");
        fixture.emit_phase("ReadyCheck");
        fixture.emit_ready_event("Create", "None");
        assert!(
            wait_for_request_count(
                &fixture,
                "GET /lol-matchmaking/v1/ready-check",
                ready_reads_before + 1,
                Duration::from_secs(1),
            )
            .await
        );

        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(
            fixture.post_count(),
            0,
            "a missing direct ready-check must not fall back to stale /search Pending"
        );
        monitor.abort();
    });
}
