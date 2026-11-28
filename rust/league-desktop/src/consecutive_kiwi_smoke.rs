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
    time::{Duration, Instant},
};

const FIRST_GAME: u64 = 8521;
const SECOND_GAME: u64 = 8522;
const UNRELATED_GAME: u64 = 8523;
const KIWI_QUEUE: u32 = 2400;
const BENCH_SWAP: &str = "/lol-champ-select/v1/session/bench/swap/";

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

    pub(super) fn swap_bench(draft: &mut DraftFixture, path: &str) -> Option<u16> {
        let requested = path.strip_prefix(BENCH_SWAP)?;
        let accepted = (|| {
            let requested = requested.parse::<u32>().ok()?;
            let selection = draft.selection.as_mut()?;
            if selection["gameId"] != SECOND_GAME || selection["queueId"] != KIWI_QUEUE {
                return None;
            }
            let local = &selection["myTeam"][0];
            let bench = &selection["benchChampions"][0];
            let current = draft.current_champion?;
            if local["puuid"] != PLAYER
                || local["championId"] != current
                || bench["championId"] != requested
            {
                return None;
            }
            selection["myTeam"][0]["championId"] = json!(requested);
            selection["benchChampions"][0]["championId"] = json!(current);
            draft.current_champion = Some(requested);
            Some(())
        })();
        Some(if accepted.is_some() { 204 } else { 409 })
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

fn serve_draft(app: &AppHandle, phase: &'static str, draft: DraftFixture) {
    let server = app.state::<MiniClient>();
    let mut scene = server.scene.lock().unwrap();
    scene.phase = phase;
    if phase == "ReadyCheck" {
        scene.response = "None";
    }
    scene.partial_kiwi = Some(draft);
}

async fn sample(
    app: &AppHandle,
    client: &Arc<LocalClient>,
    phase: &'static str,
    draft: DraftFixture,
) {
    serve_draft(app, phase, draft);
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

fn second_draft(gameflow: Value, champion: u32, bench: u32) -> DraftFixture {
    DraftFixture {
        gameflow: Some(gameflow),
        selection: Some(selection(SECOND_GAME, champion, bench, 5_000)),
        current_champion: Some(champion),
        ..Default::default()
    }
}

async fn assert_forged_swap_blocked(
    aux: &WebviewWindow,
    server: &MiniClient,
    champion: u32,
    label: &str,
) {
    let before = server.scene.lock().unwrap().writes.len();
    aux.eval(format!(
        r#"
        delete document.documentElement.dataset.rejectedBenchSwap;
        window.__TAURI__.core.invoke('league_call', {{
            namespace: 'aux-activity-main', method: 'swapBenchChampion', args: [{champion}]
        }}).then(result => document.documentElement.dataset.rejectedBenchSwap =
            result.success === false ? 'blocked' : 'unexpected')
        "#,
    ))
    .unwrap();
    check_renderer(
        aux,
        "document.documentElement.dataset.rejectedBenchSwap === 'blocked'",
        label,
        || {},
    )
    .await;
    assert_eq!(
        server.scene.lock().unwrap().writes.len(),
        before,
        "{label}: the rejected IPC call reached the League client's write endpoint"
    );
}

async fn second_draft_portraits(
    aux: &WebviewWindow,
    champion: u32,
    bench: u32,
    actionable: bool,
    label: &str,
) {
    check_renderer(
        aux,
        &format!(
            r#"
        document.querySelector('.aux-activity[data-phase=ChampSelect] .current-champion[data-champion-id="{champion}"] img')?.naturalWidth > 1 &&
        document.querySelector('.bench [data-champion-id="{bench}"][aria-disabled={disabled}] img')?.naturalWidth > 1 &&
        !document.querySelector('.aux-draft-confirming, .current-champion[data-champion-id="22"], .bench [data-champion-id="103"]') &&
        {notice}
        "#,
            disabled = !actionable,
            notice = if actionable {
                "!document.querySelector('.aux-draft-waiting')"
            } else {
                "!!document.querySelector('.aux-draft-waiting')"
            },
        ),
        label,
        || {},
    )
    .await;
}

async fn swap_in_second_draft(
    app: &AppHandle,
    client: &Arc<LocalClient>,
    aux: &WebviewWindow,
    gameflow: Value,
    current: &mut (u32, u32),
    label: &str,
) -> bool {
    let (champion, bench) = *current;
    let runtime = app.state::<ClientRuntime>();
    sample(
        app,
        client,
        "ChampSelect",
        second_draft(gameflow.clone(), champion, bench),
    )
    .await;
    let (_, state) = runtime
        .fields(crate::lcu::aux_activity::NAMESPACE, "state")
        .unwrap()
        .unwrap();
    let actionable = state["view"]["selectionActionable"] == true;
    println!(
        "HKSLGT_CONSECUTIVE_KIWI_BENCH: {label}, flowGame={} mode={} champion={} actionable={actionable}",
        gameflow["gameData"]["gameId"], state["view"]["gameMode"], state["view"]["championId"]
    );
    assert_eq!(state["view"]["gameMode"], "KIWI", "{label}");
    second_draft_portraits(aux, champion, bench, actionable, label).await;
    check_renderer(
        aux,
        r#"/^5\s*(秒|s)$/.test(document.querySelector('.aux-draft-timer')?.textContent.trim())"#,
        "the second draft shows the current server's five seconds",
        || {},
    )
    .await;
    if !actionable {
        // Observe both rejected shapes on the old build before reporting failure.
        return false;
    }

    assert_forged_swap_blocked(
        aux,
        &app.state::<MiniClient>(),
        22,
        "even a pickable champion outside the live bench cannot be requested through IPC",
    )
    .await;
    click_and_read_swap(app, client, aux, &gameflow, bench, label).await;
    second_draft_portraits(
        aux,
        bench,
        champion,
        true,
        "second draft after client readback",
    )
    .await;
    aux.eval("delete document.documentElement.dataset.smokeColored")
        .unwrap();
    assert_color_portrait(
        aux,
        &format!(r#".current-champion[data-champion-id="{bench}"] img"#),
    )
    .await;
    *current = (bench, champion);
    true
}

async fn click_and_read_swap(
    app: &AppHandle,
    client: &Arc<LocalClient>,
    aux: &WebviewWindow,
    gameflow: &Value,
    bench: u32,
    label: &str,
) {
    let server = app.state::<MiniClient>();
    let before = server.scene.lock().unwrap().writes.len();
    click(aux, &format!(r#".bench [data-champion-id="{bench}"]"#)).await;
    tokio::time::timeout(Duration::from_secs(4), async {
        while server.scene.lock().unwrap().writes.len() == before {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label}: clicking the bench never reached the TLS client"));
    {
        let scene = server.scene.lock().unwrap();
        assert_eq!(
            &scene.writes[before..],
            &[("POST".to_owned(), format!("{BENCH_SWAP}{bench}"))],
            "{label}: the bench click must reach the exact endpoint once"
        );
        let draft = scene.partial_kiwi.as_ref().unwrap();
        assert_eq!(draft.gameflow.as_ref(), Some(gameflow), "{label}");
        assert_eq!(draft.current_champion, Some(bench), "{label}");
    }
    app.state::<ClientRuntime>()
        .sample_fixture(app, client)
        .await;
}

async fn six_bench_portraits(
    aux: &WebviewWindow,
    champion: u32,
    bench: [u32; 6],
    actionable: bool,
    label: &str,
) {
    let started = Instant::now();
    check_renderer(
        aux,
        &format!(
            r#"(() => {{
                const decoded = (portrait, id) => {{
                    const img = portrait?.querySelector('img');
                    return Number(portrait?.dataset.championId) === id && img?.complete &&
                        img.naturalWidth > 1 && img.currentSrc.includes('/champion-icons/' + id + '.png');
                }};
                const own = document.querySelector('.aux-activity[data-phase=ChampSelect] .current-champion');
                const slots = [...document.querySelectorAll('.bench [data-champion-id]')];
                return decoded(own, {champion}) && slots.length === 6 &&
                    {bench}.every(id => slots.some(slot => decoded(slot, id))) &&
                    !document.querySelector('.aux-draft-confirming') &&
                    ({actionable}
                        ? slots.find(slot => Number(slot.dataset.championId) === {first})?.getAttribute('aria-disabled') === 'false'
                        : slots.every(slot => slot.getAttribute('aria-disabled') === 'true')) &&
                    /^18\s*(秒|s)$/.test(document.querySelector('.aux-draft-timer')?.textContent.trim());
            }})()"#,
            bench = json!(bench),
            first = bench[0],
        ),
        label,
        || {},
    )
    .await;
    let elapsed = started.elapsed();
    assert!(
        elapsed <= Duration::from_secs(1),
        "{label}: WebView2 took {elapsed:?}"
    );
    println!("HKSLGT_KIWI_SEVEN_PORTRAITS: {label}, rendered in {elapsed:?}");
}

async fn mislabeled_queue_draft(
    app: &AppHandle,
    client: &Arc<LocalClient>,
    aux: &WebviewWindow,
    champion: u32,
    target: u32,
) {
    let gameflow = flow("ChampSelect", SECOND_GAME, KIWI_QUEUE, "CLASSIC");
    let mut bench = [target, 1, 2, 4, 12, 17];
    let mut view = selection(SECOND_GAME, champion, target, 18_000);
    view["benchChampions"] = json!(bench.map(|id| json!({"championId": id})));
    sample(
        app,
        client,
        "ChampSelect",
        DraftFixture {
            gameflow: Some(gameflow.clone()),
            selection: Some(view.clone()),
            current_champion: Some(champion),
            ..Default::default()
        },
    )
    .await;
    let (_, state) = app
        .state::<ClientRuntime>()
        .fields(crate::lcu::aux_activity::NAMESPACE, "state")
        .unwrap()
        .unwrap();
    println!(
        "HKSLGT_KIWI_MISLABELED_QUEUE: phase={} mode={} champion={} bench={} actionable={} confirming={}",
        state["view"]["phase"], state["view"]["gameMode"], state["view"]["championId"],
        state["view"]["bench"], state["view"]["selectionActionable"], state["verifyingDraft"]
    );
    if state["view"].is_null() && state["verifyingDraft"] == true {
        check_renderer(
            aux,
            "!!document.querySelector('.aux-draft-confirming') && !document.querySelector('.current-champion, .bench')",
            "old bundled Mini reproduces the confirmation banner and missing selection UI",
            || {},
        )
        .await;
        panic!("queue 2400 with CLASSIC metadata reproduced the missing selection UI in Windows WebView2");
    }
    assert_eq!(state["view"]["gameMode"], "KIWI");
    assert_eq!(state["view"]["selectionActionable"], true);
    six_bench_portraits(
        aux,
        champion,
        bench,
        true,
        "own champion and six decoded KIWI bench portraits",
    )
    .await;
    sample(
        app,
        client,
        "ChampSelect",
        DraftFixture {
            gameflow: Some(flow("ReadyCheck", SECOND_GAME, KIWI_QUEUE, "CLASSIC")),
            selection: Some(view.clone()),
            current_champion: Some(champion),
            ..Default::default()
        },
    )
    .await;
    six_bench_portraits(
        aux,
        champion,
        bench,
        false,
        "a lagging ReadyCheck with the right queue number cannot hide the live KIWI portraits",
    )
    .await;
    let mut unnumbered = view.clone();
    unnumbered["gameId"] = json!(0);
    unnumbered["queueId"] = json!(0);
    sample(
        app,
        client,
        "ChampSelect",
        DraftFixture {
            selection: Some(unnumbered),
            current_champion: Some(champion),
            ..Default::default()
        },
    )
    .await;
    six_bench_portraits(
        aux,
        champion,
        bench,
        false,
        "authoritative bench and current hero render before gameflow or queue metadata arrive",
    )
    .await;
    let server = app.state::<MiniClient>();
    assert_forged_swap_blocked(aux, &server, target, "missing draft metadata").await;
    sample(
        app,
        client,
        "ChampSelect",
        DraftFixture {
            gameflow: Some(gameflow.clone()),
            selection: Some(view),
            current_champion: Some(champion),
            ..Default::default()
        },
    )
    .await;
    click_and_read_swap(
        app,
        client,
        aux,
        &gameflow,
        target,
        "mislabeled KIWI bench swap",
    )
    .await;
    bench[0] = champion;
    six_bench_portraits(
        aux,
        target,
        bench,
        true,
        "all seven portraits after the verified KIWI swap",
    )
    .await;
}

async fn reject_second_draft_write(
    app: &AppHandle,
    client: &Arc<LocalClient>,
    aux: &WebviewWindow,
    draft: DraftFixture,
    bench: u32,
    label: &str,
) {
    sample(app, client, "ChampSelect", draft).await;
    let runtime = app.state::<ClientRuntime>();
    let (_, state) = runtime
        .fields(crate::lcu::aux_activity::NAMESPACE, "state")
        .unwrap()
        .unwrap();
    assert_ne!(state["view"]["selectionActionable"], true, "{label}");
    check_renderer(
        aux,
        "!document.querySelector('.bench [aria-disabled=false]')",
        label,
        || {},
    )
    .await;
    assert_forged_swap_blocked(aux, &app.state::<MiniClient>(), bench, label).await;
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

    let mut portraits = (55, 136);
    let stale = swap_in_second_draft(
        app,
        &client,
        &aux,
        kiwi_flow("ChampSelect", FIRST_GAME),
        &mut portraits,
        "known retired first game still returned by gameflow",
    )
    .await;
    let alias = swap_in_second_draft(
        app,
        &client,
        &aux,
        flow("ChampSelect", SECOND_GAME, KIWI_QUEUE, "ARAM"),
        &mut portraits,
        "KIWI queue with the shared ARAM game mode label",
    )
    .await;
    assert!(
        stale && alias,
        "the verified second KIWI draft must allow a real bench swap: retired game allowed={stale}, shared ARAM label allowed={alias}"
    );
    let (champion, bench) = portraits;
    assert_eq!((champion, bench), (55, 136));
    assert_eq!(aux.hwnd().unwrap(), hwnd);
    assert!(
        aux.is_visible().unwrap(),
        "the current second draft stays visible"
    );

    let mut changed_player = second_draft(kiwi_flow("ChampSelect", SECOND_GAME), champion, bench);
    changed_player.selection.as_mut().unwrap()["myTeam"][0]["puuid"] =
        json!("different-fixture-player");
    check_renderer(
        &aux,
        "!!document.querySelector('.bench [aria-disabled=false]')",
        "the known second draft is still enabled before the server-side player changes",
        || {},
    )
    .await;
    // Do not publish a new view here. The action must re-read the player identity.
    serve_draft(app, "ChampSelect", changed_player);
    assert_forged_swap_blocked(
        &aux,
        &server,
        bench,
        "a different player occupying the same cell cannot use the previously enabled bench",
    )
    .await;

    let mut unidentified =
        second_draft(flow("ChampSelect", 0, KIWI_QUEUE, "ARAM"), champion, bench);
    unidentified
        .selection
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("gameId");
    sample(app, &client, "ChampSelect", unidentified).await;
    second_draft_portraits(
        &aux,
        champion,
        bench,
        false,
        "the shared ARAM label without a positive game ID displays a read-only bench",
    )
    .await;

    let mut changed_game = second_draft(
        flow("ChampSelect", UNRELATED_GAME, KIWI_QUEUE, "ARAM"),
        champion,
        bench,
    );
    changed_game.selection.as_mut().unwrap()["gameId"] = json!(UNRELATED_GAME);
    serve_draft(app, "ChampSelect", changed_game);
    assert_forged_swap_blocked(
        &aux,
        &server,
        bench,
        "a newly identified game cannot authorize an IPC call from an unidentified snapshot",
    )
    .await;

    let mut missing_id = second_draft(kiwi_flow("ChampSelect", FIRST_GAME), champion, bench);
    missing_id.selection.as_mut().unwrap()["gameId"] = Value::Null;
    let mut retired_id = second_draft(kiwi_flow("ChampSelect", SECOND_GAME), champion, bench);
    retired_id.selection.as_mut().unwrap()["gameId"] = json!(FIRST_GAME);
    for (label, draft) in [
        ("no game ID on the draft endpoint", missing_id),
        ("the draft endpoint returns the retired game ID", retired_id),
        (
            "an unrelated active game on gameflow",
            second_draft(kiwi_flow("ChampSelect", UNRELATED_GAME), champion, bench),
        ),
    ] {
        reject_second_draft_write(app, &client, &aux, draft, bench, label).await;
    }

    sample(
        app,
        &client,
        "ChampSelect",
        second_draft(kiwi_flow("EndOfGame", 0), champion, bench),
    )
    .await;
    second_draft_portraits(
        &aux,
        champion,
        bench,
        false,
        "stale terminal response without a game ID stays read-only",
    )
    .await;
    assert_forged_swap_blocked(
        &aux,
        &server,
        bench,
        "a terminal response without a game ID cannot authorize a bench swap",
    )
    .await;

    let mut next = selection(SECOND_GAME, champion, bench, 4_000);
    sample(
        app,
        &client,
        "ChampSelect",
        DraftFixture {
            gameflow: Some(kiwi_flow("ChampSelect", SECOND_GAME)),
            selection: Some(next.clone()),
            current_champion: Some(champion),
            ..Default::default()
        },
    )
    .await;
    second_draft_portraits(
        &aux,
        champion,
        bench,
        true,
        "the agreeing second game enables eligible actions",
    )
    .await;
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
    second_draft_portraits(
        &aux,
        champion,
        bench,
        false,
        "metadata gap preserves the correct second draft's portraits without writes",
    )
    .await;

    mislabeled_queue_draft(app, &client, &aux, champion, bench).await;
    assert_eq!(aux.hwnd().unwrap(), hwnd);
    assert!(aux.is_visible().unwrap());

    next["queueId"] = json!(420);
    sample(
        app,
        &client,
        "ChampSelect",
        DraftFixture {
            gameflow: Some(flow("ChampSelect", SECOND_GAME, 420, "CLASSIC")),
            selection: Some(next),
            current_champion: Some(champion),
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
    assert_forged_swap_blocked(&aux, &server, bench, "explicit non-ARAM").await;
    let observed = server.scene.lock().unwrap().writes.clone();
    let accept = (
        "POST".to_owned(),
        "/lol-matchmaking/v1/ready-check/accept".to_owned(),
    );
    assert_eq!(
        observed,
        [
            accept.clone(),
            accept,
            ("POST".to_owned(), format!("{BENCH_SWAP}136")),
            ("POST".to_owned(), format!("{BENCH_SWAP}55")),
            ("POST".to_owned(), format!("{BENCH_SWAP}136")),
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
    println!("HKSLGT_CONSECUTIVE_KIWI_PASS: same Mini, three verified bench swaps with decoded client readback, no stale or forged writes; displayed seconds {whole_seconds:?} over {span:.0}ms");
}
