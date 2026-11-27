//! Opt-in acceptance test for the bundled WebView2 companion windows on a disposable runner.

use super::{
    aux_window::AuxWindow, bootstrap, handle_window_event, serve_game_art, SettingsRevision,
    AUX_WINDOW, GAME_ART_SCHEME, OPGG_WINDOW, RESG_NAMESPACE,
};
use crate::{
    client_runtime::{reconcile_companions, ClientRuntime},
    lcu::GameflowSession,
    opgg_window::{OpggPreferences, OpggWindow},
    profile::DesktopProfile,
    resg_window::{ResgAction, ResgWindow},
};
use maxgamestudio_league_core::profile_database::LeagueProfileDatabase;
use serde_json::json;
use std::{fs, panic::AssertUnwindSafe, sync::Mutex, time::Duration};
use tauri::{AppHandle, Manager, RunEvent, WebviewWindow};
use tokio::time::Instant;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindowVisible};

#[path = "mini_client_fixture.rs"]
mod mini_client_fixture;
use mini_client_fixture::MiniClient;
#[path = "champ_select_event_smoke.rs"]
mod champ_select_event_smoke;
#[path = "champion_picker_smoke.rs"]
mod champion_picker_smoke;
#[path = "history_responsiveness_smoke.rs"]
mod history_responsiveness_smoke;
#[path = "consecutive_kiwi_smoke.rs"]
mod consecutive_kiwi_smoke;
#[path = "mini_optional_actions_smoke.rs"]
mod mini_optional_actions_smoke;
#[path = "partial_kiwi_selection_smoke.rs"]
mod partial_kiwi_selection_smoke;
#[path = "post_game_smoke.rs"]
mod post_game_smoke;

const OPEN_TIMEOUT: Duration = Duration::from_secs(40);
const POLL: Duration = Duration::from_millis(200);

#[test]
#[ignore = "requires a disposable Windows desktop with WebView2 and the bundled renderer"]
fn champ_select_opens_real_windows() {
    let temp = tempfile::Builder::new()
        .prefix("hkslgt-companion-")
        .tempdir()
        .expect("isolated acceptance profile");
    let directory = temp.path().join("profile");
    fs::create_dir(&directory).expect("create fixture profile directory");
    let db = LeagueProfileDatabase::open(&directory).expect("open fixture database");
    bootstrap::backend_pref(AUX_WINDOW, "showSkinSelector")
        .unwrap()
        .write(&db, &json!(true))
        .unwrap();
    for namespace in [AUX_WINDOW, OPGG_WINDOW, RESG_NAMESPACE] {
        bootstrap::backend_pref(namespace, "enabled")
            .unwrap()
            .write(&db, &json!(false))
            .unwrap();
        bootstrap::backend_pref(namespace, "autoShow")
            .unwrap()
            .write(&db, &json!(true))
            .unwrap();
    }
    let resg = bootstrap::resg_preferences(&db).expect("read fixture RESG settings");
    let profile = DesktopProfile {
        directory,
        database: Mutex::new(db),
        system_locale: if champion_picker_smoke::requested() || post_game_smoke::requested() {
            "zh-CN"
        } else {
            "en"
        },
    };
    let mut runtime =
        ClientRuntime::new(false, Default::default()).expect("disconnected fixture runtime");
    let mini_client = MiniClient::start();
    let selection_fixture = champ_select_event_smoke::requested()
        .then(|| std::sync::Arc::new(champ_select_event_smoke::SelectionFixture::start()));
    let fixture_root = selection_fixture
        .as_ref()
        .map(|fixture| fixture.root.clone())
        .unwrap_or_else(|| mini_client.root.clone());
    runtime.use_fixture_transport(fixture_root);
    let app = tauri::Builder::default()
        .any_thread()
        .manage(profile)
        .manage(runtime)
        .manage(mini_client)
        .manage(selection_fixture)
        .manage(AuxWindow::new(false, true))
        .manage(OpggWindow::new(OpggPreferences {
            enabled: false,
            auto_show: true,
        }))
        .manage(ResgWindow::new(resg))
        .manage(SettingsRevision::default())
        .register_asynchronous_uri_scheme_protocol(GAME_ART_SCHEME, serve_game_art)
        .invoke_handler(tauri::generate_handler![super::league_call])
        .on_window_event(handle_window_event)
        .build(tauri::generate_context!())
        .expect("start real Wry runtime");

    let (send, receive) = std::sync::mpsc::sync_channel(1);
    let mut send = Some(send);
    let exit = app.run_return(move |app, event| match event {
        RunEvent::Ready => {
            let app = app.clone();
            let send = send.take().expect("Wry startup should run once");
            // Creating a WebView2 from the synchronous Windows event handler can deadlock.
            std::thread::spawn(move || {
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    tauri::async_runtime::block_on(exercise(&app));
                }));
                let code = if result.is_ok() { 0 } else { 1 };
                send.send(result).expect("return the acceptance outcome");
                app.exit(code);
            });
        }
        RunEvent::ExitRequested {
            code: None, api, ..
        } => api.prevent_exit(),
        _ => {}
    });
    let result = receive
        .try_recv()
        .expect("fixture exited before its checks ran");
    if let Err(failure) = result {
        std::panic::resume_unwind(failure);
    }
    assert_eq!(exit, 0, "real Wry event loop exited unexpectedly");
}

async fn exercise(app: &AppHandle) {
    if champ_select_event_smoke::requested() {
        champ_select_event_smoke::check(app).await;
        return;
    }
    if post_game_smoke::requested() {
        post_game_smoke::check(app).await;
        return;
    }
    if champion_picker_smoke::requested() {
        champion_picker_smoke::check(app).await;
        return;
    }
    if std::env::var_os("HKSLGT_PARTIAL_KIWI_ONLY").is_some() {
        partial_kiwi_selection_smoke::check(app).await;
        return;
    }
    if consecutive_kiwi_smoke::requested() {
        consecutive_kiwi_smoke::check(app).await;
        return;
    }
    if std::env::var_os("HKSLGT_HISTORY_ONLY").is_some()
        || std::env::var_os("HKSLGT_HISTORY_CDN_ONLY").is_some()
    {
        history_responsiveness_smoke::check(app).await;
        return;
    }
    sample(app, "KIWI", "ChampSelect", 7100, 22);
    reconcile_companions(app);
    assert_hidden(app, "aux", "aux disabled during KIWI selection");
    assert_hidden(app, "opgg", "OP.GG disabled during KIWI selection");
    assert_hidden(app, "resg", "RESG disabled during KIWI selection");
    for namespace in [AUX_WINDOW, OPGG_WINDOW, RESG_NAMESPACE] {
        set_preference(app, namespace, "enabled", true);
        set_preference(app, namespace, "autoShow", false);
    }
    sample(app, "KIWI", "ChampSelect", 7100, 22);
    reconcile_companions(app);
    for label in ["aux", "opgg", "resg"] {
        assert_hidden(app, label, "Enabled companion must honor autoShow=false");
    }
    for namespace in [AUX_WINDOW, OPGG_WINDOW, RESG_NAMESPACE] {
        set_preference(app, namespace, "autoShow", true);
    }

    for (mode, phase, game) in [("CLASSIC", "Lobby", 7101), ("CLASSIC", "ChampSelect", 7102)] {
        sample(app, mode, phase, game, 22);
        reconcile_companions(app);
        assert_hidden(app, "aux", "No aux for unrelated phase or queue");
        assert_hidden(app, "opgg", "No OP.GG for unrelated phase or queue");
        assert_hidden(app, "resg", "No RESG for unrelated phase or queue");
    }

    set_preference(app, AUX_WINDOW, "enabled", false);
    let blocked_webview_dir = app.state::<DesktopProfile>().directory.join("WebView2");
    fs::write(
        &blocked_webview_dir,
        b"simulate a failed first open in this isolated fixture",
    )
    .expect("block the fresh fixture's WebView2 directory");
    let kiwi = || sample(app, "KIWI", "ChampSelect", 7103, 22);
    kiwi();
    let runtime = app.state::<ClientRuntime>();
    assert_eq!(
        runtime.opgg_target().unwrap().view(),
        json!({"gameMode":"KIWI","championId":22})
    );
    assert_eq!(runtime.resg_target().unwrap().champion_id, 22);
    assert!(app.state::<OpggWindow>().reconcile(app).is_err());
    let resg_after_failure = app
        .state::<ResgWindow>()
        .reconcile(app, ResgAction::Synchronize)
        .unwrap();
    assert_eq!(resg_after_failure["error"], "page-unavailable");
    assert_hidden(app, "opgg", "genuine initial OP.GG window opening failure");
    assert_hidden(app, "resg", "genuine initial RESG window opening failure");
    fs::remove_file(blocked_webview_dir).expect("remove only the test's blocking file");

    let opgg = wait_for_popup(app, "opgg", kiwi).await;
    let resg = wait_for_popup(app, "resg", kiwi).await;
    check_bundled_draft(&opgg, "ARAM Mayhem", "22", kiwi).await;
    println!("Recovered first-open failures in the same KIWI draft and verified visible native OP.GG/RESG windows and bundled OP.GG content");

    set_preference(app, AUX_WINDOW, "enabled", true);
    let aux = wait_for_popup(app, "aux", kiwi).await;
    check_bundled_aux(&aux, kiwi).await;
    opgg.close()
        .expect("send OP.GG native window close request");
    aux.close().expect("send aux native window close request");
    resg.close().expect("send RESG native window close request");
    for label in ["aux", "opgg", "resg"] {
        wait_for_close(app, label, kiwi).await;
    }
    let until = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < until {
        kiwi();
        assert!(
            runtime.opgg_target().is_some(),
            "same KIWI draft is still current"
        );
        reconcile_companions(app);
        assert_hidden(
            app,
            "aux",
            "explicitly closed aux stays closed in this draft",
        );
        assert_hidden(
            app,
            "opgg",
            "explicitly closed OP.GG stays closed in this draft",
        );
        assert_hidden(
            app,
            "resg",
            "explicitly closed RESG stays closed in this match",
        );
        tokio::time::sleep(POLL).await;
    }
    println!("Explicit close stayed closed through repeated reconciliation of the same KIWI draft");

    kiwi();
    let manual = app
        .state::<ResgWindow>()
        .reconcile(app, ResgAction::Show)
        .unwrap();
    assert_eq!(
        manual["visible"], true,
        "manual show must reopen RESG in the same KIWI match"
    );
    assert!(
        visible_window(app, "resg").is_some(),
        "manual RESG reopening produced a visible HWND"
    );
    app.state::<ResgWindow>()
        .reconcile(app, ResgAction::Close)
        .unwrap();
    wait_for_close(app, "resg", kiwi).await;
    kiwi();
    reconcile_companions(app);
    assert_hidden(
        app,
        "resg",
        "manually reopened RESG stays closed after a second close",
    );
    println!("Manual RESG Show reopened the same KIWI match and a second explicit close hid it");

    set_preference(app, OPGG_WINDOW, "enabled", false);
    set_preference(app, RESG_NAMESPACE, "enabled", false);
    check_aux_room_and_selection(app).await;

    let aram = || sample(app, "ARAM", "ChampSelect", 7104, 55);
    check_bundled_aux(&wait_for_popup(app, "aux", aram).await, aram).await;
    assert_hidden(app, "opgg", "aux automatically opens with OP.GG disabled");
    assert_hidden(app, "resg", "aux automatically opens with RESG disabled");

    set_preference(app, OPGG_WINDOW, "enabled", true);
    set_preference(app, RESG_NAMESPACE, "enabled", true);
    let ordinary = wait_for_popup(app, "opgg", aram).await;
    // The live TLS fixture loaded the champion catalog during the preceding room sequence.
    assert_eq!(
        runtime.opgg_target().unwrap().view(),
        json!({"gameMode":"ARAM","championId":55})
    );
    check_bundled_draft(&ordinary, "ARAM", "Katarina", aram).await;
    assert_hidden(app, "resg", "ordinary ARAM has no Hextech RESG popup");
    assert!(
        visible_window(app, "aux").is_some(),
        "automatically opened aux is visible before leaving selection"
    );
    let started = || sample(app, "ARAM", "InProgress", 7104, 55);
    started();
    assert!(
        matches!(
            runtime.aux_observation(),
            super::aux_window::AuxObservation::Inactive
        ),
        "runtime observed a confirmed end of selection"
    );
    reconcile_companions(app);
    wait_for_close(app, "opgg", started).await;
    wait_for_close(app, "aux", started).await;
    println!("An ordinary ARAM draft opened its own bundled OP.GG and aux popups and leaving ChampSelect closed them");
    history_responsiveness_smoke::check(app).await;
    partial_kiwi_selection_smoke::check(app).await;
}

async fn check_aux_room_and_selection(app: &AppHandle) {
    let server = app.state::<MiniClient>();
    let client = std::sync::Arc::new(
        crate::client_discovery::LocalClient::fixture_at(server.port)
            .expect("own the live test listener"),
    );
    // Use the production reader against a real TLS socket. Only discovery and the CA are test inputs.
    let monitor = tauri::async_runtime::spawn({
        let app = app.clone();
        let client = std::sync::Arc::clone(&client);
        async move {
            loop {
                app.state::<ClientRuntime>()
                    .sample_fixture(&app, &client)
                    .await;
                reconcile_companions(&app);
                tokio::time::sleep(POLL).await;
            }
        }
    });
    let aux = wait_for_popup(app, "aux", || {}).await;
    check_renderer(
        &aux,
        r#"document.querySelector('.aux-activity[data-phase="Lobby"]') != null &&
                     document.querySelector('[data-setting=autoMatchmakingEnabled]') != null"#,
        "original ARAM lounge with the auto-matchmaking switch",
        || {},
    )
    .await;
    let no_writes = server.scene.lock().unwrap().writes.len();
    assert_eq!(
        no_writes, 0,
        "opening the mini window must not submit manual actions"
    );
    click(&aux, "[data-setting='autoMatchmakingEnabled']").await;
    check_renderer(
        &aux,
        r#"(() => {
            const caption = document.querySelector('.queue-time')?.textContent ?? '';
            const elapsed = Number.parseFloat(caption);
            return !!document.querySelector('.aux-activity[data-phase="Matchmaking"]') &&
                elapsed >= 27.9 && elapsed < 32 && caption.includes('/ 54.3 s');
        })()"#,
        "server-read queue time after clicking Start",
        || {},
    )
    .await;
    click(&aux, "[data-action='cancel-matchmaking']").await;
    check_renderer(
        &aux,
        "document.querySelector('[data-phase=Lobby]') != null",
        "return to lobby after cancel",
        || {},
    )
    .await;
    server.scene.lock().unwrap().phase = "ReadyCheck";
    check_renderer(
        &aux,
        "document.querySelector('[data-action=decline-ready-check]') != null",
        "original ready check buttons",
        || {},
    )
    .await;
    server.scene.lock().unwrap().reject_next = true;
    click(&aux, "[data-action='accept-ready-check']").await;
    check_renderer(&aux, "document.querySelector('.n-message--warning-type, .n-message--error-type, [role=alert]') != null",
        "a rejected ready check displays a recoverable error", || {}).await;
    assert_eq!(server.scene.lock().unwrap().response, "None");
    click(&aux, "[data-action='decline-ready-check']").await;
    check_renderer(&aux, "document.querySelector('[data-phase=ReadyCheck]')?.textContent.toLowerCase().includes('declined') === true", "declined state from the server", || {}).await;
    click(&aux, "[data-action='accept-ready-check']").await;
    check_renderer(
        &aux,
        "document.querySelector('.ready-response')?.textContent.includes('ccepted') === true",
        "accepted state from the server",
        || {},
    )
    .await;
    aux.close().expect("dismiss the room stage");
    wait_for_close(app, "aux", || {}).await;
    tokio::time::sleep(Duration::from_millis(2400)).await;
    assert_hidden(
        app,
        "aux",
        "explicit dismissal remains effective during ready check",
    );

    {
        let mut scene = server.scene.lock().unwrap();
        scene.phase = "ChampSelect";
        scene.choosing = true;
        scene.champion = 0;
    }
    let aux = wait_for_popup(app, "aux", || {}).await;
    let choose_ashe = ".bench [data-champion-id='22']";
    check_renderer(&aux, r#"document.querySelector('.bench [data-champion-id="22"]')?.getAttribute('aria-disabled') === 'false'"#, "local pick action in the client subset", || {}).await;
    aux.eval(format!("document.querySelector({}).dispatchEvent(new MouseEvent('contextmenu', {{bubbles:true,button:2}}))", json!(choose_ashe))).unwrap();
    let until = Instant::now() + OPEN_TIMEOUT;
    while server.scene.lock().unwrap().intent != 22 {
        assert!(
            Instant::now() < until,
            "right-click did not preview the champion"
        );
        tokio::time::sleep(POLL).await;
    }
    assert!(
        server.scene.lock().unwrap().choosing,
        "preview must leave selection open"
    );
    click(&aux, choose_ashe).await;
    check_renderer(&aux, r#"document.querySelector('.current-champion[data-champion-id="22"] img')?.naturalWidth > 0 &&
                document.querySelector('.bench [data-champion-id="103"] img')?.naturalWidth > 0"#,
        "the selected and available bench champion portraits decoded in the WebView", || {}).await;
    assert_color_portrait(&aux, ".bench [data-champion-id='103'] img").await;
    click(&aux, ".bench [data-champion-id='103']").await;
    check_renderer(&aux, r#"document.querySelector('.current-champion[data-champion-id="103"] img')?.naturalWidth > 0 &&
                    document.querySelector('.bench [data-champion-id="22"] img')?.naturalWidth > 0"#,
        "clicking the bench really swaps the current champion and updates the bench", || {}).await;
    {
        let state = server.scene.lock().unwrap();
        assert_eq!((state.champion, state.bench), (103, 22));
        let observed: Vec<_> = state
            .writes
            .iter()
            .map(|(method, path)| (method.as_str(), path.as_str()))
            .collect();
        assert_eq!(
            observed,
            [
                ("POST", "/lol-lobby/v2/lobby/matchmaking/search"),
                ("DELETE", "/lol-lobby/v2/lobby/matchmaking/search"),
                ("POST", "/lol-matchmaking/v1/ready-check/accept"),
                ("POST", "/lol-matchmaking/v1/ready-check/decline"),
                ("POST", "/lol-matchmaking/v1/ready-check/accept"),
                ("PATCH", "/lol-champ-select/v1/session/actions/17"),
                ("PATCH", "/lol-champ-select/v1/session/actions/17"),
                ("POST", "/lol-champ-select/v1/session/bench/swap/103"),
            ]
        );
    }
    mini_optional_actions_smoke::check_optional_actions(&aux, &server).await;
    assert_hidden(app, "opgg", "aux is independent of OP.GG");
    assert_hidden(app, "resg", "aux is independent of RESG");
    monitor.abort();
    let _ = monitor.await;
    let writes = server.scene.lock().unwrap().writes.len();
    server.scene.lock().unwrap().phase = "InProgress";
    click(&aux, ".bench [data-champion-id='22']").await;
    check_renderer(
        &aux,
        "document.body.innerText.includes('英雄选择阶段已经结束')",
        "a last-frame bench click reports that champion select already ended",
        || {},
    )
    .await;
    assert_eq!(
        server.scene.lock().unwrap().writes.len(),
        writes,
        "a stale mini window must not send a game action"
    );
    app.state::<ClientRuntime>()
        .sample_fixture(app, &client)
        .await;
    reconcile_companions(app);
    wait_for_close(app, "aux", || {}).await;
    println!("Win32/WebView2 original mini: start/cancel, rejected and retried ready check, decline/accept and champion swap reached the TLS fixture exactly once per click; checked decoded colored portraits and state readback.");
}

async fn click(window: &WebviewWindow, selector: &str) {
    let condition = format!("(() => {{ const el = document.querySelector({}); return !!el && !el.disabled && el.getAttribute('aria-disabled') !== 'true' }})()", json!(selector));
    check_renderer(window, &condition, selector, || {}).await;
    // Execute exactly once. Polling a click can hide duplicate submissions and false successes.
    window
        .eval(format!(
            "document.querySelector({}).click()",
            json!(selector)
        ))
        .expect("click fixture control");
}

async fn assert_color_portrait(window: &WebviewWindow, selector: &str) {
    let script = format!(
        r#"
        (async () => {{
            const icon = document.querySelector({});
            const copy = new Image();
            copy.crossOrigin = 'anonymous';
            copy.src = icon.currentSrc;
            await copy.decode();
            const canvas = document.createElement('canvas');
            const ctx = canvas.getContext('2d', {{ willReadFrequently: true }});
            ctx.drawImage(copy, 0, 0, 16, 16);
            const pixels = ctx.getImageData(0,0,16,16).data;
            const colored = Array.from({{length:256}}, (_,i) => i * 4).filter(i =>
                pixels[i + 3] > 120 && Math.max(...pixels.slice(i,i+3)) - Math.min(...pixels.slice(i,i+3)) > 18
            ).length;
            let visibleColor = true;
            for (let node = icon; node; node = node.parentElement) {{
                for (const gray of getComputedStyle(node).filter.matchAll(/grayscale\(([\d.]+)%?\)/g)) {{
                    if (Number(gray[1]) > 0) visibleColor = false;
                }}
            }}
            document.documentElement.dataset.smokeColored = colored > 5 && visibleColor ? 'yes' : 'no';
        }})().catch(() => document.documentElement.dataset.smokeColored = 'decode-error');"#,
        json!(selector)
    );
    window.eval(script).unwrap();
    check_renderer(
        window,
        "document.documentElement.dataset.smokeColored === 'yes'",
        "visible non-gray pixels in the enabled champion portrait",
        || {},
    )
    .await;
}

fn queue_id(mode: &str) -> u32 {
    match mode {
        "ARAM" => 450,
        "KIWI" => 2400,
        _ => 420,
    }
}

fn flow(mode: &str, phase: &str, game: u64) -> GameflowSession {
    serde_json::from_value(json!({
        "phase": phase,
        "gameData": {
            "gameId": game,
            "isCustomGame": false,
            "queue": { "id": queue_id(mode), "gameMode": mode, "type": mode }
        }
    }))
    .unwrap()
}

fn sample(app: &AppHandle, mode: &str, phase: &str, game: u64, champion: u32) {
    let selection = json!({
        "queueId": queue_id(mode),
        "gameId": game,
        "isCustomGame": false,
        "isSpectating": false,
        "localPlayerCellId": 1,
        "myTeam": [{ "cellId": 1, "championId": champion }]
    });
    app.state::<ClientRuntime>().apply_companion_fixture(
        app,
        flow(mode, phase, game),
        selection,
        None,
        None,
    );
}

fn set_preference(app: &AppHandle, namespace: &str, key: &str, value: bool) {
    let profile = app.state::<DesktopProfile>();
    let db = profile.database.lock().unwrap();
    bootstrap::backend_pref(namespace, key)
        .unwrap()
        .write(&db, &json!(value))
        .unwrap();
    let resg = (namespace == RESG_NAMESPACE).then(|| bootstrap::resg_preferences(&db).unwrap());
    drop(db);
    match namespace {
        AUX_WINDOW => app.state::<AuxWindow>().update_setting(key, value).unwrap(),
        OPGG_WINDOW => app
            .state::<OpggWindow>()
            .update_setting(key, value)
            .unwrap(),
        RESG_NAMESPACE => app
            .state::<ResgWindow>()
            .replace_preferences(resg.unwrap())
            .unwrap(),
        _ => unreachable!("fixture preference belongs to a companion"),
    }
}

fn visible_window(app: &AppHandle, label: &str) -> Option<WebviewWindow> {
    let window = app.get_webview_window(label)?;
    let hwnd = window.hwnd().ok()?.0;
    let mut process = 0;
    // This HWND came directly from the fixture's Wry window and the OS verifies its owner.
    let visible =
        unsafe { GetWindowThreadProcessId(hwnd, &mut process) != 0 && IsWindowVisible(hwnd) != 0 };
    if !visible || !window.is_visible().ok()? {
        return None;
    }
    assert_eq!(process, std::process::id(), "{label} HWND owner");
    let size = window.inner_size().ok()?;
    assert!(size.width > 0 && size.height > 0, "{label} bounds");
    Some(window)
}

fn assert_hidden(app: &AppHandle, label: &str, expectation: &str) {
    assert!(visible_window(app, label).is_none(), "{expectation}");
}

async fn wait_for_popup(app: &AppHandle, label: &str, refresh: impl Fn()) -> WebviewWindow {
    let deadline = Instant::now() + OPEN_TIMEOUT;
    loop {
        refresh();
        reconcile_companions(app);
        if let Some(window) = visible_window(app, label) {
            return window;
        }
        assert!(
            Instant::now() < deadline,
            "{label} did not open a visible HWND (created: {}, current OP.GG draft: {})",
            app.get_webview_window(label).is_some(),
            app.state::<ClientRuntime>().opgg_target().is_some()
        );
        tokio::time::sleep(POLL).await;
    }
}

async fn wait_for_close(app: &AppHandle, label: &str, refresh: impl Fn()) {
    let deadline = Instant::now() + OPEN_TIMEOUT;
    loop {
        refresh();
        if visible_window(app, label).is_none() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{label} did not close after its close trigger"
        );
        tokio::time::sleep(POLL).await;
    }
}

async fn check_bundled_draft(
    window: &WebviewWindow,
    mode: &str,
    champion_title: &str,
    refresh: impl Fn(),
) {
    let script = format!(
        r#"location.pathname.endsWith('/opgg-window.html') && window.__TAURI__?.core != null &&
            document.querySelector('.opgg-companion .mode')?.textContent.trim() === {} &&
            document.querySelector('.opgg-companion h1')?.textContent.trim() === {} &&
            document.querySelector('.opgg-companion button')?.textContent.includes('OP.GG')"#,
        json!(mode),
        json!(champion_title)
    );
    check_renderer(
        window,
        &script,
        &format!("OP.GG {mode} draft and champion {champion_title}"),
        refresh,
    )
    .await;
}

async fn check_bundled_aux(window: &WebviewWindow, refresh: impl Fn()) {
    let script = r#"location.pathname.endsWith('/aux-window.html') && window.__TAURI__?.core != null &&
        document.querySelector('#app')?.hasAttribute('data-v-app') === true"#;
    check_renderer(window, script, "mounted auxiliary panel", refresh).await;
}

async fn check_renderer(window: &WebviewWindow, script: &str, expected: &str, refresh: impl Fn()) {
    let deadline = Instant::now() + OPEN_TIMEOUT;
    loop {
        refresh();
        let (send, receive) = tokio::sync::oneshot::channel();
        let send = Mutex::new(Some(send));
        window
            .eval_with_callback(script, move |value| {
                if let Some(send) = send.lock().unwrap().take() {
                    let _ = send.send(value);
                }
            })
            .expect("evaluate bundled companion renderer in real WebView2");
        if let Ok(Ok(result)) = tokio::time::timeout(POLL, receive).await {
            if serde_json::from_str::<bool>(&result).is_ok_and(|shown| shown) {
                return;
            }
        }
        if Instant::now() >= deadline {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let tx = Mutex::new(Some(tx));
            window.eval_with_callback(
                "JSON.stringify({text:document.body.innerText.slice(0,500),phase:document.querySelector('[data-phase]')?.dataset.phase,actions:[...document.querySelectorAll('[data-action]')].map(n=>n.dataset.action),color:document.documentElement.dataset.smokeColored})",
                move |value| { if let Some(tx) = tx.lock().unwrap().take() { let _ = tx.send(value); } },
            ).expect("inspect synthetic fixture UI");
            let detail = tokio::time::timeout(Duration::from_secs(2), rx).await;
            panic!("bundled WebView2 did not render {expected}: {detail:?}")
        }
        tokio::time::sleep(POLL).await;
    }
}
