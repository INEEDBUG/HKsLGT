//! Exercise the stock automation picker in Win32 using the isolated League TLS fixture.
use super::{
    assert_color_portrait, check_renderer, AppHandle, ClientRuntime, DesktopProfile, Manager,
    MiniClient, WebviewWindow,
};
use crate::client_discovery::LocalClient;
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{WebviewUrl, WebviewWindowBuilder};

pub(super) fn requested() -> bool {
    std::env::var_os("HKSLGT_CHAMPION_PICKER_ONLY").is_some()
}

pub(super) fn champion_summary() -> Value {
    // Sample from the zh_CN League client catalog, including separate League Classic IDs.
    json!([
        {"id":-1,"name":"无","alias":"None","squarePortraitPath":"/lol-game-data/assets/v1/champion-icons/-1.png","roles":[]},
        {"id":22,"name":"寒冰射手","description":"艾希","alias":"Ashe","squarePortraitPath":"/lol-game-data/assets/v1/champion-icons/22.png","roles":["marksman","support"]},
        {"id":55,"name":"不祥之刃","description":"卡特琳娜","alias":"Katarina","squarePortraitPath":"/lol-game-data/assets/v1/champion-icons/55.png","roles":["assassin","mage"]},
        {"id":64,"name":"盲僧","description":"李青","alias":"LeeSin","squarePortraitPath":"/lol-game-data/assets/v1/champion-icons/64.png","roles":["fighter","assassin"]},
        {"id":86,"name":"德玛西亚之力","description":"盖伦","alias":"Garen","squarePortraitPath":"/lol-game-data/assets/v1/champion-icons/86.png","roles":["fighter","tank"]},
        {"id":103,"name":"九尾妖狐","description":"阿狸","alias":"Ahri","squarePortraitPath":"/lol-game-data/assets/v1/champion-icons/103.png","roles":["mage","assassin"]},
        {"id":117,"name":"仙灵女巫","description":"璐璐","alias":"Lulu","squarePortraitPath":"/lol-game-data/assets/v1/champion-icons/117.png","roles":["support","mage"]},
        {"id":136,"name":"铸星龙王","description":"奥瑞利安·索尔","alias":"AurelionSol","squarePortraitPath":"/lol-game-data/assets/v1/champion-icons/136.png","roles":["mage"]},
        {"id":60022,"name":"寒冰射手","description":"艾希","alias":"Jade_Ashe","squarePortraitPath":"/lol-game-data/assets/v1/champion-icons/60022.png","roles":["marksman","support"]},
        {"id":60103,"name":"九尾妖狐","description":"阿狸","alias":"Jade_Ahri","squarePortraitPath":"/lol-game-data/assets/v1/champion-icons/60103.png","roles":["mage","assassin"]},
        {"id":60117,"name":"仙灵女巫","description":"璐璐","alias":"Jade_Lulu","squarePortraitPath":"/lol-game-data/assets/v1/champion-icons/60117.png","roles":["support","mage"]}
    ])
}

pub(super) fn recommended_positions() -> Value {
    json!({
        "22":{"recommendedPositions":["BOTTOM","UTILITY"]},
        "55":{"recommendedPositions":["MIDDLE"]},
        "64":{"recommendedPositions":["JUNGLE"]},
        "86":{"recommendedPositions":["TOP"]},
        "103":{"recommendedPositions":["MIDDLE"]},
        "117":{"recommendedPositions":["UTILITY"]},
        "136":{"recommendedPositions":["MIDDLE"]},
        "60103":{"recommendedPositions":["MIDDLE"]}
    })
}

const NAVIGATION: &str = ".menu-item[data-key=automation]";
const TAB: &str = r#"Array.from(document.querySelectorAll('.n-tabs-tab')).find(el => el.textContent.replaceAll(/\s/g, '') === '自动选择/禁用')"#;
const EDIT: &str = r#"Array.from(document.querySelectorAll('.settings-row')).find(el => el.getClientRects().length && el.querySelector('.settings-row-label-main')?.textContent.trim() === '意向英雄')?.querySelector('.settings-row-control button')"#;
const SOURCE: &str = ".n-transfer-list-item--source";
const DESTINATION: &str = ".n-transfer-list-item--target";
const SEARCH: &str = ".n-transfer-list--source input";
const FILTERS: &str = ".n-transfer-list--source .n-transfer-list-header__title > div";

async fn value(window: &WebviewWindow, expression: &str) -> Value {
    let (send, receive) = tokio::sync::oneshot::channel();
    let send = Mutex::new(Some(send));
    window
        .eval_with_callback(format!("JSON.stringify({expression})"), move |result| {
            if let Some(send) = send.lock().unwrap().take() {
                let _ = send.send(result);
            }
        })
        .expect("inspect isolated automation picker");
    let browser = tokio::time::timeout(Duration::from_secs(8), receive)
        .await
        .expect("WebView2 picker evaluation timed out")
        .expect("WebView2 picker callback");
    let json = serde_json::from_str::<String>(&browser).expect("picker returned serialized data");
    serde_json::from_str(&json).expect("picker data JSON")
}

async fn click(window: &WebviewWindow, expression: &str) {
    check_renderer(window, &format!("!!({expression})"), expression, || {}).await;
    window
        .eval(format!("({expression}).click()"))
        .expect("use normal picker control");
}

async fn rows(window: &WebviewWindow) -> Value {
    value(
        window,
        &format!(
            r#"Array.from(document.querySelectorAll({})).map(row => {{
        const img = row.querySelector('.champion-icon-container img')
        return {{ name:row.querySelector('.n-transfer-list-item__label')?.title,
                   image:img?.currentSrc?.split('/').at(-1) ?? null,
                   decoded:(img?.naturalWidth ?? 0) > 1 }}
    }})"#,
            json!(SOURCE)
        ),
    )
    .await
}

fn search(window: &WebviewWindow, query: &str) {
    window
        .eval(format!(
            r#"(() => {{
            const input = document.querySelector({})
            Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,{})
            input.dispatchEvent(new Event('input', {{ bubbles:true }}))
        }})()"#,
            json!(SEARCH),
            json!(query),
        ))
        .expect("type in the actual picker search input");
}

fn row_names(rows: &Value) -> Vec<&str> {
    let mut names: Vec<_> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    names
}

fn named_row(selector: &str, name: &str) -> String {
    format!("Array.from(document.querySelectorAll({})).find(x => x.querySelector('.n-transfer-list-item__label')?.title === {})",
        json!(selector), json!(name))
}

async fn saved_pick(app: &AppHandle, ids: &[u32]) {
    let profile = app.state::<DesktopProfile>();
    for _ in 0..30 {
        let saved = profile
            .database
            .lock()
            .unwrap()
            .setting("auto-select-main/pickConfig")
            .unwrap();
        if saved
            .as_ref()
            .is_some_and(|value| value["aram"]["champions"]["default"] == json!(ids))
        {
            assert_eq!(saved.unwrap()["aram"]["enabled"], false);
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the real picker did not save expected ARAM IDs {ids:?} in its isolated SQLite");
}

pub(super) async fn check(app: &AppHandle) {
    let server = app.state::<MiniClient>();
    let runtime = app.state::<ClientRuntime>();
    let profile = app.state::<DesktopProfile>();
    {
        let db = profile.database.lock().unwrap();
        super::bootstrap::backend_pref("self-update-main", "autoCheckUpdates")
            .unwrap()
            .write(&db, &json!(false))
            .unwrap();
        db.set_setting(
            "auto-select-main/pickConfig",
            &json!({
                "aram":{"enabled":false,"champions":{"default":[60103,55,60117]}}
            }),
        )
        .unwrap();
    }
    let client = Arc::new(LocalClient::fixture_at(server.port).expect("own fixture TLS listener"));
    let main = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("main-window.html".into()))
        .data_directory(profile.directory.join("ChampionPickerWebView2"))
        .inner_size(1160.0, 860.0)
        .decorations(false)
        .on_navigation(super::super::bundled_navigation)
        .build()
        .expect("create isolated main window");
    runtime.sample_fixture(app, &client).await;
    let catalog = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let (_, state) = runtime
                .fields("league-client-main", "gameData")
                .unwrap()
                .unwrap();
            if state["champions"]["103"]["alias"] == "Ahri" {
                break state;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("verified test client's champion catalog reached the native state");
    assert_eq!(catalog["champions"]["60103"]["alias"], "Jade_Ahri");
    click(
        &main,
        &format!("document.querySelector({})", json!(NAVIGATION)),
    )
    .await;
    click(&main, TAB).await;
    check_renderer(
        &main,
        "document.body.innerText.includes('大乱斗自动选禁')",
        "the native ARAM automation section",
        || {},
    )
    .await;
    assert_eq!(server.scene.lock().unwrap().position_reads, 0);
    click(&main, EDIT).await;
    check_renderer(
        &main,
        "document.querySelectorAll('.n-transfer-list-item--source').length >= 7",
        "at least seven LCU champion rows in the genuine transfer modal",
        || {},
    )
    .await;
    assert_eq!(
        value(
            &main,
            &format!(
                "document.querySelector({})?.childElementCount",
                json!(FILTERS)
            )
        )
        .await,
        5
    );
    let all = rows(&main).await;
    let mut lanes = Vec::new();
    let mut position_controls = Vec::new();
    for index in 0..5 {
        click(
            &main,
            &format!(
                "document.querySelector({})?.children[{index}]",
                json!(FILTERS)
            ),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(160)).await;
        lanes.push(rows(&main).await);
        position_controls.push(
            value(
                &main,
                &format!(
                    r#"(() => {{
            const control = document.querySelector({})?.children[{index}];
            return [control?.tagName, control?.title, control?.getAttribute('aria-pressed')];
        }})()"#,
                    json!(FILTERS)
                ),
            )
            .await,
        );
    }
    let names = row_names(&all);
    let lane_names: Vec<_> = lanes.iter().map(row_names).collect();
    println!("HKSLGT_CHAMPION_PICKER_ROSTER: {}", all);
    println!("HKSLGT_CHAMPION_PICKER_LANES: {lane_names:?}");
    let expected = [
        vec!["德玛西亚之力"],
        vec!["不祥之刃", "九尾妖狐", "铸星龙王"],
        vec!["盲僧"],
        vec!["寒冰射手"],
        vec!["仙灵女巫", "寒冰射手"],
    ];
    let unique = names.len() == 7 && names.windows(2).all(|w| w[0] != w[1]);
    assert!(unique && lane_names == expected,
        "picker must exclude League Classic duplicates and show five distinct recommended-position results");
    assert_eq!(server.scene.lock().unwrap().position_reads, 1);
    for (control, name) in position_controls
        .iter()
        .zip(["上路", "中路", "打野", "下路", "辅助"])
    {
        assert_eq!(*control, json!(["BUTTON", name, "true"]));
    }
    check_renderer(
        &main,
        &format!(
            "document.querySelectorAll({}).length === 1 && !!({})",
            json!(DESTINATION),
            named_row(DESTINATION, "不祥之刃")
        ),
        "only current Katarina is visible from the previously saved legacy-inclusive configuration",
        || {},
    )
    .await;
    saved_pick(app, &[60103, 55, 60117]).await;

    // Clicking the selected position again clears the filter.
    click(
        &main,
        &format!("document.querySelector({})?.children[4]", json!(FILTERS)),
    )
    .await;
    check_renderer(
        &main,
        "document.querySelectorAll('.n-transfer-list-item--source').length === 7",
        "position clear restores seven rows",
        || {},
    )
    .await;
    for (name, id) in [("九尾妖狐", 103), ("寒冰射手", 22), ("不祥之刃", 55)] {
        let selector = format!("{SOURCE} [title='{name}'] .champion-icon-container img");
        check_renderer(&main, &format!("document.querySelector({})?.currentSrc.endsWith('/{id}.png') && document.querySelector({})?.naturalWidth > 1",
            json!(selector), json!(selector)), name, || {}).await;
        main.eval("delete document.documentElement.dataset.smokeColored")
            .unwrap();
        assert_color_portrait(&main, &selector).await;
    }
    click(
        &main,
        &format!("document.querySelector({})?.children[1]", json!(FILTERS)),
    )
    .await;
    search(&main, "寒冰");
    check_renderer(
        &main,
        "document.querySelector('.n-transfer-list--source input')?.value === '寒冰' &&
         document.querySelectorAll('.n-transfer-list-item--source').length === 0",
        "search does not show a bottom-lane champion in the middle-lane filter",
        || {},
    )
    .await;
    search(&main, "九尾");
    check_renderer(&main, "document.querySelectorAll('.n-transfer-list-item--source').length === 1 &&
        document.querySelector('.n-transfer-list-item--source .n-transfer-list-item__label')?.title === '九尾妖狐'",
        "Chinese partial-name search intersects with the middle-lane filter", || {}).await;
    click(&main, &named_row(SOURCE, "九尾妖狐")).await;
    check_renderer(
        &main,
        &format!("!!({})", named_row(DESTINATION, "九尾妖狐")),
        "Ahri moves to the selected list",
        || {},
    )
    .await;
    saved_pick(app, &[55, 103]).await;

    search(&main, "");
    check_renderer(
        &main,
        "document.querySelector('.n-transfer-list--source input')?.value === '' &&
        document.querySelectorAll('.n-transfer-list-item--source').length === 3",
        "clear search restores the three middle-lane heroes without clearing selection",
        || {},
    )
    .await;
    click(
        &main,
        &format!(
            "({})?.querySelector('.n-transfer-list-item__close')",
            named_row(DESTINATION, "九尾妖狐")
        ),
    )
    .await;
    saved_pick(app, &[55]).await;
    click(
        &main,
        "document.querySelector('.n-transfer-list--target .n-transfer-list-header__button')",
    )
    .await;
    saved_pick(app, &[]).await;
    click(
        &main,
        &format!("document.querySelector({})?.children[1]", json!(FILTERS)),
    )
    .await;
    click(&main, &named_row(SOURCE, "寒冰射手")).await;
    saved_pick(app, &[22]).await;
    click(&main, "document.querySelector('.n-modal-mask')").await;
    check_renderer(
        &main,
        "!document.querySelector('.n-transfer')",
        "close the normal modal",
        || {},
    )
    .await;
    let prior_reads = {
        let mut scene = server.scene.lock().unwrap();
        scene.reject_position_once = true;
        scene.position_reads
    };
    click(&main, EDIT).await;
    check_renderer(
        &main,
        &format!("!!({})", named_row(DESTINATION, "寒冰射手")),
        "reopened picker shows the saved active ID 22",
        || {},
    )
    .await;
    let retry = r#"Array.from(document.querySelectorAll('.n-transfer-list--source button')).find(button => button.textContent.trim() === '重试分路数据')"#;
    check_renderer(
        &main,
        &format!("!!({retry})"),
        "picker exposes its real Retry button after one HTTP 503",
        || {},
    )
    .await;
    assert_eq!(server.scene.lock().unwrap().position_reads, prior_reads + 1);
    click(&main, retry).await;
    check_renderer(
        &main,
        &format!(
            "document.querySelector({})?.childElementCount === 5",
            json!(FILTERS)
        ),
        "successful retry restores the five position controls",
        || {},
    )
    .await;
    click(
        &main,
        &format!("document.querySelector({})?.children[2]", json!(FILTERS)),
    )
    .await;
    check_renderer(&main, "document.querySelectorAll('.n-transfer-list-item--source').length === 1 &&
        document.querySelector('.n-transfer-list-item--source .n-transfer-list-item__label')?.title === '盲僧'",
        "filtering the recovered recommendations returns the actual jungle hero", || {}).await;
    assert_eq!(server.scene.lock().unwrap().position_reads, prior_reads + 2);
    assert!(
        server.scene.lock().unwrap().writes.is_empty(),
        "settings only; picker must not mutate the League client"
    );
    main.destroy().expect("close the isolated picker window");
    println!("HKSLGT_CHAMPION_PICKER_PASS: all five distinct lanes, current colored portraits, Chinese search, clear and isolated SQLite persistence");
}
