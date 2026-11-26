//! Enter the bundled player tab while its real same-shard HTTPS request is held open.
use super::{
    bootstrap, check_renderer, mini_client_fixture::MiniClient, set_preference, ClientRuntime,
    DesktopProfile, AUX_WINDOW,
};
use crate::{client_discovery::LocalClient, client_runtime::reconcile_companions};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetWindowThreadProcessId, SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_BLOCK,
    WM_NULL,
};

const PLAYER: &str = "11111111-2222-4333-8444-555555555555";
const GAMES: &str =
    "/lol-match-history/v1/products/lol/11111111-2222-4333-8444-555555555555/matches";
const BUDGET: Duration = Duration::from_millis(1500);
const FIRST_GAME: u64 = 8_050_001;
const PAGE_SIZE: u64 = 20;
const NEW_GAME: u64 = FIRST_GAME + PAGE_SIZE;

// LCU list responses contain the requesting participant. The game endpoint supplies both teams.
fn game(id: u64, full: bool) -> Value {
    let mut participants = Vec::new();
    let mut identities = Vec::new();
    let hour = id - FIRST_GAME;
    let own_kills = if id == NEW_GAME { 13 } else { 12 };
    for slot in 1..=if full { 10 } else { 1 } {
        let champion = [22, 103, 55, 86, 111, 1, 45, 99, 75, 21][slot - 1];
        let player = if slot == 1 {
            PLAYER.to_owned()
        } else {
            format!("10000000-0000-4000-8000-{slot:012}")
        };
        let name = if slot == 1 {
            "History Fixture".to_owned()
        } else {
            format!("Fixture player {slot}")
        };
        identities.push(json!({"participantId":slot,"player":{"puuid":player,"currentPlatformId":"HN1","platformId":"HN1",
            "accountId":41+slot,"summonerId":41+slot,"gameName":name,"summonerName":name,"tagLine":"MOCK","profileIcon":42}}));
        let mut stats = json!({"participantId":slot,"win":slot<=5,"kills":if slot==1 {own_kills} else {6},
            "deaths":3,"assists":19,"champLevel":18,"goldEarned":18900,"totalDamageDealtToChampions":34567,
            "totalDamageTaken":19500,"totalMinionsKilled":41,"neutralMinionsKilled":0,"magicDamageDealtToChampions":1500,
            "physicalDamageDealtToChampions":32567,"trueDamageDealtToChampions":500,"totalDamageDealt":66000,
            "physicalDamageTaken":1000,"magicalDamageTaken":12000,"trueDamageTaken":500,"totalHeal":0,
            "totalUnitsHealed":0,"visionScore":0,"perkPrimaryStyle":8000,"perkSubStyle":8100});
        let fields = stats.as_object_mut().unwrap();
        for (index, item) in [3006, 6672, 3085, 3031, 3036, 3072, 2052]
            .into_iter()
            .enumerate()
        {
            fields.insert(format!("item{index}"), json!(item));
        }
        for (index, perk) in [8005, 8009, 9104, 8014, 8139, 8106].into_iter().enumerate() {
            fields.insert(format!("perk{index}"), json!(perk));
        }
        participants.push(
            json!({"participantId":slot,"teamId":if slot<=5 {100} else {200},
            "championId":champion,"spell1Id":4,"spell2Id":32,
            "timeline":{"participantId":slot,"lane":"NONE","role":"NONE"},"stats":stats}),
        );
    }
    json!({"gameId":id,"gameCreation":1_795_138_400_000_u64 + hour*3_600_000,
        "gameCreationDate":format!("2026-11-20T{:02}:33:20.000Z", 1+hour),"gameDuration":1380,"gameMode":"ARAM","gameType":"MATCHED_GAME",
        "mapId":12,"queueId":450,"platformId":"HN1","gameVersion":"16.22.1","seasonId":0,"endOfGameResult":"GameComplete",
        "gameModeMutators":[],"participantIdentities":identities,"participants":participants,
        "teams":[{"teamId":100,"win":"Win","bans":[]},{"teamId":200,"win":"Fail","bans":[]}]})
}

fn timeline() -> Value {
    let frames: Vec<_> = (0..24).map(|minute| {
        let participants: serde_json::Map<_,_> = (1..=10).map(|id| (id.to_string(),json!({
            "participantId":id,"level":1+(minute*2/3),"currentGold":300,"totalGold":500+minute*500,
            "xp":minute*500,"minionsKilled":minute*2,"jungleMinionsKilled":0,"position":{"x":500+id*10,"y":750+id*10}
        }))).collect();
        json!({"timestamp":minute*60000,"participantFrames":participants,
            "events":if minute==10 {json!([{"type":"CHAMPION_KILL","timestamp":600000,"killerId":1,"victimId":6,"assistingParticipantIds":[2]}])} else {json!([])}})
    }).collect();
    json!({"frameInterval":60000,"frames":frames})
}

#[derive(Default)]
struct HistoryRequests {
    hold: bool,
    fail: bool,
    pages: usize,
    failed_pages: usize,
    new_game_reads: usize,
    champion_images: usize,
    ranked: usize,
    profiles: usize,
    details: usize,
    timelines: usize,
    in_flight: usize,
    peak: usize,
    hold_details: bool,
    timed_out: bool,
}

#[derive(Default)]
pub(super) struct HistoryFixture {
    active: AtomicBool,
    requests: Mutex<HistoryRequests>,
    release: Condvar,
}

impl HistoryFixture {
    pub(super) fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn begin(&self) {
        *self.requests.lock().unwrap() = HistoryRequests {
            hold: true,
            hold_details: true,
            ..Default::default()
        };
        self.active.store(true, Ordering::Release);
    }

    fn release_page(&self) {
        self.requests.lock().unwrap().hold = false;
        self.release.notify_all();
    }

    fn end(&self) {
        self.active.store(false, Ordering::Release);
        self.requests.lock().unwrap().hold_details = false;
        self.release_page();
    }

    pub(super) fn get(&self, path: &str) -> Option<(u16, Value)> {
        if !self.active() {
            return None;
        }
        let mut requests = self.requests.lock().unwrap();
        let details = path
            .strip_prefix("/lol-match-history/v1/games/")
            .map(|id| (id, false))
            .or_else(|| {
                path.strip_prefix("/lol-match-history/v1/game-timelines/")
                    .map(|id| (id, true))
            });
        if let Some((id, is_timeline)) = details {
            let id: u64 = id.parse().ok()?;
            if !(FIRST_GAME..=NEW_GAME).contains(&id) {
                return None;
            }
            if is_timeline {
                requests.timelines += 1;
            } else {
                requests.details += 1;
                if id == NEW_GAME {
                    requests.new_game_reads += 1;
                    println!(
                        "HKSLGT_HISTORY_NEW_GAME: renderer requested fresh game {id} after retry"
                    );
                }
            }
            requests.in_flight += 1;
            requests.peak = requests.peak.max(requests.in_flight);
            let (mut state, elapsed) = self
                .release
                .wait_timeout_while(requests, Duration::from_secs(7), |r| r.hold_details)
                .unwrap();
            if elapsed.timed_out() {
                state.timed_out = true;
            }
            drop(state);
            thread::sleep(Duration::from_millis(75));
            let data = if is_timeline {
                timeline()
            } else {
                game(id, true)
            };
            self.requests.lock().unwrap().in_flight -= 1;
            return Some((200, data));
        }
        match path.split('?').next()? {
            asset if asset.starts_with("/lol-game-data/assets/v1/champion-icons/") => {
                if requests.pages > 0 {
                    requests.champion_images += 1;
                }
                None // MiniClient serves the actual image response.
            }
            "/lol-chat/v1/me" => Some((200, json!({"puuid":PLAYER,"platformId":"HN1"}))),
            "/lol-summoner/v1/current-summoner" => {
                requests.profiles += 1;
                Some((
                    200,
                    json!({"puuid":PLAYER,"accountId":42,"summonerId":42,"gameName":"History Fixture","displayName":"History Fixture",
                    "internalName":"History Fixture","tagLine":"MOCK","profileIconId":42,"summonerLevel":45,
                    "xpSinceLastLevel":30,"xpUntilNextLevel":70,"privacy":"PUBLIC"}),
                ))
            }
            "/lol-ranked/v1/ranked-stats/11111111-2222-4333-8444-555555555555" => {
                requests.ranked += 1;
                Some((200, json!({"queues":[],"queueMap":{}})))
            }
            GAMES => {
                requests.pages += 1;
                let (mut state, elapsed) = self
                    .release
                    .wait_timeout_while(requests, Duration::from_secs(7), |r| r.hold)
                    .unwrap();
                if elapsed.timed_out() {
                    state.timed_out = true;
                }
                if state.fail {
                    state.failed_pages += 1;
                    println!(
                        "HKSLGT_HISTORY_HTTP: page={} returned HTTP 503",
                        state.pages
                    );
                    return Some((503, json!({"message":"Synthetic history unavailable"})));
                }
                let oldest = FIRST_GAME + u64::from(state.failed_pages > 0);
                let newest = oldest + PAGE_SIZE - 1;
                println!("HKSLGT_HISTORY_HTTP: page={} returned HTTP 200, newestGame={newest} oldestGame={oldest}", state.pages);
                drop(state);
                let games: Vec<_> = (oldest..=newest).rev().map(|id| game(id, false)).collect();
                Some((
                    200,
                    json!({"accountId":42,"platformId":"HN1","games":{
                        "gameIndexBegin":0,"gameIndexEnd":19,"gameCount":PAGE_SIZE,"games":games
                    }}),
                ))
            }
            _ => None,
        }
    }
}

fn child_window_count(parent: *mut std::ffi::c_void) -> usize {
    unsafe extern "system" fn count(_: *mut std::ffi::c_void, data: isize) -> i32 {
        // EnumChildWindows invokes this synchronously on the caller's thread.
        unsafe { *(data as *mut usize) += 1 };
        1
    }
    let mut children = 0_usize;
    unsafe { EnumChildWindows(parent, Some(count), (&mut children as *mut usize) as isize) };
    children
}

struct StopWatchdog(Arc<AtomicBool>);

impl Drop for StopWatchdog {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn watchdog(
    hwnd: usize,
    stop: Arc<AtomicBool>,
    stage: Arc<Mutex<&'static str>>,
    entered: Instant,
) -> thread::JoinHandle<(usize, Duration)> {
    thread::spawn(move || {
        let hwnd = hwnd as *mut std::ffi::c_void;
        let mut owner = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut owner) };
        assert_eq!(
            owner,
            std::process::id(),
            "main HWND must belong to this fixture"
        );
        let mut checks = 0;
        let mut worst = Duration::ZERO;
        let mut max_children = child_window_count(hwnd);
        let mut last_stage = None;
        let mut stage_checks = 0;
        let mut stage_worst = Duration::ZERO;
        while !stop.load(Ordering::Acquire) {
            let start = Instant::now();
            let section = *stage.lock().unwrap();
            let children = child_window_count(hwnd);
            max_children = max_children.max(children);
            if last_stage != Some(section) {
                if let Some(previous) = last_stage {
                    println!("HKSLGT_HISTORY_NATIVE_ACK: stage={previous}, pings={stage_checks}, worst={stage_worst:?}");
                }
                stage_checks = 0;
                stage_worst = Duration::ZERO;
                println!("HKSLGT_HISTORY_CHILD_HWND: stage={section}, elapsed={:?}, children={children}, maxChildren={max_children}", start.duration_since(entered));
                last_stage = Some(section);
            }
            let mut reply = 0;
            let delivered = unsafe {
                SendMessageTimeoutW(
                    hwnd,
                    WM_NULL,
                    0,
                    0,
                    SMTO_ABORTIFHUNG | SMTO_BLOCK,
                    BUDGET.as_millis() as u32,
                    &mut reply,
                )
            };
            if delivered == 0 {
                let at_timeout = child_window_count(hwnd);
                if stop.load(Ordering::Acquire) {
                    let mut current_owner = 0;
                    unsafe { GetWindowThreadProcessId(hwnd, &mut current_owner) };
                    if current_owner != owner {
                        // The coordinator already unwound and its owned HWND has been destroyed.
                        break;
                    }
                }
                panic!("HKSLGT_HISTORY_NATIVE_HUNG: main HWND failed WM_NULL in {BUDGET:?}; sent at {:?} during {section}, timeout during {}, childrenBefore={children}, childrenAtTimeout={at_timeout}, maxChildren={}",
                    start.duration_since(entered), *stage.lock().unwrap(), max_children.max(at_timeout));
            }
            let elapsed = start.elapsed();
            worst = worst.max(elapsed);
            stage_worst = stage_worst.max(elapsed);
            stage_checks += 1;
            checks += 1;
            thread::sleep(Duration::from_millis(50));
        }
        if let Some(previous) = last_stage {
            println!("HKSLGT_HISTORY_NATIVE_ACK: stage={previous}, pings={stage_checks}, worst={stage_worst:?}");
        }
        println!("HKSLGT_HISTORY_CHILD_HWND: final maxChildren={max_children}");
        (checks, worst)
    })
}

// Register the WebView evaluation off the test's async coordinator. If Windows stops
// dispatching, eval_with_callback itself can block before registering its callback.
async fn page_bool(window: &WebviewWindow, script: &'static str, label: &'static str) -> bool {
    page_bool_within(window, script, label, BUDGET).await
}

async fn page_bool_within(
    window: &WebviewWindow,
    script: &'static str,
    label: &'static str,
    budget: Duration,
) -> bool {
    tokio::time::timeout(budget, page_result(window, script))
        .await
        .unwrap_or_else(|_| panic!("HKSLGT_HISTORY_RENDERER_HUNG: {label} exceeded {budget:?}"))
}

async fn page_result<T: serde::de::DeserializeOwned>(
    window: &WebviewWindow,
    script: &'static str,
) -> T {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let window = window.clone();
    thread::spawn(move || {
        let tx = Mutex::new(Some(tx));
        let _ = window.eval_with_callback(script, move |raw| {
            if let Some(tx) = tx.lock().unwrap().take() {
                let _ = tx.send(String::from(raw));
            }
        });
    });
    let raw = rx.await.expect("main WebView2 evaluation callback closed");
    serde_json::from_str(&raw).expect("main WebView2 returned the expected JSON type")
}

const COLLAPSED: &str =
    "document.querySelector('.app-sidebar')?.classList.contains('collapsed') === true";
const PRESS_SIDEBAR: &str = "(() => { const el = document.querySelector('.app-sidebar__expand-line'); if (!el) return false; el.dispatchEvent(new MouseEvent('mousedown',{bubbles:true,button:0})); return true })()";

const ARM_ROOM_CLICK: &str = r#"(() => {
    const sidebar = document.querySelector('.app-sidebar');
    const control = sidebar?.querySelector('.app-sidebar__expand-line');
    if (!control || !location.hash.includes('player-tabs') || window.__hkslgtRoomClick) return false;
    const before = sidebar.classList.contains('collapsed');
    const armed = performance.now();
    const pulse = window.__hkslgtRoomClick = {
        before, armed, due: armed + 100, armedWall: Date.now(),
        fired: null, firedWall: null, changed: null, changedWall: null, after: null, error: null
    };
    const observer = new MutationObserver(() => {
        const collapsed = sidebar.classList.contains('collapsed');
        if (pulse.fired !== null && collapsed !== before) {
            pulse.changed = performance.now();
            pulse.changedWall = Date.now();
            pulse.after = collapsed;
            observer.disconnect();
        }
    });
    observer.observe(sidebar, {attributes: true, attributeFilter: ['class']});
    setTimeout(() => {
        if (!control.isConnected || !location.hash.includes('player-tabs') || sidebar.classList.contains('collapsed') !== before) {
            pulse.error = 'the real history sidebar changed before its scheduled input';
            observer.disconnect();
            return;
        }
        pulse.fired = performance.now();
        pulse.firedWall = Date.now();
        control.dispatchEvent(new MouseEvent('mousedown', {bubbles:true, button:0}));
    }, 100);
    return true;
})()"#;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoomClick {
    before: bool,
    armed: f64,
    due: f64,
    armed_wall: f64,
    fired: Option<f64>,
    fired_wall: Option<f64>,
    changed: Option<f64>,
    changed_wall: Option<f64>,
    sampled: f64,
    after: Option<bool>,
    error: Option<String>,
}

fn wall_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn measured_room_click(
    main: &WebviewWindow,
    before: bool,
    open_wall: u64,
    cold: bool,
) -> u64 {
    let read_started = Instant::now();
    let pulse = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let pulse: RoomClick = page_result(main, "({...window.__hkslgtRoomClick, sampled:performance.now()})").await;
            if let Some(error) = &pulse.error {
                panic!("HKSLGT_HISTORY_ROOM_INPUT: {error}");
            }
            if pulse.changed.is_some() {
                break pulse;
            }
            assert!(pulse.sampled - pulse.due <= BUDGET.as_secs_f64() * 1000.0,
                "HKSLGT_HISTORY_ROOM_INPUT_HUNG: browser readback succeeded; timer or Vue change still pending after {:.2}ms (fired={:?})",
                pulse.sampled - pulse.due, pulse.fired);
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }).await.expect("HKSLGT_HISTORY_ROOM_INPUT_COLLECTION: callback did not return a finished browser measurement within 8s");
    let fired = pulse
        .fired
        .expect("browser timer dispatched the real input");
    // WebView2 serializes the integer returned by Date.now() as a JSON double.
    let fired_wall = pulse.fired_wall.unwrap() as u64;
    let changed = pulse.changed.unwrap();
    assert_eq!(
        pulse.before, before,
        "sidebar changed while arming the timer"
    );
    assert_eq!(
        pulse.after,
        Some(!before),
        "Vue must toggle the original sidebar class"
    );
    assert!(
        pulse.armed_wall <= open_wall as f64 && open_wall <= fired_wall,
        "test timer outside the real open: armed={} open={open_wall} fired={fired_wall}",
        pulse.armed_wall
    );
    assert!(
        pulse.due >= pulse.armed && changed >= fired,
        "invalid browser monotonic clock samples"
    );
    let late = (fired - pulse.due).max(0.0);
    let feedback = changed - fired;
    let elapsed = late + feedback;
    println!("HKSLGT_HISTORY_ROOM_INPUT: newWindow={cold} plannedMs={:.2} firedMs={fired:.2} changedMs={changed:.2} lateMs={late:.2} vueFeedbackMs={feedback:.2} actualMs={elapsed:.2} firedAfterOpenMs={} collectionWait={:?} resultAgeAtCollectionMs={}",
        pulse.due, fired_wall - open_wall, read_started.elapsed(), wall_millis().saturating_sub(pulse.changed_wall.unwrap() as u64));
    assert!(elapsed <= BUDGET.as_secs_f64() * 1000.0,
        "HKSLGT_HISTORY_ROOM_INPUT_HUNG: the scheduled sidebar input and Vue class change took {elapsed:.2}ms");
    fired_wall
}

// PlayerTabHeader drives NButton.loading from all eight profile requests. Naive UI 2.44.1
// ignores clicks whilst loading but sets the DOM disabled property only for its disabled prop.
const CLICK_READY_REFRESH: &str = r#"(() => {
    const buttons = document.querySelectorAll('.mb-6.h-28 > .ml-8 > button.n-button:last-child');
    const refresh = Array.from(buttons).find(button => button.getClientRects().length > 0);
    if (!refresh || refresh.disabled || refresh.classList.contains('n-button--loading')) return false;
    refresh.click();
    return true;
})()"#;
const LOADING: &str =
    "location.hash.includes('player-tabs') && document.querySelector('.n-spin') != null";
const CARDS: &str = r#"document.querySelectorAll('[class*="contain-intrinsic-size:116px"]').length === 20 && [...document.querySelectorAll('[class*="contain-intrinsic-size:116px"]')].some(card => /12\s*\/\s*3\s*\/\s*19/.test(card.innerText))"#;
const RECOVERED: &str = r#"(() => { const cards = document.querySelectorAll('[class*="contain-intrinsic-size:116px"]'); return cards.length === 20 && /13\s*\/\s*3\s*\/\s*19/.test(cards[0].innerText); })()"#;

async fn click_ready_refresh(main: &WebviewWindow, label: &'static str) {
    let started = Instant::now();
    loop {
        if page_bool(main, CLICK_READY_REFRESH, label).await {
            println!(
                "HKSLGT_HISTORY_REFRESH_CLICK: {label} available after {:?}",
                started.elapsed()
            );
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{label}: the stock refresh stayed unavailable"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn click_sidebar(
    main: &WebviewWindow,
    expected: bool,
    budget: Duration,
    label: &'static str,
) -> Duration {
    let started = Instant::now();
    assert!(
        page_bool_within(main, PRESS_SIDEBAR, label, budget).await,
        "stock sidebar control missing"
    );
    let dispatched = started.elapsed();
    assert_eq!(
        page_bool_within(
            main,
            COLLAPSED,
            label,
            budget.saturating_sub(started.elapsed())
        )
        .await,
        expected,
        "sidebar did not handle its input"
    );
    let elapsed = started.elapsed();
    if elapsed > BUDGET {
        println!("HKSLGT_HISTORY_SLOW_INPUT: {label}, click dispatch={dispatched:?}, following Vue readback={:?}", elapsed - dispatched);
    }
    elapsed
}

async fn input_round_trip(main: &WebviewWindow, label: &'static str) {
    let started = Instant::now();
    println!("HKSLGT_HISTORY_UI_INPUT: {label}");
    let previous = page_bool(main, COLLAPSED, "read sidebar").await;
    for expected in [!previous, previous] {
        click_sidebar(main, expected, BUDGET, label).await;
    }
    println!("HKSLGT_HISTORY_UI_ACK: {label} in {:?}", started.elapsed());
}

pub(super) async fn check(app: &AppHandle) {
    let profile = app.state::<DesktopProfile>();
    let (aux_enabled, auto_update) = {
        let db = profile.database.lock().unwrap();
        let update = bootstrap::backend_pref("self-update-main", "autoCheckUpdates").unwrap();
        let auto_update = update.read(&db).unwrap();
        update.write(&db, &json!(false)).unwrap();
        let enabled = bootstrap::backend_pref(AUX_WINDOW, "enabled")
            .unwrap()
            .read(&db)
            .unwrap()
            .as_bool()
            .unwrap();
        (enabled, auto_update)
    };
    set_preference(app, AUX_WINDOW, "enabled", false);
    let server = app.state::<MiniClient>();
    let previous_phase = {
        let mut scene = server.scene.lock().unwrap();
        std::mem::replace(&mut scene.phase, "None")
    };
    server.history.begin();
    let client =
        Arc::new(LocalClient::fixture_at(server.port).expect("owned HN1 fixture listener"));
    let main = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("main-window.html".into()))
        .data_directory(
            profile
                .webview_directory()
                .expect("shared fixture WebView2 directory"),
        )
        .inner_size(1100.0, 760.0)
        .decorations(false)
        .on_navigation(super::super::bundled_navigation)
        .build()
        .expect("create bundled main window for history acceptance");
    check_renderer(
        &main,
        "document.querySelector('.menu-item[data-key=toolkit]') != null",
        "bundled main navigation",
        || {},
    )
    .await;
    assert!(page_bool(&main, "(() => { document.querySelector('.menu-item[data-key=toolkit]').click(); return true })()", "open toolkit before history").await);
    check_renderer(
        &main,
        "location.hash.includes('toolkit')",
        "actual toolkit route before history",
        || {},
    )
    .await;
    let runtime = app.state::<ClientRuntime>();
    runtime.sample_fixture(app, &client).await;
    reconcile_companions(app);
    super::assert_hidden(
        app,
        "aux",
        "aux must be hidden before opening player history",
    );
    assert_eq!(
        server.history.requests.lock().unwrap().pages,
        0,
        "history must start on actual navigation"
    );
    input_round_trip(&main, "toolkit before navigation").await;
    let started = Instant::now();
    let stop = StopWatchdog(Arc::new(AtomicBool::new(false)));
    let stage = Arc::new(Mutex::new("history navigation; aux disabled"));
    println!(
        "HKSLGT_HISTORY_CHILD_HWND: before navigation children={}",
        child_window_count(main.hwnd().unwrap().0)
    );
    let ping = watchdog(
        main.hwnd().unwrap().0 as usize,
        Arc::clone(&stop.0),
        Arc::clone(&stage),
        started,
    );
    println!(
        "HKSLGT_HISTORY_ENTER: responsive main in toolkit, clicking the bundled player-tabs menu"
    );
    assert!(page_bool(&main, "(() => { document.querySelector('.menu-item[data-key=player-tabs]').click(); return true })()", "enter player-tabs").await);
    tokio::time::timeout(Duration::from_secs(6), async {
        while server.history.requests.lock().unwrap().pages == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("no HN1 history request arrived after clicking the player tab");
    *stage.lock().unwrap() = "history read outstanding; aux disabled";
    println!(
        "HKSLGT_HISTORY_PENDING: native HN1 page read entered at {:?}; aux disabled",
        started.elapsed()
    );
    assert!(
        page_bool(&main, LOADING, "loading with aux still disabled").await,
        "real history route must still be loading"
    );
    input_round_trip(&main, "held first HN1 page with aux disabled").await;
    server.history.release_page();
    tokio::time::timeout(Duration::from_secs(6), async {
        while server.history.requests.lock().unwrap().details == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the production renderer did not request any HN1 game data");
    *stage.lock().unwrap() = "first full page game details and image requests; aux disabled";
    println!("HKSLGT_HISTORY_DETAILS_PENDING: at {:?}, game details for the 20-row page are pending with aux disabled", started.elapsed());
    input_round_trip(&main, "full game data pending with aux disabled").await;
    let before_open = page_bool(&main, COLLAPSED, "sidebar before the room opens").await;
    let cold = app.get_webview_window("aux").is_none();
    println!(
        "HKSLGT_HISTORY_AUX_REQUESTED: at {:?} after the standalone page heartbeat; newWindow={cold}, shared WebView2 directory",
        started.elapsed(),
    );
    *stage.lock().unwrap() = "room sampling while history read outstanding";
    set_preference(app, AUX_WINDOW, "enabled", true);
    server.scene.lock().unwrap().phase = "Lobby";
    let (opening, open_started) = tokio::sync::oneshot::channel();
    let popup = tauri::async_runtime::spawn({
        let app = app.clone();
        let main = main.clone();
        let client = Arc::clone(&client);
        let stage = Arc::clone(&stage);
        async move {
            app.state::<ClientRuntime>()
                .sample_fixture(&app, &client)
                .await;
            assert!(
                page_bool(
                    &main,
                    ARM_ROOM_CLICK,
                    "arm stock history sidebar before room open"
                )
                .await,
                "could not arm the one-shot input in the real main window"
            );
            *stage.lock().unwrap() = "room window opening while history read outstanding";
            println!(
                "HKSLGT_HISTORY_AUX_OPEN: at {:?}, real Lobby sample ready; requesting HWND",
                started.elapsed()
            );
            let _ = opening.send(wall_millis());
            reconcile_companions(&app);
            let open_returned = wall_millis();
            println!(
                "HKSLGT_HISTORY_AUX_OPEN_RETURN: native room open returned at {:?}",
                started.elapsed()
            );
            open_returned
        }
    });
    let open_wall = tokio::time::timeout(Duration::from_secs(3), open_started)
        .await
        .expect("the concurrent Lobby sample did not arm and request the real mini window")
        .unwrap();
    // Turn the pending HTTPS requests loose during the room open, independently of its
    // first input acknowledgement and within the real League transport's 4-second timeout.
    let history = Arc::clone(&server.history);
    let release = tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        history.requests.lock().unwrap().hold_details = false;
        history.release.notify_all();
    });
    let fired_wall = measured_room_click(&main, before_open, open_wall, cold).await;
    let following =
        click_sidebar(&main, before_open, BUDGET, "next input during room opening").await;
    println!("HKSLGT_HISTORY_AUX_INPUT: newWindow={cold}, nextAck={following:?}");
    release.await.unwrap();
    *stage.lock().unwrap() = "first full page rendering alongside room";
    check_renderer(
        &main,
        CARDS,
        "20 ARAM match cards with the fixture player's KDA on first entry",
        || {},
    )
    .await;
    println!(
        "HKSLGT_HISTORY_FIRST_PAGE: 20 ARAM cards rendered at {:?}",
        started.elapsed()
    );
    tokio::time::timeout(Duration::from_secs(12), async {
        while server.history.requests.lock().unwrap().timelines < PAGE_SIZE as usize {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("full HN1 timelines not requested by the bundled history page");
    let returned_wall = tokio::time::timeout(Duration::from_secs(12), popup)
        .await
        .expect("aux opening remained pending")
        .unwrap();
    let aux = super::visible_window(app, "aux").expect("real concurrently opened mini window");
    check_renderer(
        &aux,
        "document.querySelector('.aux-activity[data-phase=Lobby]') != null",
        "visible aux lounge while main is on history",
        || {},
    )
    .await;
    if cold {
        // WebView construction returns before the new document finishes loading. Read the
        // WebView's own recorded load time instead of timing a later automation callback.
        let loaded_wall: f64 = tokio::time::timeout(BUDGET, page_result(&aux,
            "performance.timeOrigin + performance.getEntriesByType('navigation')[0].loadEventEnd"))
            .await.expect("could not read the already-mounted cold room's navigation timing");
        println!("HKSLGT_HISTORY_COLD_OVERLAP: nativeReturnAfterOpenMs={} newDocumentLoadedAfterOpenMs={:.1} inputAfterOpenMs={}",
            returned_wall - open_wall, loaded_wall - open_wall as f64, fired_wall - open_wall);
    }
    input_round_trip(&main, "full history rendered alongside visible room").await;

    *stage.lock().unwrap() = "failed refresh and retry";
    server.history.requests.lock().unwrap().fail = true;
    click_ready_refresh(&main, "refresh from player header").await;
    tokio::time::timeout(Duration::from_secs(6), async {
        while server.history.requests.lock().unwrap().failed_pages == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the stock refresh did not receive the required HTTP 503");
    check_renderer(
        &main,
        "/Failed to Load Match History|加载战绩失败/i.test(document.body.innerText)",
        "explicit history error after the simulated HTTP 503",
        || {},
    )
    .await;
    let failed_page = server.history.requests.lock().unwrap().pages;
    server.history.requests.lock().unwrap().fail = false;
    click_ready_refresh(&main, "retry from player header").await;
    tokio::time::timeout(Duration::from_secs(6), async {
        while server.history.requests.lock().unwrap().pages <= failed_page {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("retry did not reach the HN1 history service");
    tokio::time::timeout(Duration::from_secs(8), async {
        while server.history.requests.lock().unwrap().new_game_reads == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the real renderer did not fetch the new HN1 game ID after recovery");
    check_renderer(
        &main,
        RECOVERED,
        "the new 13/3/19 game visible at the top of the recovered 20-match page",
        || {},
    )
    .await;
    input_round_trip(&main, "history after HTTP 503 retry").await;
    drop(stop);
    let (pings, worst) = ping
        .join()
        .expect("independent Windows main-thread heartbeat");
    let (pages, failed, ranked, profiles, details, timelines, images, peak, timeout) = {
        let state = server.history.requests.lock().unwrap();
        (
            state.pages,
            state.failed_pages,
            state.ranked,
            state.profiles,
            state.details,
            state.timelines,
            state.champion_images,
            state.peak,
            state.timed_out,
        )
    };
    assert!(pings >= 3 && pages >= 3 && failed > 0 && details >= PAGE_SIZE as usize && ranked > 0 && profiles > 0 && !timeout,
        "missing live request/heartbeat coverage: pings={pings}, pages={pages}, ranked={ranked}, profiles={profiles}, gateTimedOut={timeout}");
    println!("HKSLGT_HISTORY_RESPONSIVE: nativePings={pings} worstNative={worst:?} profileReads={profiles} rankReads={ranked} historyReads={pages} detailReads={details} timelineReads={timelines} championImages={images} peakGameReads={peak}; stock sidebar handled input during loading, room opened, HTTP 503 was visible and retry rendered a new game among 20 matches");
    server.scene.lock().unwrap().phase = "None";
    runtime.sample_fixture(app, &client).await;
    reconcile_companions(app);
    set_preference(app, AUX_WINDOW, "enabled", aux_enabled);
    server.scene.lock().unwrap().phase = previous_phase;
    server.history.end();
    main.destroy()
        .expect("destroy only the owned history main window");
    bootstrap::backend_pref("self-update-main", "autoCheckUpdates")
        .unwrap()
        .write(&profile.database.lock().unwrap(), &auto_update)
        .unwrap();
}
