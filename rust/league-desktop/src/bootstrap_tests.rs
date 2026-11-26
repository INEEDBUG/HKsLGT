use super::*;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

struct ProfileFixture(PathBuf);

impl ProfileFixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "mgs-league-bootstrap-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn open(&self) -> LeagueProfileDatabase {
        LeagueProfileDatabase::open(&self.0).unwrap()
    }
}

impl Drop for ProfileFixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0.join("LeagueAkari.db"));
        let _ = fs::remove_dir(&self.0);
    }
}

#[test]
fn pinned_startup_states_have_a_specific_disconnected_snapshot() {
    let profile = ProfileFixture::new();
    let db = profile.open();
    // These subscriptions run unconditionally before mount in at least one local window.
    // ClientRuntime and the actual Tauri windows own their live snapshots separately.
    for (namespace, name) in [
        ("akari-api-main", "state"),
        ("auto-gameflow-main", "settings"),
        ("auto-gameflow-main", "state"),
        (AUTO_SELECT, "state"),
        (AUTO_SELECT, "settings"),
        ("auto-misc-main", "settings"),
        ("extra-assets-main", "gtimg"),
        ("extra-assets-main", "opgg"),
        ("game-client-main", "settings"),
        (CLIENT, "initialization"),
        (CLIENT, "gameData"),
        (CLIENT, "honor"),
        (CLIENT, "champSelect"),
        (CLIENT, "chat"),
        (CLIENT, "matchmaking"),
        (CLIENT, "lobby"),
        (CLIENT, "login"),
        (CLIENT, "lobbyTeamBuilder"),
        ("league-client-ux-main", "settings"),
        ("logger-factory-main", "state"),
        ("ongoing-game-main", "settings"),
        ("ongoing-game-main", "state"),
        ("renderer-debug-main", "state"),
        ("self-update-main", "settings"),
        ("self-update-main", "state"),
        ("sgp-main", "state"),
        (WINDOW_MANAGER, "state"),
        (WINDOW_MANAGER, "settings"),
        (MAIN_WINDOW, "settings"),
        (AUX_WINDOW, "settings"),
        (OPGG_WINDOW, "settings"),
    ] {
        let state = initial_state(&db, namespace, name)
            .unwrap()
            .unwrap_or_else(|| panic!("missing {namespace}/{name}"));
        assert!(!state.as_object().unwrap().is_empty(), "{namespace}/{name}");
    }
    assert_eq!(
        initial_state(&db, CLIENT, "initialization").unwrap(),
        Some(json!({"progress": null}))
    );
    let champion_select = initial_state(&db, CLIENT, "champSelect").unwrap().unwrap();
    assert!(champion_select.get("currentPickableChampionIds").is_none());
    assert_eq!(ongoing_game_data()["matchHistory"], json!({}));
    assert_eq!(initial_state(&db, "unknown", "state").unwrap(), None);
    assert_eq!(initial_state(&db, CLIENT, "entitlements").unwrap(), None);
}

#[test]
fn restores_renderer_and_companion_preferences_from_existing_keys() {
    let profile = ProfileFixture::new();
    {
        let db = profile.open();
        db.set_setting("main-window-ui-renderer/sidebarCollapsed", &json!(true))
            .unwrap();
        db.set_setting(&format!("{OPGG_WINDOW}/enabled"), &json!(false))
            .unwrap();
        db.set_setting(&format!("{RESG_NAMESPACE}/enabled"), &json!(true))
            .unwrap();
        db.set_setting(&format!("{RESG_NAMESPACE}/autoShow"), &json!(false))
            .unwrap();
        db.set_setting("self-update-main/autoCheckUpdates", &json!(false))
            .unwrap();
        let setting = renderer_pref("opgg", "opgg-renderer", "savedPreferences").unwrap();
        setting
            .write(&db, &json!({"flashPosition":"f","mode":"aram"}))
            .unwrap();
    }
    let db = profile.open();
    let sidebar = renderer_pref("main", "main-window-ui-renderer", "sidebarCollapsed").unwrap();
    assert_eq!(sidebar.read(&db).unwrap(), json!(true));
    assert!(renderer_pref("aux", "main-window-ui-renderer", "sidebarCollapsed").is_none());
    assert!(renderer_pref("main", "league-client-main", "token").is_none());
    let opgg = initial_state(&db, OPGG_WINDOW, "settings")
        .unwrap()
        .unwrap();
    assert_eq!(opgg["enabled"], false);
    let resg = resg_preferences(&db).unwrap();
    assert_eq!(resg["enabled"], true);
    assert_eq!(resg["autoShow"], false);
    assert_eq!(
        initial_state(&db, "self-update-main", "settings")
            .unwrap()
            .unwrap()["autoCheckUpdates"],
        false
    );
    assert_eq!(
        renderer_pref("opgg", "opgg-renderer", "savedPreferences")
            .unwrap()
            .read(&db)
            .unwrap()["flashPosition"],
        "f"
    );
}

#[test]
fn room_controls_reopen_legacy_sqlite_settings_and_reject_invalid_numbers() {
    use std::time::Duration;
    let profile = ProfileFixture::new();
    {
        let db = profile.open();
        let defaults = room_options(&db).unwrap();
        assert!(!defaults.accept && !defaults.search);
        assert!(!defaults.honor && !defaults.play_again);
        assert_eq!(defaults.search_delay, Duration::from_secs(5));
        for (name, value) in [
            ("autoAcceptEnabled", json!(true)),
            ("autoHonorEnabled", json!(true)),
            ("playAgainEnabled", json!(true)),
            ("autoMatchmakingEnabled", json!(true)),
            ("autoAcceptDelaySeconds", json!(1.5)),
            ("autoMatchmakingMinimumMembers", json!(99)),
            ("autoMatchmakingWaitForInvitees", json!(false)),
        ] {
            assert!(crate::room_flow::is_room_setting(name));
            backend_pref(crate::room_flow::NAMESPACE, name)
                .unwrap()
                .write(&db, &value)
                .unwrap();
        }
        assert!(
            backend_pref(crate::room_flow::NAMESPACE, "autoMatchmakingDelaySeconds")
                .unwrap()
                .write(&db, &json!(-0.1))
                .is_err()
        );
        assert!(
            backend_pref(crate::room_flow::NAMESPACE, "autoMatchmakingMinimumMembers")
                .unwrap()
                .write(&db, &json!(100))
                .is_err()
        );
    }
    let db = profile.open();
    let restored = room_options(&db).unwrap();
    assert!(restored.accept && restored.search && !restored.wait_for_invitees);
    assert!(restored.honor && restored.play_again);
    assert_eq!(restored.accept_delay, Duration::from_millis(1500));
    assert_eq!(restored.minimum_members, 99);
    assert_eq!(
        db.setting("auto-gameflow-main/autoAcceptEnabled").unwrap(),
        Some(json!(true))
    );
}

#[test]
fn aram_partial_updates_replace_champion_order_and_keep_other_groups() {
    let profile = ProfileFixture::new();
    let db = profile.open();
    let ranked = json!({"enabled":true,"champions":{"top":[201,5]}});
    db.set_setting("auto-select-main/pickConfig", &json!({"ranked":ranked}))
        .unwrap();
    set_aram_config(
        &db,
        "pickConfig",
        "aram",
        &json!({
            "champions": {"default":[1,2]}, "showIntent":true
        }),
    )
    .unwrap();
    let visible = set_aram_config(
        &db,
        "pickConfig",
        "aram",
        &json!({
            "champions": {"default":[3]}, "enabled":true
        }),
    )
    .unwrap();
    assert_eq!(
        visible,
        json!({"aram": {
            "enabled":true,
            "champions":{"default":[3],"top":[],"jungle":[],"middle":[],"bottom":[],"utility":[]},
            "ignoreIntent":false,"showIntent":true,"delaySeconds":0,"strategy":"show-and-lock-in",
            "benchHandleTradeEnabled":false,"benchSelectFirstAvailableChampion":false,
            "benchSwapAccumulatedDelaySeconds":2.9,
        }})
    );
    assert!(set_aram_config(&db, "pickConfig", "ranked", &json!({"enabled":false})).is_err());
    assert!(set_aram_config(
        &db,
        "pickConfig",
        "aram",
        &json!({"champions":{"default":["5"]}})
    )
    .is_err());
    let saved = db.setting("auto-select-main/pickConfig").unwrap().unwrap();
    assert_eq!(saved["ranked"], ranked);
    assert_eq!(saved["aram"], visible["aram"]);
}
