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
use serde_json::{json, Value};
use std::{fs, panic::AssertUnwindSafe, sync::Mutex, time::Duration};
use tauri::{AppHandle, Manager, RunEvent, WebviewWindow};
use tokio::time::Instant;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindowVisible};

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
        system_locale: "en",
    };
    let runtime =
        ClientRuntime::new(false, Default::default()).expect("disconnected fixture runtime");
    let app = tauri::Builder::default()
        .any_thread()
        .manage(profile)
        .manage(runtime)
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
    check_bundled_draft(&opgg, "ARAM Mayhem", 22, kiwi).await;
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
    check_bundled_draft(&ordinary, "ARAM", 55, aram).await;
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
}

async fn check_aux_room_and_selection(app: &AppHandle) {
    let runtime = app.state::<ClientRuntime>();
    let lobby = json!({
        "gameConfig": { "queueId": 450, "gameMode": "ARAM", "isCustom": false },
        "members": [{"puuid": "fixture-player"}, {"puuid": "fixture-teammate"}],
    });
    let search = json!({
        "queueId": 450,
        "timeInQueue": 27.9,
        "estimatedQueueTime": 54.25,
        "readyCheck": { "playerResponse": "Accepted" },
    });
    let room_phase = |phase, search| {
        runtime.apply_companion_fixture(
            app,
            flow("ARAM", phase, 0),
            Value::Null,
            Some(lobby.clone()),
            search,
        );
    };
    let room = || room_phase("Lobby", None);
    let aux = wait_for_popup(app, "aux", room).await;
    check_renderer(
        &aux,
        r#"document.querySelector('.aux-activity[data-phase="Lobby"]') != null &&
            document.querySelector('.party-size')?.textContent.trim() === '2'"#,
        "aux ARAM lobby and its two players",
        room,
    )
    .await;

    let searching = || room_phase("Matchmaking", Some(search.clone()));
    check_renderer(
        &aux,
        r#"document.querySelector('.aux-activity[data-phase="Matchmaking"]') != null &&
            document.querySelector('.queue-time')?.textContent.includes('27')"#,
        "aux live queue time",
        searching,
    )
    .await;

    let ready = || room_phase("ReadyCheck", Some(search.clone()));
    check_renderer(
        &aux,
        r#"document.querySelector('.aux-activity[data-phase="ReadyCheck"]') != null &&
            document.querySelector('.ready-response')?.textContent.trim() === 'Accepted'"#,
        "aux accepted ready check",
        ready,
    )
    .await;
    aux.close()
        .expect("dismiss the auxiliary panel in the room stage");
    wait_for_close(app, "aux", ready).await;
    let until = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < until {
        ready();
        reconcile_companions(app);
        assert_hidden(
            app,
            "aux",
            "dismissed auxiliary panel must stay closed in the room stage",
        );
        tokio::time::sleep(POLL).await;
    }

    let select = |champion, bench| {
        let selection = json!({
            "queueId": 450,
            "gameId": 0,
            "isCustomGame": false,
            "isSpectating": false,
            "localPlayerCellId": 2,
            "myTeam": [{ "cellId": 1, "championId": 55 }, { "cellId": 2, "championId": champion }],
            "benchChampions": [{ "championId": bench }],
            "timer": { "adjustedTimeLeftInPhase": 17810 },
        });
        runtime.apply_companion_fixture(app, flow("ARAM", "ChampSelect", 0), selection, None, None);
    };
    let initial = || select(22, 103);
    let aux = wait_for_popup(app, "aux", initial).await;
    check_renderer(
        &aux,
        r#"document.querySelector('.aux-activity[data-phase="ChampSelect"]') != null &&
            document.querySelector('.current-champion[data-champion-id="22"]') != null &&
            document.querySelector('.bench li[data-champion-id="103"]') != null &&
            document.querySelector('.remaining-time')?.textContent.includes('18')"#,
        "aux early selection with current champion, bench, and 18-second timer",
        initial,
    )
    .await;

    let swapped = || select(103, 22);
    check_renderer(
        &aux,
        r#"document.querySelector('.current-champion[data-champion-id="103"]') != null &&
            document.querySelector('.bench li[data-champion-id="22"]') != null"#,
        "aux live update after a champion swap",
        swapped,
    )
    .await;
    assert_hidden(
        app,
        "opgg",
        "OP.GG stays disabled in the independent aux scene",
    );
    assert_hidden(
        app,
        "resg",
        "RESG stays disabled in the independent aux scene",
    );

    let started = || sample(app, "ARAM", "InProgress", 7105, 103);
    started();
    reconcile_companions(app);
    wait_for_close(app, "aux", started).await;
    println!("Aux opened from a synthetic ARAM lobby with live party, queue, and ready-check state; dismissing it in the room allowed a new popup in early selection with a live champion swap");
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
    champion: u32,
    refresh: impl Fn(),
) {
    let script = format!(
        r#"location.pathname.endsWith('/opgg-window.html') && window.__TAURI__?.core != null &&
            document.querySelector('.opgg-companion .mode')?.textContent.trim() === {} &&
            document.querySelector('.opgg-companion h1')?.textContent.trim() === '{}' &&
            document.querySelector('.opgg-companion button')?.textContent.includes('OP.GG')"#,
        json!(mode),
        champion
    );
    check_renderer(
        window,
        &script,
        &format!("OP.GG {mode} draft and champion {champion}"),
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
        assert!(
            Instant::now() < deadline,
            "bundled WebView2 did not render {expected}"
        );
        tokio::time::sleep(POLL).await;
    }
}
