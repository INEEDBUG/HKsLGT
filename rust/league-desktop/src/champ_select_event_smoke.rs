//! Production WSS-to-monitor regression using only a synthetic local LCU peer.

use super::{
    check_renderer, click, set_preference, wait_for_popup, AppHandle, ClientRuntime, Manager,
    AUX_WINDOW,
};
use crate::client_discovery::LocalClient;
use rustls::{
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
    ServerConfig, ServerConnection, StreamOwned,
};
use serde_json::{json, Value};
use std::{
    io::{self, BufReader, Cursor, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tauri::WebviewWindow;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{self, Message};

const PLAYER: &str = "11111111-2222-4333-8444-555555555555";
const FIXTURE_AUTH: &str = "Basic cmlvdDpmaXh0dXJlLW9ubHk=";
const CHAMP_SELECT: &str = "/lol-champ-select/v1/session";
const SWAP_AHRI: &str = "/lol-champ-select/v1/session/bench/swap/103";
const FAST_REFRESH_LIMIT: Duration = Duration::from_millis(850);

pub(super) fn requested() -> bool {
    std::env::var_os("HKSLGT_SELECTION_EVENT_ONLY").is_some()
}

pub(super) struct SelectionFixture {
    pub port: u16,
    pub root: CertificateDer<'static>,
    pub client: Arc<LocalClient>,
    scene: Arc<Mutex<Scene>>,
    event_senders: Arc<Mutex<Vec<mpsc::Sender<String>>>>,
    subscribers: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    connections: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

struct Scene {
    http_phase: &'static str,
    flow_missing: bool,
    game_id: u64,
    queue_id: u32,
    champion: u32,
    bench: Vec<u32>,
    timer_ms: u64,
    selection_override: Option<(u16, Value)>,
    selection_reads: usize,
    flow_reads: usize,
    posts: Vec<String>,
}

impl SelectionFixture {
    pub(super) fn start() -> Self {
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
            http_phase: "ChampSelect",
            flow_missing: false,
            game_id: 7100,
            queue_id: 450,
            champion: 22,
            bench: Vec::new(),
            timer_ms: 30_000,
            selection_override: None,
            selection_reads: 0,
            flow_reads: 0,
            posts: Vec::new(),
        }));
        let event_senders = Arc::new(Mutex::new(Vec::new()));
        let subscribers = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let worker = {
            let (scene, event_senders, subscribers, stop, connections) = (
                Arc::clone(&scene),
                Arc::clone(&event_senders),
                Arc::clone(&subscribers),
                Arc::clone(&stop),
                Arc::clone(&connections),
            );
            let config = Arc::new(config);
            thread::spawn(move || {
                listener.set_nonblocking(true).unwrap();
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((socket, _)) => {
                            let (scene, event_senders, subscribers, stop, config) = (
                                Arc::clone(&scene),
                                Arc::clone(&event_senders),
                                Arc::clone(&subscribers),
                                Arc::clone(&stop),
                                Arc::clone(&config),
                            );
                            let task = thread::spawn(move || {
                                let _ = serve_connection(
                                    socket,
                                    config,
                                    scene,
                                    event_senders,
                                    subscribers,
                                    stop,
                                );
                            });
                            connections.lock().unwrap().push(task);
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        let client = Arc::new(LocalClient::fixture_at(port).expect("fixture listener identity"));
        assert!(
            client.alive(),
            "synthetic loopback server process stays alive"
        );
        assert!(
            client.target().is_ok(),
            "fixture port is owned by this process"
        );
        Self {
            port,
            root,
            client,
            scene,
            event_senders,
            subscribers,
            stop,
            worker: Some(worker),
            connections,
        }
    }

    pub(super) async fn wait_subscribed(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.subscribers.load(Ordering::Acquire) > 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.subscribers.load(Ordering::Acquire) > 0
    }

    pub(super) async fn wait_for_selection_reads(&self, count: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.scene.lock().unwrap().selection_reads >= count {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.scene.lock().unwrap().selection_reads >= count
    }

    async fn wait_for_flow_reads(&self, count: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.scene.lock().unwrap().flow_reads >= count {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.scene.lock().unwrap().flow_reads >= count
    }

    pub(super) fn selection_reads(&self) -> usize {
        self.scene.lock().unwrap().selection_reads
    }

    fn flow_reads(&self) -> usize {
        self.scene.lock().unwrap().flow_reads
    }

    pub(super) fn set_bench(&self, bench: &[u32], timer_ms: u64, event_type: &str) -> Instant {
        let mut scene = self.scene.lock().unwrap();
        scene.bench = bench.to_vec();
        scene.timer_ms = timer_ms;
        let payload = if event_type == "Delete" {
            Value::Null
        } else {
            session(&scene)
        };
        drop(scene);
        let event = json!([8, "OnJsonApiEvent", {
            "uri": CHAMP_SELECT,
            "eventType": event_type,
            "data": payload
        }])
        .to_string();
        self.emit(event)
    }

    fn emit(&self, event: String) -> Instant {
        let started = Instant::now();
        if let Ok(senders) = self.event_senders.lock() {
            for sender in senders.iter() {
                let _ = sender.send(event.clone());
            }
        }
        started
    }
}

impl Drop for SelectionFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Ok(mut connections) = self.connections.lock() {
            for connection in connections.drain(..) {
                let _ = connection.join();
            }
        }
    }
}

pub(super) async fn check(app: &AppHandle) {
    let fixture_state = app.state::<Option<Arc<SelectionFixture>>>();
    let fixture = Arc::clone(
        fixture_state
            .as_ref()
            .expect("selection event fixture is installed"),
    );
    set_preference(app, AUX_WINDOW, "enabled", true);
    set_preference(app, AUX_WINDOW, "autoShow", true);

    let runtime = app.state::<ClientRuntime>();
    runtime.use_fixture_clients(vec![Arc::clone(&fixture.client)]);
    runtime
        .select(app, Some(fixture.client.instance))
        .expect("select only the verified synthetic LCU process");
    let monitor = tauri::async_runtime::spawn(crate::client_runtime::monitor(app.clone()));
    let aux = wait_for_popup(app, "aux", || {}).await;
    check_renderer(
        &aux,
        r#"document.querySelector('.aux-activity[data-phase="ChampSelect"]') !== null"#,
        "synthetic ARAM ChampSelect rendered in the real Wry mini-window",
        || {},
    )
    .await;
    check_renderer(
        &aux,
        r#"document.querySelector('.current-champion[data-champion-id="22"] img')?.naturalWidth > 1"#,
        "synthetic fixture portrait is decoded before measuring bench updates",
        || {},
    )
    .await;
    assert!(
        fixture.wait_subscribed(Duration::from_secs(3)).await,
        "the client uses the authenticated WSS subscription"
    );

    // Align the measured event just after an ordinary poll so the old two-second fallback
    // cannot accidentally satisfy the fast-refresh window.
    let before_poll = fixture.selection_reads();
    assert!(
        fixture
            .wait_for_selection_reads(before_poll + 1, Duration::from_secs(3))
            .await
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        bench_visible(&aux, 103, Duration::from_millis(40))
            .await
            .is_none(),
        "fixture starts with an empty reserve bench"
    );

    let reads_before_event = fixture.selection_reads();
    let started = fixture.set_bench(&[103], 25_000, "Update");
    let fast = bench_visible(&aux, 103, FAST_REFRESH_LIMIT).await;
    if fast.is_some() {
        eprintln!(
            "Selection bench WSS event-to-visible latency: {:?} (fast bound {FAST_REFRESH_LIMIT:?})",
            started.elapsed()
        );
    } else {
        let fallback = bench_visible(&aux, 103, Duration::from_millis(2200))
            .await
            .is_some();
        let poll_read = fixture
            .wait_for_selection_reads(reads_before_event + 1, Duration::from_millis(3500))
            .await;
        let bench_card = bench_card_present(&aux, 103).await;
        let decoded_image = bench_visible(&aux, 103, Duration::from_millis(100))
            .await
            .is_some();
        let selection_active = matches!(
            runtime.aux_observation(),
            crate::desktop::aux_window::AuxObservation::Active {
                stage: crate::desktop::aux_window::AuxStage::Selection,
                ..
            }
        );
        eprintln!(
            "Selection bench WSS event-to-visible latency: fast bound missed; 2s fallback image={fallback}, poll read observed={poll_read}, session_reads={} (+{}), flow_reads={}, bench_card={bench_card}, decoded_image={decoded_image}, selection_active={selection_active}, total elapsed={:?}",
            fixture.selection_reads(),
            fixture.selection_reads().saturating_sub(reads_before_event),
            fixture.flow_reads(),
            started.elapsed()
        );
    }
    assert!(fast.is_some(), "a changed reserve bench should refresh the visible mini-window under {FAST_REFRESH_LIMIT:?}");

    // Same list with timer-only changes must not schedule another selection read.
    let reads_before_timer = fixture.selection_reads();
    fixture.set_bench(&[103], 20_000, "Update");
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        fixture.selection_reads(),
        reads_before_timer,
        "timer-only session updates do not sample again"
    );

    let reads_before_burst = fixture.selection_reads();
    fixture.set_bench(&[22, 103], 19_000, "Update");
    fixture.set_bench(&[103, 22], 18_000, "Update");
    assert!(
        bench_order_matches(&aux, &[103, 22], Duration::from_millis(700)).await,
        "a short burst refreshes the latest bench order"
    );
    assert_eq!(
        fixture.selection_reads().saturating_sub(reads_before_burst),
        1,
        "a burst of bench changes is coalesced into one fresh session read"
    );

    check_missing_http_session(app, &fixture, &aux).await;

    // A disabled companion is not an active selection demand, so its bench events do not
    // wake the normal sampling loop; its bounded polling fallback remains available.
    set_preference(app, AUX_WINDOW, "enabled", false);
    runtime.refresh_soon();
    let flow_before = fixture.flow_reads();
    assert!(
        fixture
            .wait_for_flow_reads(flow_before + 1, Duration::from_secs(3))
            .await
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let flow_reads = fixture.flow_reads();
    fixture.set_bench(&[22], 18_000, "Update");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        fixture.flow_reads(),
        flow_reads,
        "disabled selection demand does not trigger a monitor refresh"
    );
    monitor.abort();
    let _ = aux.close();
}

async fn check_missing_http_session(
    app: &AppHandle,
    fixture: &SelectionFixture,
    aux: &WebviewWindow,
) {
    let runtime = app.state::<ClientRuntime>();
    let hwnd = aux.hwnd().unwrap();
    {
        let mut scene = fixture.scene.lock().unwrap();
        scene.game_id = 7101;
        scene.queue_id = 2400;
        scene.bench.clear();
        scene.timer_ms = 28_000;
    }
    runtime.refresh_soon();
    check_renderer(aux, r#"
        document.querySelector('.aux-activity[data-phase=ChampSelect] .current-champion[data-champion-id="22"] img')?.naturalWidth > 1 &&
        !document.querySelector('.bench [data-champion-id]')
    "#, "KIWI game confirmed with an empty bench before withholding its HTTP session", || {}).await;
    let (_, state) = runtime
        .fields(crate::lcu::aux_activity::NAMESPACE, "state")
        .unwrap()
        .unwrap();
    assert_eq!(state["view"]["gameMode"], "KIWI");

    let before = {
        let mut scene = fixture.scene.lock().unwrap();
        scene.selection_override = Some((404, Value::Null));
        scene.selection_reads
    };
    fixture.set_bench(&[103], 18_000, "Update");
    assert!(
        fixture
            .wait_for_selection_reads(before + 1, Duration::from_secs(3))
            .await,
        "the runtime attempted a fresh HTTP GET while the authenticated event had the roster"
    );
    check_renderer(aux, r#"
        document.querySelector('.current-champion[data-champion-id="22"] img')?.naturalWidth > 1 &&
        document.querySelector('.bench [data-champion-id="103"][aria-disabled=true] img')?.naturalWidth > 1 &&
        /^18\s*(秒|s)$/.test(document.querySelector('.aux-draft-timer')?.textContent.trim()) &&
        !!document.querySelector('.aux-draft-waiting')
    "#, "authenticated KIWI roster and timer rendered read-only during the HTTP 404", || {}).await;
    aux.eval(
        r#"
        window.__TAURI__.core.invoke('league_call', {
            namespace: 'aux-activity-main', method: 'swapBenchChampion', args: [103]
        }).then(result => document.documentElement.dataset.eventRosterWrite =
            result.success === false ? 'blocked' : 'unexpected')
    "#,
    )
    .unwrap();
    check_renderer(
        aux,
        "document.documentElement.dataset.eventRosterWrite === 'blocked'",
        "event-only roster cannot authorize a direct bench write",
        || {},
    )
    .await;
    assert!(fixture.scene.lock().unwrap().posts.is_empty());

    fixture.scene.lock().unwrap().selection_override = Some((200, json!({})));
    fixture.set_bench(&[103, 55, 1, 2, 4, 12], 18_000, "Update");
    check_renderer(
        aux,
        r#"(() => {
            const slots = [...document.querySelectorAll('.bench [data-champion-id][aria-disabled=true] img')];
            return slots.length === 6 && slots.every(img => img.naturalWidth > 1) &&
                !document.querySelector('.aux-draft-confirming');
        })()"#,
        "an HTTP 200 with an empty body object cannot hide the decoded authenticated event bench",
        || {},
    )
    .await;
    {
        let mut scene = fixture.scene.lock().unwrap();
        scene.http_phase = "ReadyCheck";
        scene.flow_missing = true;
    }
    fixture.emit(
        json!([8, "OnJsonApiEvent", {
            "uri": "/lol-gameflow/v1/gameflow-phase", "eventType": "Update", "data": "ChampSelect"
        }])
        .to_string(),
    );
    let complete_event = fixture.set_bench(&[103, 55, 1, 2, 4, 17], 18_000, "Update");
    check_renderer(
        aux,
        r#"(() => {
            const slots = [...document.querySelectorAll('.bench [data-champion-id][aria-disabled=true] img')];
            return slots.length === 6 && slots.every(img => img.naturalWidth > 1) &&
                document.querySelector('.bench [data-champion-id="17"] img')?.naturalWidth > 1 &&
                document.querySelector('.current-champion[data-champion-id="22"] img')?.naturalWidth > 1 &&
                !document.querySelector('.aux-draft-confirming');
        })()"#,
        "both authenticated draft events keep the Mini visible when the HTTP phase lags",
        || {},
    )
    .await;
    let elapsed = complete_event.elapsed();
    assert!(
        elapsed <= Duration::from_secs(1),
        "authenticated selection event took {elapsed:?}"
    );
    println!("HKSLGT_SELECTION_EVENT_SEVEN_PORTRAITS: {elapsed:?}");
    assert!(fixture.scene.lock().unwrap().posts.is_empty());
    {
        let mut scene = fixture.scene.lock().unwrap();
        scene.http_phase = "ChampSelect";
        scene.flow_missing = false;
        scene.selection_override = None;
    }
    fixture.set_bench(&[103], 18_000, "Update");
    click(aux, r#".bench [data-champion-id="103"]"#).await;
    check_renderer(
        aux,
        r#"
        document.querySelector('.current-champion[data-champion-id="103"] img')?.naturalWidth > 1 &&
        document.querySelector('.bench [data-champion-id="22"] img')?.naturalWidth > 1
    "#,
        "fresh HTTP session re-enables the bench and reads back the one accepted swap",
        || {},
    )
    .await;
    assert_eq!(fixture.scene.lock().unwrap().posts, [SWAP_AHRI]);
    assert_eq!(aux.hwnd().unwrap(), hwnd);

    fixture.scene.lock().unwrap().selection_override = Some((404, Value::Null));
    fixture.set_bench(&[22], 18_000, "Update");
    check_renderer(
        aux,
        r#"document.querySelector('.bench [data-champion-id="22"][aria-disabled=true] img')?.naturalWidth > 1"#,
        "the updated event roster remains in the cache before the Delete",
        || {},
    )
    .await;
    fixture.set_bench(&[], 18_000, "Delete");
    assert!(
        bench_order_matches(aux, &[], FAST_REFRESH_LIMIT).await,
        "a WSS session Delete clears the old bench before polling or cache expiry"
    );
    assert_eq!(fixture.scene.lock().unwrap().posts, [SWAP_AHRI]);
    println!("HKSLGT_SELECTION_EVENT_HTTP_FALLBACK_PASS: missing/empty selection and a lagging phase preserve the event roster read-only; one recovered HTTPS swap; Delete clears the bench");
}

async fn bench_visible(
    window: &WebviewWindow,
    champion: u32,
    timeout: Duration,
) -> Option<Duration> {
    let started = Instant::now();
    let deadline = started + timeout;
    let script = format!(
        "document.querySelector('.bench [data-champion-id=\"{champion}\"] img')?.naturalWidth > 1"
    );
    while Instant::now() < deadline {
        let (send, receive) = tokio::sync::oneshot::channel();
        let send = Mutex::new(Some(send));
        if window
            .eval_with_callback(&script, move |value| {
                if let Some(send) = send.lock().unwrap().take() {
                    let _ = send.send(value);
                }
            })
            .is_ok()
        {
            if let Ok(Ok(result)) = tokio::time::timeout(Duration::from_millis(75), receive).await {
                if serde_json::from_str::<bool>(&result).is_ok_and(|visible| visible) {
                    return Some(started.elapsed());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

async fn bench_card_present(window: &WebviewWindow, champion: u32) -> bool {
    let script =
        format!("document.querySelector('.bench [data-champion-id=\"{champion}\"]') !== null");
    let (send, receive) = tokio::sync::oneshot::channel();
    let send = Mutex::new(Some(send));
    window
        .eval_with_callback(&script, move |value| {
            if let Some(send) = send.lock().unwrap().take() {
                let _ = send.send(value);
            }
        })
        .expect("query current synthetic bench card");
    tokio::time::timeout(Duration::from_millis(200), receive)
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|value| serde_json::from_str::<bool>(&value).ok())
        .unwrap_or(false)
}

async fn bench_order_matches(window: &WebviewWindow, champions: &[u32], timeout: Duration) -> bool {
    let expected = serde_json::to_string(champions).expect("numeric fixture champion IDs");
    let script = format!(
        "JSON.stringify([...document.querySelectorAll('.bench [data-champion-id]')].map((node) => Number(node.getAttribute('data-champion-id')))) === {}",
        json!(expected)
    );
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let (send, receive) = tokio::sync::oneshot::channel();
        let send = Mutex::new(Some(send));
        if window
            .eval_with_callback(&script, move |value| {
                if let Some(send) = send.lock().unwrap().take() {
                    let _ = send.send(value);
                }
            })
            .is_ok()
        {
            if let Ok(Ok(result)) = tokio::time::timeout(Duration::from_millis(75), receive).await {
                if serde_json::from_str::<bool>(&result).is_ok_and(|matches| matches) {
                    return true;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn session(scene: &Scene) -> Value {
    json!({
        "queueId": scene.queue_id,
        "gameId": scene.game_id,
        "isSpectating": false,
        "isCustomGame": false,
        "localPlayerCellId": 1,
        "myTeam": [{"cellId": 1, "puuid": PLAYER, "championId": scene.champion, "championPickIntent": 0}],
        "theirTeam": [],
        "benchEnabled": true,
        "benchChampions": scene.bench.iter().map(|id| json!({"championId": id, "isPriority": false})).collect::<Vec<_>>(),
        "allowRerolling": true,
        "rerollsRemaining": 1,
        "allowSubsetChampionPicks": false,
        "timer": {"phase": "FINALIZATION", "adjustedTimeLeftInPhase": scene.timer_ms},
        "actions": []
    })
}

fn serve_connection(
    socket: TcpStream,
    config: Arc<ServerConfig>,
    scene: Arc<Mutex<Scene>>,
    event_senders: Arc<Mutex<Vec<mpsc::Sender<String>>>>,
    subscribers: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(Duration::from_secs(5))).ok();
    socket.set_write_timeout(Some(Duration::from_secs(2))).ok();
    let mut stream = StreamOwned::new(ServerConnection::new(config).unwrap(), socket);
    let mut bytes = Vec::new();
    let mut byte = [0; 1];
    while !bytes.ends_with(b"\r\n\r\n") && bytes.len() < 16 * 1024 {
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
    }
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
        if !authorized(&header) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "fixture WSS auth rejected",
            ));
        }
        serve_websocket(prefixed, event_senders, subscribers, stop)
    } else {
        serve_http(prefixed, &header, scene)
    }
}

fn authorized(header: &str) -> bool {
    header.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value.trim() == FIXTURE_AUTH
        })
    })
}

fn serve_websocket(
    stream: PrefixStream<StreamOwned<ServerConnection, TcpStream>>,
    event_senders: Arc<Mutex<Vec<mpsc::Sender<String>>>>,
    subscribers: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    let mut socket = tungstenite::accept(stream).map_err(io::Error::other)?;
    let (sender, receiver) = mpsc::channel();
    event_senders.lock().unwrap().push(sender);
    let mut subscribed = false;
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        while let Ok(event) = receiver.try_recv() {
            if subscribed && socket.send(Message::Text(event.into())).is_err() {
                return Ok(());
            }
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(&text) {
                    if parts.first() == Some(&json!(5))
                        && parts.get(1).and_then(Value::as_str) == Some("OnJsonApiEvent")
                    {
                        if !subscribed {
                            subscribers.fetch_add(1, Ordering::AcqRel);
                            subscribed = true;
                        }
                    }
                }
            }
            Ok(Message::Ping(data)) => {
                let _ = socket.send(Message::Pong(data));
            }
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
    if subscribed {
        subscribers.fetch_sub(1, Ordering::AcqRel);
    }
    let _ = socket.close(None);
    Ok(())
}

fn serve_http<S: Read + Write>(
    stream: S,
    header: &str,
    scene: Arc<Mutex<Scene>>,
) -> io::Result<()> {
    let mut request = header.lines().next().unwrap_or("").split_whitespace();
    let method = request.next().unwrap_or("");
    let path = request.next().unwrap_or("");
    let content_length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let mut reader = BufReader::new(stream);
    if content_length > 65_536 {
        return Err(io::Error::other("unexpected fixture request size"));
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;
    let authenticated = authorized(header);
    let (status, content_type, response) = if !authenticated {
        (
            401,
            "application/json",
            serde_json::to_vec(&json!({"error":"fixture auth required"}))?,
        )
    } else if method == "GET"
        && matches!(
            path,
            "/lol-game-data/assets/v1/champion-icons/22.png"
                | "/lol-game-data/assets/v1/champion-icons/103.png"
                | "/lol-game-data/assets/v1/champion-icons/55.png"
                | "/lol-game-data/assets/v1/champion-icons/1.png"
                | "/lol-game-data/assets/v1/champion-icons/2.png"
                | "/lol-game-data/assets/v1/champion-icons/4.png"
                | "/lol-game-data/assets/v1/champion-icons/12.png"
                | "/lol-game-data/assets/v1/champion-icons/17.png"
        )
    {
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAMAAAACCAIAAAASFvFNAAAAFUlEQVR4nGO4pMEvt8BNK+8EA5wFAEKIBtsqErd8AAAAAElFTkSuQmCC")
            .expect("known valid 3x2 synthetic PNG");
        (200, "image/png", png)
    } else {
        let mut scene = scene.lock().unwrap();
        let result = match (method, path) {
            ("GET", "/lol-gameflow/v1/gameflow-phase") => (200, json!(scene.http_phase)),
            ("GET", "/lol-gameflow/v1/session") if scene.flow_missing => (404, Value::Null),
            ("GET", "/lol-gameflow/v1/session") => {
                scene.flow_reads += 1;
                let mode = if scene.queue_id == 2400 {
                    "KIWI"
                } else {
                    "ARAM"
                };
                (
                    200,
                    json!({
                        "phase":"ChampSelect",
                        "gameData":{"gameId":scene.game_id,"isCustomGame":false,"queue":{"id":scene.queue_id,"name":mode,"gameMode":mode,"type":mode}},
                        "map":{"name":"Howling Abyss","gameMode":mode,"assets":{}}
                    }),
                )
            }
            ("GET", "/lol-summoner/v1/current-summoner") => (
                200,
                json!({"puuid":PLAYER,"summonerId":1,"gameName":"Synthetic","tagLine":"TEST"}),
            ),
            ("GET", CHAMP_SELECT) => {
                scene.selection_reads += 1;
                scene
                    .selection_override
                    .clone()
                    .unwrap_or_else(|| (200, session(&scene)))
            }
            ("GET", "/lol-champ-select/v1/current-champion") => (200, json!(scene.champion)),
            ("GET", "/lol-champ-select/v1/pickable-champion-ids") => (200, json!([22, 103])),
            ("POST", path) => {
                scene.posts.push(path.to_owned());
                if path == SWAP_AHRI && scene.selection_override.is_none() && scene.bench == [103] {
                    scene.bench = vec![scene.champion];
                    scene.champion = 103;
                    (204, Value::Null)
                } else {
                    (409, Value::Null)
                }
            }
            ("GET", "/lol-lobby-team-builder/champ-select/v1/subset-champion-list") => {
                (200, json!([]))
            }
            ("GET", "/lol-game-data/assets/v1/champion-summary.json") => (
                200,
                json!([
                    {"id":22,"name":"Synthetic Ashe","alias":"Ashe","squarePortraitPath":"/lol-game-data/assets/v1/champion-icons/22.png"},
                    {"id":103,"name":"Synthetic Ahri","alias":"Ahri","squarePortraitPath":"/lol-game-data/assets/v1/champion-icons/103.png"}
                ]),
            ),
            ("GET", "/lol-game-data/assets/v1/perks.json")
            | ("GET", "/lol-game-data/assets/v1/summoner-spells.json")
            | ("GET", "/lol-game-data/assets/v1/items.json")
            | ("GET", "/lol-game-data/assets/v1/cherry-augments.json") => (200, json!([])),
            ("GET", "/lol-game-data/assets/v1/perkstyles.json") => {
                (200, json!({"schemaVersion":1,"styles":[]}))
            }
            ("GET", "/lol-game-data/assets/v1/queues.json") => (
                200,
                json!([
                    {"id":450,"name":"Synthetic ARAM"}, {"id":2400,"name":"Synthetic KIWI"}
                ]),
            ),
            _ => (
                404,
                json!({"error":"not present in synthetic selection fixture"}),
            ),
        };
        let response = if result.0 == 204 {
            Vec::new()
        } else {
            serde_json::to_vec(&result.1)?
        };
        (result.0, "application/json", response)
    };
    let mut stream = reader.into_inner();
    write!(
        stream,
        "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
    )?;
    stream.write_all(&response)?;
    stream.flush()
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
