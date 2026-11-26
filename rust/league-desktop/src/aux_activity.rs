use super::GameflowSession;
use serde::Serialize;
use serde_json::Value;

pub(crate) const NAMESPACE: &str = "aux-activity-main";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AuxSelectionExtras {
    pub(crate) pickable_champion_ids: Option<Vec<u32>>,
    pub(crate) subset_champion_ids: Option<Vec<u32>>,
    pub(crate) selected_skin_id: Option<u32>,
    pub(crate) selected_skin_champion_id: Option<u32>,
    pub(crate) skin_selector_available: bool,
    pub(crate) skin_selection_disabled: bool,
    pub(crate) skins: Vec<AuxSkin>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuxSkin {
    pub(crate) id: u32,
    pub(crate) name: String,
    pub(crate) is_chroma: bool,
    pub(crate) preview_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalDraftAction {
    pub(crate) id: u64,
    #[serde(rename = "type")]
    pub(crate) action_type: String,
    pub(crate) champion_id: u32,
    pub(crate) completed: bool,
    pub(crate) is_in_progress: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum AuxPhase {
    Lobby,
    Matchmaking,
    ReadyCheck,
    ChampSelect,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) enum AuxActivity {
    #[default]
    Inactive,
    Unconfirmed,
    Active(AuxView),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuxView {
    pub(crate) phase: AuxPhase,
    #[serde(skip)]
    pub(crate) game_id: Option<u64>,
    pub(crate) game_mode: &'static str,
    pub(crate) queue_name: Option<String>,
    pub(crate) map_name: Option<String>,
    pub(crate) mode_icon_path: Option<String>,
    pub(crate) party_size: Option<usize>,
    pub(crate) queue_seconds: Option<f64>,
    pub(crate) estimated_seconds: Option<f64>,
    pub(crate) penalty_seconds: Option<f64>,
    pub(crate) penalty_total_seconds: Option<f64>,
    pub(crate) ready_response: Option<&'static str>,
    pub(crate) champion_id: Option<u32>,
    pub(crate) bench: Vec<u32>,
    pub(crate) remaining_seconds: Option<u64>,
    pub(crate) bench_enabled: bool,
    pub(crate) timer_phase: Option<String>,
    pub(crate) local_player_cell_id: Option<i64>,
    pub(crate) local_actions: Vec<LocalDraftAction>,
    pub(crate) pickable_champion_ids: Option<Vec<u32>>,
    pub(crate) subset_champion_ids: Option<Vec<u32>>,
    pub(crate) allow_subset_champion_picks: bool,
    pub(crate) allow_rerolling: bool,
    pub(crate) rerolls_remaining: u32,
    pub(crate) selected_skin_id: Option<u32>,
    pub(crate) skin_selector_available: bool,
    pub(crate) skin_selection_disabled: bool,
    pub(crate) skins: Vec<AuxSkin>,
}

impl AuxActivity {
    pub(crate) fn project(
        phase: &str,
        flow: Option<&GameflowSession>,
        lobby: Option<&Value>,
        search: Option<&Value>,
        selection: Option<&Value>,
    ) -> Self {
        Self::project_with_extras(phase, flow, lobby, search, selection, None)
    }

    pub(crate) fn project_with_extras(
        phase: &str,
        flow: Option<&GameflowSession>,
        lobby: Option<&Value>,
        search: Option<&Value>,
        selection: Option<&Value>,
        extras: Option<&AuxSelectionExtras>,
    ) -> Self {
        // Phase and session can be sampled on opposite sides of a transition.
        if flow.is_some_and(|flow| !flow.phase.is_empty() && flow.phase != phase) {
            return Self::Unconfirmed;
        }
        let phase = match phase {
            "Lobby" => AuxPhase::Lobby,
            "Matchmaking" => AuxPhase::Matchmaking,
            "ReadyCheck" => AuxPhase::ReadyCheck,
            "ChampSelect" => AuxPhase::ChampSelect,
            _ => return Self::Inactive,
        };
        let config = lobby.and_then(|lobby| lobby.get("gameConfig"));
        let in_selection = phase == AuxPhase::ChampSelect;
        let select = selection.filter(|value| value.is_object());
        if in_selection
            && select.is_some_and(|select| {
                select["isSpectating"] == true || select["isCustomGame"] == true
            })
        {
            return Self::Inactive;
        }

        let lobby_queue = config.and_then(|config| positive_id(&config["queueId"]));
        let search_queue = search
            .filter(|_| matches!(phase, AuxPhase::Matchmaking | AuxPhase::ReadyCheck))
            .and_then(|search| positive_id(&search["queueId"]));
        if !compatible_ids(lobby_queue, search_queue) {
            return Self::Unconfirmed;
        }
        let selected_queue = if in_selection {
            select.and_then(|select| positive_id(&select["queueId"]))
        } else {
            lobby_queue.or(search_queue)
        };
        let flow_queue = flow.and_then(GameflowSession::queue);
        let flow_queue_id = flow_queue
            .and_then(|queue| u64::try_from(queue.id?).ok())
            .filter(|&id| id > 0);
        if lobby_queue.is_none() && !compatible_ids(search_queue, flow_queue_id) {
            return Self::Unconfirmed;
        }
        let queue = selected_queue.or(flow_queue_id);
        let config = config.filter(|_| compatible_ids(lobby_queue, queue));
        let matching_flow = flow.filter(|_| compatible_ids(flow_queue_id, queue));
        if config.is_some_and(|config| config["isCustom"] == true)
            || matching_flow
                .and_then(|flow| flow.game_data.as_ref())
                .is_some_and(|data| data.is_custom_game == Some(true))
        {
            return Self::Inactive;
        }
        let mode = config
            .and_then(|config| config["gameMode"].as_str().filter(|mode| !mode.is_empty()))
            .or_else(|| matching_flow.and_then(GameflowSession::aux_game_mode));
        let game_mode = match (mode, queue) {
            (Some("ARAM"), _) | (None, Some(450)) => "ARAM",
            (Some("KIWI"), _) | (None, Some(2400)) => "KIWI",
            (Some(_), _) | (None, Some(_)) => return Self::Inactive,
            (None, None) => return Self::Unconfirmed,
        };

        let mut view = AuxView {
            phase,
            game_id: None,
            game_mode,
            queue_name: matching_flow
                .and_then(GameflowSession::aux_queue_name)
                .and_then(safe_label),
            map_name: matching_flow
                .and_then(GameflowSession::aux_map_name)
                .and_then(safe_label),
            mode_icon_path: matching_flow.and_then(GameflowSession::aux_mode_icon_path),
            party_size: None,
            queue_seconds: None,
            estimated_seconds: None,
            penalty_seconds: None,
            penalty_total_seconds: None,
            ready_response: None,
            champion_id: None,
            bench: Vec::new(),
            remaining_seconds: None,
            bench_enabled: false,
            timer_phase: None,
            local_player_cell_id: None,
            local_actions: Vec::new(),
            pickable_champion_ids: None,
            subset_champion_ids: None,
            allow_subset_champion_picks: false,
            allow_rerolling: false,
            rerolls_remaining: 0,
            selected_skin_id: None,
            skin_selector_available: false,
            skin_selection_disabled: false,
            skins: Vec::new(),
        };
        if in_selection {
            let Some(select) = select else {
                return Self::Unconfirmed;
            };
            let flow_game = matching_flow
                .and_then(|flow| flow.game_data.as_ref())
                .and_then(|data| data.game_id)
                .filter(|&id| id > 0);
            let select_game = positive_id(&select["gameId"]);
            if !compatible_ids(flow_game, select_game) {
                return Self::Unconfirmed;
            }
            let Some(cell) = select["localPlayerCellId"]
                .as_i64()
                .filter(|&cell| cell >= 0)
            else {
                return Self::Unconfirmed;
            };
            view.local_player_cell_id = Some(cell);
            let Some(player) = select["myTeam"].as_array().and_then(|team| {
                team.iter()
                    .find(|player| player["cellId"].as_i64() == Some(cell))
            }) else {
                return Self::Unconfirmed;
            };
            view.game_id = select_game.or(flow_game);
            view.champion_id = champion_id(&player["championId"]);
            view.bench_enabled = select["benchEnabled"] == true;
            view.timer_phase = select["timer"]["phase"]
                .as_str()
                .filter(|phase| matches!(*phase, "BAN_PICK" | "FINALIZATION"))
                .map(str::to_owned);
            view.allow_subset_champion_picks = select["allowSubsetChampionPicks"] == true;
            view.allow_rerolling = select["allowRerolling"] == true;
            view.rerolls_remaining = select["rerollsRemaining"]
                .as_u64()
                .filter(|&count| count <= 99)
                .unwrap_or(0) as u32;
            view.local_actions = local_draft_actions(select, cell);
            if let Some(extras) = extras {
                view.pickable_champion_ids =
                    extras.pickable_champion_ids.as_deref().map(normalized_ids);
                view.subset_champion_ids =
                    extras.subset_champion_ids.as_deref().map(normalized_ids);
                view.selected_skin_id = extras.selected_skin_id;
                view.skin_selector_available = extras.skin_selector_available;
                view.skin_selection_disabled = extras.skin_selection_disabled;
                if view.champion_id.is_some()
                    && extras.selected_skin_champion_id == view.champion_id
                    && extras.skin_selector_available
                {
                    view.skins = extras
                        .skins
                        .iter()
                        .filter(|skin| skin.id > 0 && !skin.name.is_empty())
                        .take(100)
                        .cloned()
                        .collect();
                }
            }
            // Preserve the prior display projection when old or partial LCU
            // samples omit benchEnabled; mutations still require explicit true.
            if select["benchEnabled"] != false {
                for hero in select["benchChampions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|hero| champion_id(&hero["championId"]))
                {
                    if !view.bench.contains(&hero) {
                        view.bench.push(hero);
                        if view.bench.len() == 20 {
                            break;
                        }
                    }
                }
            }
            view.remaining_seconds = select["timer"]["adjustedTimeLeftInPhase"]
                .as_u64()
                .filter(|&milliseconds| milliseconds < 86_400_000)
                .map(|milliseconds| milliseconds.div_ceil(1000));
        } else {
            if config.is_some() {
                view.party_size = lobby
                    .and_then(|lobby| lobby["members"].as_array())
                    .map(Vec::len)
                    .filter(|size| (1..=20).contains(size));
            }
            if let Some(search) = search {
                if phase != AuxPhase::Lobby {
                    view.queue_seconds = bounded_seconds(&search["timeInQueue"]);
                    view.estimated_seconds = bounded_seconds(&search["estimatedQueueTime"]);
                }
                if phase == AuxPhase::Lobby {
                    view.penalty_seconds = search["errors"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|error| bounded_seconds(&error["penaltyTimeRemaining"]))
                        .max_by(f64::total_cmp);
                } else if phase == AuxPhase::Matchmaking {
                    view.penalty_seconds =
                        bounded_seconds(&search["lowPriorityData"]["penaltyTimeRemaining"]);
                    view.penalty_total_seconds =
                        bounded_seconds(&search["lowPriorityData"]["penaltyTime"]);
                }
                if phase == AuxPhase::ReadyCheck {
                    view.ready_response = match search["readyCheck"]["playerResponse"].as_str() {
                        Some("Accepted") => Some("Accepted"),
                        Some("Declined") => Some("Declined"),
                        Some("None") => Some("None"),
                        _ => None,
                    };
                }
            }
        }
        Self::Active(view)
    }

    pub(crate) fn view(&self) -> Option<&AuxView> {
        match self {
            Self::Active(view) => Some(view),
            _ => None,
        }
    }
}

fn normalized_ids(ids: &[u32]) -> Vec<u32> {
    let mut result = Vec::new();
    for &id in ids.iter().filter(|&&id| (1..100_000).contains(&id)) {
        if !result.contains(&id) {
            result.push(id);
        }
        if result.len() == 200 {
            break;
        }
    }
    result
}

fn safe_label(value: &str) -> Option<String> {
    (value.len() <= 96 && !value.chars().any(char::is_control)).then(|| value.to_owned())
}

fn local_draft_actions(selection: &Value, cell: i64) -> Vec<LocalDraftAction> {
    let Some(groups) = selection["actions"].as_array() else {
        return Vec::new();
    };
    groups
        .iter()
        .filter_map(Value::as_array)
        .flat_map(|group| group.iter())
        .filter(|action| action["actorCellId"].as_i64() == Some(cell))
        .filter_map(|action| {
            let id = action["id"].as_u64()?;
            let action_type = action["type"].as_str().filter(|kind| {
                !kind.is_empty() && kind.len() <= 32 && !kind.chars().any(char::is_control)
            })?;
            Some(LocalDraftAction {
                id,
                action_type: action_type.to_owned(),
                champion_id: champion_id(&action["championId"]).unwrap_or(0),
                completed: action["completed"] == true,
                is_in_progress: action["isInProgress"] == true,
            })
        })
        .take(32)
        .collect()
}

fn compatible_ids(left: Option<u64>, right: Option<u64>) -> bool {
    left.zip(right).is_none_or(|(left, right)| left == right)
}

fn positive_id(value: &Value) -> Option<u64> {
    value.as_u64().filter(|&id| id > 0)
}

fn champion_id(value: &Value) -> Option<u32> {
    let id = u32::try_from(value.as_u64()?).ok()?;
    (1..100_000).contains(&id).then_some(id)
}

fn bounded_seconds(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|value| (0.0..86_400.0).contains(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view(activity: AuxActivity) -> Value {
        let AuxActivity::Active(view) = activity else {
            panic!("expected current ARAM activity, got {activity:?}");
        };
        json!(view)
    }

    #[test]
    fn lobby_search_and_ready_states_show_actual_values_without_a_gameflow_session() {
        let lobby = json!({
            "gameConfig": { "queueId": 450, "gameMode": "ARAM", "isCustom": false },
            "members": [{"puuid":"fixture-private-player"}, {"puuid":"fixture-private-friend"}]
        });
        let search = json!({
            "queueId": 450, "timeInQueue": 27.9, "estimatedQueueTime": 54.25,
            "readyCheck": { "playerResponse": "Accepted", "timer": 10.25 },
            "lowPriorityData": {"penaltyTimeRemaining":12.5,"penaltyTime":90.0,
                                "bustedLeaverAccessToken": "fixture-only-value"},
            "errors": [{"penaltyTimeRemaining":15.0},{"penaltyTimeRemaining":30.5}],
        });
        for (name, extra) in [
            ("Lobby", None),
            ("Matchmaking", Some(&search)),
            ("ReadyCheck", Some(&search)),
        ] {
            let data = view(AuxActivity::project(name, None, Some(&lobby), extra, None));
            assert_eq!(data["phase"], name);
            assert_eq!(data["gameMode"], "ARAM");
            assert_eq!(data["partySize"], 2);
            assert!(data.get("members").is_none());
            assert!(data.get("lowPriorityData").is_none());
            if name != "Lobby" {
                assert_eq!(data["queueSeconds"], 27.9);
                assert_eq!(data["estimatedSeconds"], 54.25);
            }
            if name == "ReadyCheck" {
                assert_eq!(data["readyResponse"], "Accepted");
            }
        }
        let lobby_view = view(AuxActivity::project(
            "Lobby",
            None,
            Some(&lobby),
            Some(&search),
            None,
        ));
        assert_eq!(lobby_view["penaltySeconds"], 30.5);
        let queue_view = view(AuxActivity::project(
            "Matchmaking",
            None,
            Some(&lobby),
            Some(&search),
            None,
        ));
        assert_eq!(queue_view["penaltySeconds"], 12.5);
        assert_eq!(queue_view["penaltyTotalSeconds"], 90.0);
    }

    #[test]
    fn keeps_real_queue_map_caption_and_only_a_checked_local_map_icon() {
        let flow: GameflowSession = serde_json::from_value(json!({
            "phase": "Lobby",
            "gameData": {"queue":{"id":450,"gameMode":"ARAM","name":"Synthetic ARAM queue"}},
            "map": {"gameMode":"ARAM","name":"Synthetic map",
                     "assets":{"game-select-icon-hover":"/lol-game-data/assets/content/src/LeagueClient/GameModeAssets/ARAM/img/game-select-icon-hover.png"}}
        })).unwrap();
        let lobby = json!({"gameConfig":{"queueId":450,"gameMode":"ARAM"}});
        let data = view(AuxActivity::project(
            "Lobby",
            Some(&flow),
            Some(&lobby),
            None,
            None,
        ));
        assert_eq!(data["queueName"], "Synthetic ARAM queue");
        assert_eq!(data["mapName"], "Synthetic map");
        assert_eq!(data["modeIconPath"], "/lol-game-data/assets/content/src/LeagueClient/GameModeAssets/ARAM/img/game-select-icon-hover.png");
    }

    #[test]
    fn selection_shows_own_champion_bench_and_timer_before_the_numeric_game_id_arrives() {
        let selection = json!({
            "queueId": 2400, "gameId": 0, "localPlayerCellId": 2, "isSpectating": false,
            "myTeam": [
                { "cellId": 1, "championId": 55, "puuid": "fixture-ally" },
                { "cellId": 2, "championId": 22, "puuid": "fixture-local" }
            ],
            "benchChampions": [{ "championId": 103 }, { "championId": 103 }, { "championId": 0 }],
            "timer": { "adjustedTimeLeftInPhase": 17810 }
        });
        let data = view(AuxActivity::project(
            "ChampSelect",
            None,
            None,
            None,
            Some(&selection),
        ));
        assert_eq!(data["gameMode"], "KIWI");
        assert_eq!(data["championId"], 22);
        assert_eq!(data["bench"], json!([103]));
        assert_eq!(data["remainingSeconds"], 18);
        assert!(data.get("myTeam").is_none());
        assert!(data.get("gameId").is_none());
    }

    #[test]
    fn selection_projection_exposes_only_safe_local_action_and_allowlisted_candidates() {
        let selection = json!({
            "queueId": 450, "gameId": 101, "localPlayerCellId": 2,
            "allowSubsetChampionPicks": true, "benchEnabled": true,
            "allowRerolling": true, "rerollsRemaining": 1,
            "timer": { "phase": "BAN_PICK", "adjustedTimeLeftInPhase": 5000 },
            "myTeam": [
                { "cellId": 1, "championId": 55, "puuid": "fixture-ally" },
                { "cellId": 2, "championId": 103, "puuid": "fixture-local" }
            ],
            "actions": [
                [{ "id": 40, "actorCellId": 1, "type": "pick", "championId": 55,
                    "completed": false, "isInProgress": true }],
                [{ "id": 0, "actorCellId": 2, "type": "pick", "championId": 0,
                    "completed": false, "isInProgress": true },
                 { "id": 42, "actorCellId": 2, "type": "pick", "championId": 0,
                    "completed": false, "isInProgress": false }]
            ],
            "benchChampions": [{ "championId": 103 }, { "championId": 0 }],
            "puuid": "must-not-project", "somePrivateField": "must-not-project"
        });
        let extras = AuxSelectionExtras {
            pickable_champion_ids: Some(vec![103, 57, 57, 0]),
            subset_champion_ids: Some(vec![57, 63, 0]),
            selected_skin_id: Some(1001),
            selected_skin_champion_id: Some(103),
            skin_selection_disabled: false,
            skin_selector_available: true,
            skins: vec![AuxSkin {
                id: 1001,
                name: "Synthetic skin".into(),
                is_chroma: false,
                preview_path: Some("/lol-game-data/assets/ASSETS/Characters/Ashe/Skins/Base/Images/ashe_splash_centered_0.jpg".into()),
            }],
        };
        let data = view(AuxActivity::project_with_extras(
            "ChampSelect",
            None,
            None,
            None,
            Some(&selection),
            Some(&extras),
        ));
        assert_eq!(data["timerPhase"], "BAN_PICK");
        assert_eq!(data["localPlayerCellId"], 2);
        assert_eq!(data["allowSubsetChampionPicks"], true);
        assert_eq!(data["benchEnabled"], true);
        assert_eq!(data["allowRerolling"], true);
        assert_eq!(data["rerollsRemaining"], 1);
        assert_eq!(data["pickableChampionIds"], json!([103, 57]));
        assert_eq!(data["subsetChampionIds"], json!([57, 63]));
        assert_eq!(data["localActions"].as_array().unwrap().len(), 2);
        assert_eq!(data["localActions"][0]["id"], 0);
        assert_eq!(data["localActions"][0]["type"], "pick");
        assert_eq!(data["selectedSkinId"], 1001);
        assert_eq!(data["skins"][0]["name"], "Synthetic skin");
        assert_eq!(data["skins"][0]["previewPath"], "/lol-game-data/assets/ASSETS/Characters/Ashe/Skins/Base/Images/ashe_splash_centered_0.jpg");
        assert!(data.get("puuid").is_none());
        assert!(data.get("somePrivateField").is_none());
        assert!(data.get("actions").is_none());
        assert!(data.get("myTeam").is_none());
    }

    #[test]
    fn uses_the_client_map_as_a_mode_fallback_and_rejects_a_conflicting_game_id() {
        let flow: GameflowSession = serde_json::from_value(json!({
            "phase": "ChampSelect",
            "map": {"gameMode": "KIWI"},
            "gameData": {"gameId": 101}
        }))
        .unwrap();
        let mut selection = json!({
            "gameId": 0, "localPlayerCellId": 0,
            "myTeam": [{"cellId": 0, "championId": 103}]
        });
        let data = view(AuxActivity::project(
            "ChampSelect",
            Some(&flow),
            None,
            None,
            Some(&selection),
        ));
        assert_eq!(data["gameMode"], "KIWI");
        assert_eq!(data["championId"], 103);
        selection["gameId"] = json!(102);
        assert_eq!(
            AuxActivity::project("ChampSelect", Some(&flow), None, None, Some(&selection)),
            AuxActivity::Unconfirmed
        );
    }

    #[test]
    fn does_not_open_from_a_lobby_sample_during_a_known_phase_transition() {
        let flow: GameflowSession = serde_json::from_value(json!({
            "phase": "InProgress",
            "gameData": {"queue": {"id": 450, "gameMode": "ARAM"}}
        }))
        .unwrap();
        let lobby = json!({"gameConfig":{"queueId": 450, "gameMode": "ARAM"}});
        assert_eq!(
            AuxActivity::project("Lobby", Some(&flow), Some(&lobby), None, None),
            AuxActivity::Unconfirmed
        );
    }

    #[test]
    fn waits_when_search_reports_a_different_queue_than_the_current_lobby() {
        let lobby = json!({"gameConfig":{"queueId": 420, "gameMode": "CLASSIC"}});
        let old_search =
            json!({"queueId":450,"timeInQueue":24.0,"readyCheck":{"playerResponse":"Accepted"}});
        for phase in ["Matchmaking", "ReadyCheck"] {
            assert_eq!(
                AuxActivity::project(phase, None, Some(&lobby), Some(&old_search), None),
                AuxActivity::Unconfirmed
            );
        }
    }

    #[test]
    fn rejects_spectators_and_non_aram_without_reusing_old_players() {
        let other = json!({"gameConfig":{ "queueId": 420, "gameMode": "CLASSIC" }});
        assert_eq!(
            AuxActivity::project("Lobby", None, Some(&other), None, None),
            AuxActivity::Inactive
        );
        let spectator = json!({"queueId": 2400, "isSpectating": true, "localPlayerCellId": 0,
                               "myTeam":[{"cellId": 0, "championId": 103}]});
        assert_eq!(
            AuxActivity::project("ChampSelect", None, None, None, Some(&spectator)),
            AuxActivity::Inactive
        );
        assert_eq!(
            AuxActivity::project("ChampSelect", None, None, None, None),
            AuxActivity::Unconfirmed
        );
    }
}
