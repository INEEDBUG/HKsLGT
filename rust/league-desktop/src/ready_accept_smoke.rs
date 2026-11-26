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
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Cursor, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tauri::Manager;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_tungstenite::tungstenite::{self, Message};

const PLAYER: &str = "11111111-2222-4333-8444-555555555555";
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
    response: String,
    slow_search_ms: u64,
    request_trace: Vec<String>,
    posts: Vec<Instant>,
}

#[derive(Clone)]
struct WsEvent {
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
    }

    fn set_classic(&self) {
        let mut scene = self.scene.lock().unwrap();
        scene.queue_id = 440;
        scene.phase = "ReadyCheck".into();
        scene.response = "None".into();
    }

    fn emit_phase(&self, phase: &'static str) {
        self.emit(WsEvent {
            uri: "/lol-gameflow/v1/gameflow-phase",
            data: json!(phase),
        });
    }

    fn emit_ready(&self, response: &'static str) {
        self.emit(WsEvent {
            uri: "/lol-matchmaking/v1/ready-check",
            data: json!({"state":"InProgress","playerResponse":response}),
        });
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
                    let payload = json!([8,"OnJsonApiEvent",{"uri":event.uri,"eventType":"Update","data":event.data}]);
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
    let (status, response, post_time) = if !authenticated {
        (401, Value::Null, None)
    } else if method == "POST" && path == "/lol-matchmaking/v1/ready-check/accept" {
        let now = Instant::now();
        let mut scene = scene.lock().unwrap();
        scene.posts.push(now);
        scene.response = "Accepted".into();
        (200, json!({}), Some(now))
    } else if method == "GET" {
        let scene = scene.lock().unwrap();
        if path == "/lol-matchmaking/v1/search" && scene.slow_search_ms > 0 {
            thread::sleep(Duration::from_millis(scene.slow_search_ms));
        }
        let mode = if scene.queue_id == 450 {
            "ARAM"
        } else {
            "CLASSIC"
        };
        let response = match path {
            "/lol-summoner/v1/current-summoner" => {
                json!({"puuid":PLAYER,"summonerId":41,"displayName":"Fixture"})
            }
            "/lol-chat/v1/me" => json!({"puuid":PLAYER,"platformId":"NA1","name":"Fixture"}),
            "/lol-gameflow/v1/gameflow-phase" => json!(scene.phase),
            "/lol-gameflow/v1/session" => {
                json!({"phase":scene.phase,"gameData":{"gameId":0,"isCustomGame":false,"queue":{"id":scene.queue_id,"gameMode":mode,"type":mode}}})
            }
            "/lol-lobby/v2/lobby" => {
                json!({"partyId":"fixture-party","gameConfig":{"queueId":scene.queue_id,"gameMode":mode,"isCustom":false},"localMember":{"puuid":PLAYER,"isLeader":true},"members":[{"puuid":PLAYER,"isLeader":true}],"invitations":[]})
            }
            "/lol-matchmaking/v1/search" => {
                json!({"queueId":scene.queue_id,"readyCheck":{"state":"InProgress","playerResponse":scene.response}})
            }
            "/lol-matchmaking/v1/ready-check" => {
                json!({"state":"InProgress","playerResponse":scene.response})
            }
            _ => Value::Null,
        };
        (if response.is_null() { 404 } else { 200 }, response, None)
    } else {
        (404, Value::Null, None)
    };
    scene.lock().unwrap().request_trace.push(format!(
        "{method} {path} -> {status} (authenticated={authenticated})"
    ));
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
        .replace_room_options(
            &app.handle(),
            RoomOptions {
                accept: true,
                ..RoomOptions::default()
            },
        )
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
