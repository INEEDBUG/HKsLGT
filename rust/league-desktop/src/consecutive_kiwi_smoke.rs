//! Two League drafts on the same verified HTTPS connection and native Mini window.
use super::{
    assert_color_portrait, check_renderer, click, mini_client_fixture::PLAYER,
    partial_kiwi_selection_smoke::DraftFixture, reconcile_companions, set_preference,
    wait_for_close, wait_for_popup, AppHandle, ClientRuntime, Manager, MiniClient, WebviewWindow,
    AUX_WINDOW, OPGG_WINDOW, RESG_NAMESPACE,
};
use crate::client_discovery::LocalClient;
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const FIRST_GAME: u64 = 8521;
const SECOND_GAME: u64 = 8522;
const KIWI_QUEUE: u32 = 2400;

pub(super) fn requested() -> bool {
    std::env::var_os("HKSLGT_CONSECUTIVE_KIWI_ONLY").is_some()
}

pub(super) struct RoomFixture {
    elapsed_seconds: f64,
    search_reads: usize,
}

impl RoomFixture {
    pub(super) fn get(&mut self, path: &str, phase: &str, response: &str) -> Option<Value> {
        Some(match path {
            "/lol-lobby/v2/lobby" => json!({
                "partyId":"consecutive-fixture-party","gameConfig":{"queueId":KIWI_QUEUE,"gameMode":"KIWI","isCustom":false},
                "localMember":{"puuid":PLAYER,"isLeader":true},
                "members":[{"puuid":PLAYER,"isLeader":true}],"invitations":[]
            }),
            "/lol-matchmaking/v1/search" => {
                self.search_reads += 1;
                json!({
                    "queueId":KIWI_QUEUE,"searchState":if phase == "Lobby" {"Invalid"} else {"Searching"},
                    "timeInQueue":self.elapsed_seconds,"estimatedQueueTime":54.25,
                    "readyCheck":{"state":"InProgress","playerResponse":response}
                })
            }
            "/lol-champ-select/v1/pickable-champion-ids" => json!([22, 55, 103, 136]),
            _ => return None,
        })
    }
}

fn flow(phase: &str, game_id: u64, queue: u32, mode: &str) -> Value {
    json!({"phase":phase,"gameData":{"gameId":game_id,"isCustomGame":false,
        "queue":{"id":queue,"gameMode":mode}}})
}

fn kiwi_flow(phase: &str, game_id: u64) -> Value {
    flow(phase, game_id, KIWI_QUEUE, "KIWI")
}

fn selection(game_id: u64, champion: u32, bench: u32, remaining_ms: u32) -> Value {
    json!({"gameId":game_id,"queueId":KIWI_QUEUE,"localPlayerCellId":0,
        "isCustomGame":false,"isSpectating":false,
        "myTeam":[{"cellId":0,"puuid":PLAYER,"championId":champion}],
        "benchEnabled":true,"benchChampions":[{"championId":bench}],
        "timer":{"phase":"FINALIZATION","isInfinite":false,"adjustedTimeLeftInPhase":remaining_ms}
    })
}

async fn sample(
    app: &AppHandle,
    client: &Arc<LocalClient>,
    phase: &'static str,
    draft: DraftFixture,
) {
    let server = app.state::<MiniClient>();
    {
        let mut scene = server.scene.lock().unwrap();
        scene.phase = phase;
        if phase == "ReadyCheck" {
            scene.response = "None";
        }
        scene.partial_kiwi = Some(draft);
    }
    app.state::<ClientRuntime>()
        .sample_fixture(app, client)
        .await;
    reconcile_companions(app);
}

async fn room(
    app: &AppHandle,
    client: &Arc<LocalClient>,
    aux: &WebviewWindow,
    phase: &'static str,
) {
    sample(
        app,
        client,
        phase,
        DraftFixture {
            gameflow: Some(kiwi_flow(phase, 0)),
            ..Default::default()
        },
    )
    .await;
    check_renderer(
        aux,
        &format!(
            "document.querySelector('.aux-activity')?.dataset.phase === {}",
            json!(phase)
        ),
        phase,
        || {},
    )
    .await;
    if phase == "ReadyCheck" {
        click(aux, "[data-action=accept-ready-check]").await;
        let server = app.state::<MiniClient>();
        tokio::time::timeout(Duration::from_secs(4), async {
            while server.scene.lock().unwrap().response != "Accepted" {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the normal ReadyCheck button reached the synthetic League client");
        app.state::<ClientRuntime>()
            .sample_fixture(app, client)
            .await;
        check_renderer(
            aux,
            "!!document.querySelector('.ready-response')",
            "the accepted server response",
            || {},
        )
        .await;
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClockReading {
    seconds: f64,
    at_ms: f64,
    eta: bool,
}

async fn queue_clock(
    app: &AppHandle,
    client: &Arc<LocalClient>,
    aux: &WebviewWindow,
) -> (Vec<ClockReading>, [usize; 3]) {
    let server = app.state::<MiniClient>();
    aux.eval(r#"(() => {
        const caption = document.querySelector('.queue-time');
        const values = window.__hkslgtQueueClock = [];
        window.__hkslgtQueueBeforePolls = [];
        const read = () => {
            const label = caption.textContent;
            const seconds = Number.parseFloat(label);
            const eta = label.includes('/ 54.3 s');
            const last = values.at(-1);
            if (Number.isFinite(seconds) && (last?.seconds !== seconds || last?.eta !== eta)) {
                values.push({ seconds, atMs: performance.now(), eta });
            }
        };
        window.__hkslgtQueueObserver = new MutationObserver(read);
        window.__hkslgtQueueObserver.observe(caption, {subtree:true, childList:true, characterData:true});
        read();
    })()"#).unwrap();
    check_renderer(
        aux,
        "window.__hkslgtQueueClock?.length >= 1",
        "capture the real elapsed display",
        || {},
    )
    .await;
    let initial_reads = server
        .scene
        .lock()
        .unwrap()
        .consecutive_room
        .as_ref()
        .unwrap()
        .search_reads;
    let started = tokio::time::Instant::now();
    for (index, elapsed) in [12.1, 14.2, 16.3].into_iter().enumerate() {
        let poll = index + 1;
        tokio::time::sleep_until(started + Duration::from_millis(2100 * poll as u64)).await;
        aux.eval("window.__hkslgtQueueBeforePolls.push(window.__hkslgtQueueClock.length)")
            .unwrap();
        check_renderer(
            aux,
            &format!("window.__hkslgtQueueBeforePolls.length === {poll}"),
            "mark the samples before the next HTTPS poll",
            || {},
        )
        .await;
        server
            .scene
            .lock()
            .unwrap()
            .consecutive_room
            .as_mut()
            .unwrap()
            .elapsed_seconds = elapsed;
        app.state::<ClientRuntime>()
            .sample_fixture(app, client)
            .await;
        check_renderer(
            aux,
            &format!(
                "Number.parseFloat(document.querySelector('.queue-time')?.textContent) >= {}",
                elapsed.floor()
            ),
            "forward elapsed after the live poll",
            || {},
        )
        .await;
    }
    let (send, receive) = tokio::sync::oneshot::channel();
    let send = Mutex::new(Some(send));
    aux.eval_with_callback("(() => { window.__hkslgtQueueObserver.disconnect(); return [window.__hkslgtQueueClock, window.__hkslgtQueueBeforePolls] })()", move |raw| {
        if let Some(send) = send.lock().unwrap().take() {
            let _ = send.send(raw);
        }
    }).unwrap();
    let (values, before_polls): (Vec<ClockReading>, [usize; 3]) = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(3), receive)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let reads = server
        .scene
        .lock()
        .unwrap()
        .consecutive_room
        .as_ref()
        .unwrap()
        .search_reads
        - initial_reads;
    assert_eq!(
        reads, 3,
        "the displayed clock must not create more HTTPS reads than the three paced polls"
    );
    let seconds: Vec<_> = values.iter().map(|value| value.seconds).collect();
    println!("HKSLGT_CONSECUTIVE_KIWI_CLOCK: seconds={seconds:?} beforePolls={before_polls:?} httpsReads={reads}");
    (values, before_polls)
}

pub(super) async fn check(app: &AppHandle) {
    set_preference(app, AUX_WINDOW, "enabled", true);
    set_preference(app, OPGG_WINDOW, "enabled", false);
    set_preference(app, RESG_NAMESPACE, "enabled", false);
    let server = app.state::<MiniClient>();
    server.scene.lock().unwrap().consecutive_room = Some(RoomFixture {
        elapsed_seconds: 10.0,
        search_reads: 0,
    });
    let client =
        Arc::new(LocalClient::fixture_at(server.port).expect("real synthetic LCU connection"));
    sample(
        app,
        &client,
        "Lobby",
        DraftFixture {
            gameflow: Some(kiwi_flow("Lobby", 0)),
            ..Default::default()
        },
    )
    .await;
    let aux = wait_for_popup(app, "aux", || {}).await;
    let hwnd = aux.hwnd().unwrap();
    check_renderer(
        &aux,
        "!!document.querySelector('[data-phase=Lobby]')",
        "initial KIWI room",
        || {},
    )
    .await;
    room(app, &client, &aux, "Matchmaking").await;
    room(app, &client, &aux, "ReadyCheck").await;
    sample(
        app,
        &client,
        "ChampSelect",
        DraftFixture {
            gameflow: Some(kiwi_flow("ChampSelect", FIRST_GAME)),
            selection: Some(selection(FIRST_GAME, 22, 103, 20_000)),
            current_champion: Some(22),
            ..Default::default()
        },
    )
    .await;
    check_renderer(
        &aux,
        r#"
        document.querySelector('.current-champion[data-champion-id="22"] img')?.naturalWidth > 1 &&
        document.querySelector('.bench [data-champion-id="103"] img')?.naturalWidth > 1
    "#,
        "first KIWI draft with Ashe and Ahri",
        || {},
    )
    .await;
    assert_color_portrait(&aux, ".current-champion[data-champion-id='22'] img").await;

    let current_first = || DraftFixture {
        gameflow: Some(kiwi_flow("ChampSelect", FIRST_GAME)),
        selection: Some(selection(FIRST_GAME, 22, 103, 18_000)),
        current_champion: Some(22),
        ..Default::default()
    };
    sample(app, &client, "ReadyCheck", current_first()).await;
    check_renderer(
        &aux,
        "!!document.querySelector('.aux-activity[data-phase=ChampSelect] .current-champion[data-champion-id=\"22\"]')",
        "late ReadyCheck does not retire a corroborated first draft",
        || {},
    )
    .await;
    sample(
        app,
        &client,
        "ChampSelect",
        DraftFixture {
            gameflow: Some(kiwi_flow("ChampSelect", FIRST_GAME)),
            selection: Some(selection(SECOND_GAME, 55, 136, 5_000)),
            current_champion: Some(55),
            ..Default::default()
        },
    )
    .await;
    let runtime = app.state::<ClientRuntime>();
    let (_, disputed) = runtime
        .fields(crate::lcu::aux_activity::NAMESPACE, "state")
        .unwrap()
        .unwrap();
    assert_ne!(
        disputed["view"]["championId"], 55,
        "the second draft cannot replace an unretired first draft"
    );
    check_renderer(
        &aux,
        "!!document.querySelector('.aux-draft-confirming') && !document.querySelector('.current-champion[data-champion-id=\"55\"]')",
        "unknown game conflict cannot render the second draft before the first exits",
        || {},
    )
    .await;
    sample(app, &client, "ChampSelect", current_first()).await;
    check_renderer(
        &aux,
        "!!document.querySelector('.current-champion[data-champion-id=\"22\"]')",
        "the first draft returns after its own session agrees again",
        || {},
    )
    .await;

    sample(
        app,
        &client,
        "EndOfGame",
        DraftFixture {
            gameflow: Some(kiwi_flow("EndOfGame", FIRST_GAME)),
            ..Default::default()
        },
    )
    .await;
    wait_for_close(app, "aux", || {}).await;
    sample(
        app,
        &client,
        "Lobby",
        DraftFixture {
            gameflow: Some(kiwi_flow("Lobby", 0)),
            ..Default::default()
        },
    )
    .await;
    let (_, second_room) = runtime
        .fields(crate::lcu::aux_activity::NAMESPACE, "state")
        .unwrap()
        .unwrap();
    assert_eq!(
        second_room["view"]["phase"], "Lobby",
        "the next room is eligible to show after the first draft retires"
    );
    let again = wait_for_popup(app, "aux", || {}).await;
    assert_eq!(
        again.hwnd().unwrap(),
        hwnd,
        "return to the same native Mini"
    );
    check_renderer(&aux, "!!document.querySelector('[data-phase=Lobby]') && !document.querySelector('.current-champion')", "return to room without retaining the first draft", || {}).await;
    room(app, &client, &aux, "Matchmaking").await;
    let (clock, before_polls) = queue_clock(app, &client, &aux).await;
    room(app, &client, &aux, "ReadyCheck").await;

    for (label, prior_flow) in [
        (
            "stale ChampSelect from the first game",
            kiwi_flow("ChampSelect", FIRST_GAME),
        ),
        (
            "stale terminal response with no game ID",
            kiwi_flow("EndOfGame", 0),
        ),
    ] {
        sample(
            app,
            &client,
            "ChampSelect",
            DraftFixture {
                gameflow: Some(prior_flow),
                selection: Some(selection(SECOND_GAME, 55, 136, 5_000)),
                current_champion: Some(55),
                ..Default::default()
            },
        )
        .await;
        let (_, stale) = runtime
            .fields(crate::lcu::aux_activity::NAMESPACE, "state")
            .unwrap()
            .unwrap();
        println!("HKSLGT_CONSECUTIVE_KIWI_LATE: {label}, phase={} mode={} champion={} actionable={} confirming={}",
            stale["view"]["phase"], stale["view"]["gameMode"], stale["view"]["championId"],
            stale["view"]["selectionActionable"], stale["verifyingDraft"]);
        check_renderer(&aux, r#"
            document.querySelector('.aux-activity[data-phase=ChampSelect] .current-champion[data-champion-id="55"] img')?.naturalWidth > 1 &&
            document.querySelector('.bench [data-champion-id="136"][aria-disabled=true] img')?.naturalWidth > 1 &&
            /^5\s*(秒|s)$/.test(document.querySelector('.aux-draft-timer')?.textContent.trim()) &&
            !document.querySelector('.aux-draft-confirming, .current-champion[data-champion-id="22"], .bench [data-champion-id="103"]')
        "#, label, || {}).await;
    }
    aux.eval("delete document.documentElement.dataset.smokeColored")
        .unwrap();
    assert_color_portrait(&aux, ".current-champion[data-champion-id='55'] img").await;
    assert_eq!(aux.hwnd().unwrap(), hwnd);
    assert!(
        aux.is_visible().unwrap(),
        "the current second draft stays visible"
    );

    let mut unidentified = selection(SECOND_GAME, 55, 136, 5_000);
    unidentified["gameId"] = Value::Null;
    sample(
        app,
        &client,
        "ChampSelect",
        DraftFixture {
            gameflow: Some(kiwi_flow("ChampSelect", FIRST_GAME)),
            selection: Some(unidentified),
            current_champion: Some(55),
            ..Default::default()
        },
    )
    .await;
    let (_, unidentified_state) = runtime
        .fields(crate::lcu::aux_activity::NAMESPACE, "state")
        .unwrap()
        .unwrap();
    assert_ne!(
        unidentified_state["view"]["selectionActionable"], true,
        "an absent game ID cannot resurrect the retired first draft as writable"
    );
    aux.eval(
        r#"
        window.__TAURI__.core.invoke('league_call', {
            namespace: 'aux-activity-main', method: 'swapBenchChampion', args: [136]
        }).then(result => document.documentElement.dataset.staleSecondWrite =
            result.success === false ? 'blocked' : 'unexpected')
    "#,
    )
    .unwrap();
    check_renderer(
        &aux,
        "document.documentElement.dataset.staleSecondWrite === 'blocked'",
        "a stale second draft cannot write",
        || {},
    )
    .await;

    let mut next = selection(SECOND_GAME, 55, 136, 4_000);
    sample(
        app,
        &client,
        "ChampSelect",
        DraftFixture {
            gameflow: Some(kiwi_flow("ChampSelect", SECOND_GAME)),
            selection: Some(next.clone()),
            current_champion: Some(55),
            ..Default::default()
        },
    )
    .await;
    check_renderer(&aux, "!!document.querySelector('.bench [data-champion-id=\"136\"][aria-disabled=false]') && !document.querySelector('.aux-draft-waiting')", "fresh second game enables eligible actions", || {}).await;
    next["queueId"] = Value::Null;
    sample(
        app,
        &client,
        "ChampSelect",
        DraftFixture {
            selection: Some(next.clone()),
            ..Default::default()
        },
    )
    .await;
    check_renderer(&aux, r#"
        document.querySelector('.current-champion[data-champion-id="55"] img')?.naturalWidth > 1 &&
        document.querySelector('.bench [data-champion-id="136"][aria-disabled=true] img')?.naturalWidth > 1
    "#, "metadata gap within the second verified game preserves only its own portraits", || {}).await;

    next["queueId"] = json!(420);
    sample(
        app,
        &client,
        "ChampSelect",
        DraftFixture {
            gameflow: Some(flow("ChampSelect", SECOND_GAME, 420, "CLASSIC")),
            selection: Some(next),
            current_champion: Some(55),
            ..Default::default()
        },
    )
    .await;
    check_renderer(
        &aux,
        "!document.querySelector('.current-champion, .bench')",
        "explicit non-ARAM cannot retain the prior draft",
        || {},
    )
    .await;
    let observed = server.scene.lock().unwrap().writes.clone();
    assert_eq!(
        observed,
        vec![
            (
                "POST".to_owned(),
                "/lol-matchmaking/v1/ready-check/accept".to_owned()
            );
            2
        ]
    );

    let first = clock.first().expect("actual first displayed queue reading");
    let mut previous = first.seconds;
    for reading in &clock {
        let expected = first.seconds + (reading.at_ms - first.at_ms) / 1000.0;
        assert!(
            reading.eta,
            "the displayed estimated queue time changed: {reading:?}"
        );
        assert!(
            reading.seconds >= previous,
            "displayed queue time moved backward: {previous} -> {reading:?}"
        );
        assert!((reading.seconds - expected).abs() <= 0.55,
            "displayed queue time drifted beyond half a second from the paced fixture: {reading:?}, expected {expected:.2}");
        previous = reading.seconds;
    }
    let span = clock.last().unwrap().at_ms - first.at_ms;
    assert!(
        span >= 5900.0,
        "the browser clock only observed {span:.0}ms"
    );
    let mut whole_seconds: Vec<_> = clock
        .iter()
        .map(|reading| reading.seconds.floor() as u32)
        .collect();
    whole_seconds.dedup();
    let mut previous_poll = 0;
    for (i, before_poll) in before_polls.into_iter().enumerate() {
        let expected = 11 + 2 * i as u32;
        assert!(
            clock[previous_poll..before_poll]
                .iter()
                .any(|reading| reading.seconds.floor() as u32 == expected),
            "the real caption must show {expected} seconds before the next native poll"
        );
        previous_poll = before_poll;
    }
    assert!(
        whole_seconds.starts_with(&[10, 11, 12, 13, 14, 15, 16])
            && whole_seconds
                .windows(2)
                .all(|steps| steps[1] == steps[0] + 1),
        "the actual queue caption did not skip a displayed second: {whole_seconds:?}"
    );
    assert_eq!(aux.hwnd().unwrap(), hwnd);
    println!("HKSLGT_CONSECUTIVE_KIWI_PASS: same Mini, two observed drafts, no stale writes; displayed seconds {whole_seconds:?} over {span:.0}ms");
}
