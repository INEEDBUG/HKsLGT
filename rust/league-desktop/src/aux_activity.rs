use super::GameflowSession;
use serde::Serialize;
use serde_json::Value;

pub(crate) const NAMESPACE: &str = "aux-activity-main";

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

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuxView {
    pub(crate) phase: AuxPhase,
    #[serde(skip)]
    pub(crate) game_id: Option<u64>,
    game_mode: &'static str,
    party_size: Option<usize>,
    queue_seconds: Option<u64>,
    estimated_seconds: Option<u64>,
    ready_response: Option<&'static str>,
    champion_id: Option<u32>,
    bench: Vec<u32>,
    remaining_seconds: Option<u64>,
}

impl AuxActivity {
    pub(crate) fn project(
        phase: &str,
        flow: Option<&GameflowSession>,
        lobby: Option<&Value>,
        search: Option<&Value>,
        selection: Option<&Value>,
    ) -> Self {
        let flow = flow.filter(|flow| flow.phase == phase);
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
            party_size: None,
            queue_seconds: None,
            estimated_seconds: None,
            ready_response: None,
            champion_id: None,
            bench: Vec::new(),
            remaining_seconds: None,
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
            let Some(player) = select["myTeam"].as_array().and_then(|team| {
                team.iter()
                    .find(|player| player["cellId"].as_i64() == Some(cell))
            }) else {
                return Self::Unconfirmed;
            };
            view.game_id = select_game.or(flow_game);
            view.champion_id = champion_id(&player["championId"]);
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
            if let Some(search) = search.filter(|_| phase != AuxPhase::Lobby) {
                view.queue_seconds = seconds(&search["timeInQueue"]);
                view.estimated_seconds = seconds(&search["estimatedQueueTime"]);
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

fn seconds(value: &Value) -> Option<u64> {
    value
        .as_f64()
        .filter(|value| (0.0..86_400.0).contains(value))
        .map(|value| value as u64)
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
            "lowPriorityData": { "bustedLeaverAccessToken": "fixture-only-value" }
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
                assert_eq!(data["queueSeconds"], 27);
                assert_eq!(data["estimatedSeconds"], 54);
            }
            if name == "ReadyCheck" {
                assert_eq!(data["readyResponse"], "Accepted");
            }
        }
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
