//! Verify missing and out-of-order ChampSelect data through the real sampler and WebView2.

use super::{
    check_renderer, mini_client_fixture::PLAYER, reconcile_companions, set_preference,
    wait_for_close, wait_for_popup, AppHandle, ClientRuntime, Manager, MiniClient, AUX_WINDOW,
    OPGG_WINDOW, RESG_NAMESPACE,
};
use crate::client_discovery::LocalClient;
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) struct DraftFixture {
    pub gameflow: Option<Value>,
    pub selection: Option<Value>,
    pub current_champion: Option<u32>,
}

fn flow(phase: &str, queue: u64, mode: &str) -> Value {
    json!({
        "phase":phase,
        "gameData":{"gameId":7812,"isCustomGame":false,"queue":{"id":queue,"gameMode":mode}}
    })
}

const VISIBLE_CHAMPION: &str = r#"
    document.querySelector('.aux-activity[data-phase="ChampSelect"]') !== null &&
    document.querySelector('.current-champion[data-champion-id="103"] img')?.naturalWidth > 1
"#;
const NO_DRAFT: &str = r#"
    document.querySelector('.aux-activity')?.getAttribute('data-phase') === null &&
    document.querySelector('.current-champion') === null &&
    document.querySelector('.bench') === null
"#;

pub(super) async fn check(app: &AppHandle) {
    set_preference(app, AUX_WINDOW, "enabled", true);
    set_preference(app, OPGG_WINDOW, "enabled", false);
    set_preference(app, RESG_NAMESPACE, "enabled", false);
    let server = app.state::<MiniClient>();
    let runtime = app.state::<ClientRuntime>();
    let client = Arc::new(LocalClient::fixture_at(server.port).expect("own the live TLS listener"));
    let previous_phase = {
        let mut scene = server.scene.lock().unwrap();
        std::mem::replace(&mut scene.phase, "Lobby")
    };
    runtime.sample_fixture(app, &client).await;
    let aux = wait_for_popup(app, "aux", || {}).await;
    let window = aux.hwnd().unwrap();
    check_renderer(
        &aux,
        r#"document.querySelector('.aux-activity[data-phase="Lobby"]') !== null"#,
        "already-visible Mini during Lobby",
        || {},
    )
    .await;

    let mut selection = json!({
        "queueId":2400, "gameId":7812, "isSpectating":false, "isCustomGame":false,
        "benchChampions":[{"championId":22}],
        "myTeam":[{"cellId":1,"championId":55}],
        "timer":{"phase":"FINALIZATION","adjustedTimeLeftInPhase":20110}
    });
    let writes = {
        let mut scene = server.scene.lock().unwrap();
        scene.phase = "ChampSelect";
        scene.partial_kiwi = Some(DraftFixture {
            gameflow: Some(flow("ReadyCheck", 450, "ARAM")),
            selection: Some(selection.clone()),
            current_champion: Some(103),
        });
        scene.writes.len()
    };
    runtime.sample_fixture(app, &client).await;
    let (revision, state) = runtime
        .fields(crate::lcu::aux_activity::NAMESPACE, "state")
        .expect("read the real Mini state provider")
        .expect("Mini state namespace must exist");
    let view = &state["view"];
    println!(
        "HKSLGT_PARTIAL_KIWI_STATE: revision={revision} phase={} mode={} champion={} bench_size={} actionable={}",
        view["phase"], view["gameMode"], view["championId"],
        view["bench"].as_array().map_or(0, Vec::len), view["selectionActionable"],
    );
    assert_eq!(view["phase"], "ChampSelect");
    assert_eq!(view["gameMode"], "KIWI");
    assert_eq!(view["championId"], 103);
    assert_eq!(view["bench"], json!([22]));
    assert_eq!(view["selectionActionable"], false);
    reconcile_companions(app);
    check_renderer(
        &aux,
        VISIBLE_CHAMPION,
        "selected Ahri before synchronized gameflow/local cell",
        || {},
    )
    .await;
    check_renderer(
        &aux,
        r#"document.querySelector('.bench [data-champion-id="22"][aria-disabled="true"] img')?.naturalWidth > 1 &&
           !document.querySelector('.bench [data-champion-id="55"]') &&
           document.querySelector('.aux-draft-waiting[role=status]')?.textContent.trim().length > 0 &&
           document.querySelector('[data-action="leave-game"]')?.disabled === true"#,
        "real bench shown without exposing an ally or enabling any write", || {},
    ).await;
    aux.eval(r#"
        window.__TAURI__.core.invoke('league_call', {
            namespace: 'aux-activity-main', method: 'swapBenchChampion', args: [22]
        }).then(result => {
            document.documentElement.dataset.kiwiPartialWrite = result.success === false ? 'blocked' : 'unexpected'
        })
    "#).unwrap();
    check_renderer(
        &aux,
        "document.documentElement.dataset.kiwiPartialWrite === 'blocked'",
        "native IPC rejects an attempted write from the read-only draft",
        || {},
    )
    .await;
    assert_eq!(server.scene.lock().unwrap().writes.len(), writes);

    server.scene.lock().unwrap().partial_kiwi = Some(DraftFixture {
        gameflow: Some(flow("ChampSelect", 2400, "KIWI")),
        selection: None,
        current_champion: Some(103),
    });
    runtime.sample_fixture(app, &client).await;
    check_renderer(
        &aux,
        &format!("({VISIBLE_CHAMPION}) && document.querySelector('.bench') === null"),
        "known KIWI and real champion survive a missing selection endpoint without fake bench data",
        || {},
    )
    .await;

    selection["localPlayerCellId"] = json!(2);
    selection["myTeam"] = json!([{"cellId":2,"puuid":PLAYER,"championId":103}]);
    selection["benchEnabled"] = json!(true);
    server
        .scene
        .lock()
        .unwrap()
        .partial_kiwi
        .as_mut()
        .unwrap()
        .selection = Some(selection.clone());
    runtime.sample_fixture(app, &client).await;
    check_renderer(
        &aux,
        &format!(r#"({VISIBLE_CHAMPION}) &&
                 document.querySelector('.bench [data-champion-id="22"][aria-disabled="false"]') !== null &&
                 document.querySelector('.aux-draft-waiting') === null"#),
        "recovered verified KIWI data updates the existing draft and re-enables eligible input", || {},
    ).await;
    assert_eq!(aux.hwnd().unwrap(), window);

    selection["gameId"] = json!(7813);
    server
        .scene
        .lock()
        .unwrap()
        .partial_kiwi
        .as_mut()
        .unwrap()
        .selection = Some(selection.clone());
    runtime.sample_fixture(app, &client).await;
    check_renderer(
        &aux,
        NO_DRAFT,
        "clears data on a conflicting game ID",
        || {},
    )
    .await;

    server
        .scene
        .lock()
        .unwrap()
        .partial_kiwi
        .as_mut()
        .unwrap()
        .gameflow = Some(flow("ChampSelect", 420, "CLASSIC"));
    runtime.sample_fixture(app, &client).await;
    check_renderer(
        &aux,
        NO_DRAFT,
        "stale KIWI should not survive a known CLASSIC draft",
        || {},
    )
    .await;

    selection["gameId"] = json!(7812);
    server.scene.lock().unwrap().partial_kiwi = Some(DraftFixture {
        gameflow: Some(flow("ChampSelect", 2400, "KIWI")),
        selection: Some(selection),
        current_champion: Some(103),
    });
    runtime.sample_fixture(app, &client).await;
    check_renderer(
        &aux,
        VISIBLE_CHAMPION,
        "current KIWI before disconnect",
        || {},
    )
    .await;
    runtime
        .select(app, None)
        .expect("disconnect only the owned fixture");
    check_renderer(
        &aux,
        NO_DRAFT,
        "disconnect clears the rendered game and roster",
        || {},
    )
    .await;
    reconcile_companions(app);
    wait_for_close(app, "aux", || {}).await;
    {
        let mut scene = server.scene.lock().unwrap();
        assert_eq!(scene.writes.len(), writes);
        scene.partial_kiwi = None;
        scene.phase = previous_phase;
    }
    println!("Win32/WebView2 KIWI: preserved the real visible draft over an out-of-order gameflow and missing roster, blocked partial writes, and cleared changed/disconnected drafts");
}
