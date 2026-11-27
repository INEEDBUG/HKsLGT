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

#[derive(Default)]
pub(crate) struct AuxSelectionObservation<'a> {
    pub(crate) session: Option<&'a Value>,
    pub(crate) extras: Option<&'a AuxSelectionExtras>,
    pub(crate) current_champion: Option<u32>,
    pub(crate) player: Option<&'a str>,
    pub(crate) previous_game_mode: Option<&'static str>,
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
    #[serde(skip)]
    queue_id: Option<u64>,
    #[serde(skip)]
    pub(crate) selection_player: Option<String>,
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
    pub(crate) selection_actionable: bool,
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

/// Queue evidence from a confirmed room transition or draft. Heroes and
/// actions always come from the current ChampSelect sample.
pub(crate) struct ConfirmedDraftMode {
    game_id: u64,
    player: String,
    pub(crate) mode: &'static str,
}

impl ConfirmedDraftMode {
    pub(crate) fn matches_available_identity(
        &self,
        player: Option<&str>,
        game_id: Option<u64>,
    ) -> bool {
        signed_in_player(player).is_none_or(|player| player == self.player)
            && game_id.filter(|&id| id > 0).is_none_or(|id| id == self.game_id)
    }

    pub(crate) fn agrees_with_observation(
        &self,
        player: Option<&str>,
        flow: Option<&GameflowSession>,
        selection: Option<&Value>,
    ) -> bool {
        let flow_queue = flow.and_then(GameflowSession::queue);
        let selection_queue = selection.and_then(|draft| positive_id(&draft["queueId"]));
        let flow_queue_id = flow_queue.and_then(|queue| u64::try_from(queue.id?).ok());
        let roster_player = selection
            .and_then(|draft| draft_player(draft, signed_in_player(player)))
            .and_then(|row| row["puuid"].as_str());
        self.matches_available_identity(player, flow.and_then(game_id))
            && self.matches_available_identity(
                roster_player, selection.and_then(|draft| positive_id(&draft["gameId"])),
            )
            && [selection_queue, flow_queue_id].into_iter().flatten().all(|queue| {
                matches!((queue, self.mode), (0, _) | (450, "ARAM") | (2400, "KIWI"))
            })
            && flow_queue.and_then(|queue| queue.game_mode.as_deref())
                .is_none_or(|mode| matches!(mode, "" | "ARAM" | "KIWI"))
    }

    pub(crate) fn mode_for_same_game(
        &self,
        player: Option<&str>,
        select: &Value,
    ) -> Option<&'static str> {
        (positive_id(&select["gameId"]) == Some(self.game_id)
            && player == Some(self.player.as_str()))
            .then_some(self.mode)
    }
}

impl AuxActivity {
    #[cfg(test)]
    pub(crate) fn project(
        phase: &str,
        flow: Option<&GameflowSession>,
        lobby: Option<&Value>,
        search: Option<&Value>,
        selection: Option<&Value>,
    ) -> Self {
        Self::project_with_extras(phase, flow, lobby, search, selection, None)
    }

    #[cfg(test)]
    pub(crate) fn project_with_extras(
        phase: &str,
        flow: Option<&GameflowSession>,
        lobby: Option<&Value>,
        search: Option<&Value>,
        selection: Option<&Value>,
        extras: Option<&AuxSelectionExtras>,
    ) -> Self {
        Self::project_observed(
            phase,
            flow,
            lobby,
            search,
            AuxSelectionObservation {
                session: selection,
                extras,
                ..Default::default()
            },
        )
    }

    pub(crate) fn project_observed(
        phase: &str,
        flow: Option<&GameflowSession>,
        lobby: Option<&Value>,
        search: Option<&Value>,
        observed: AuxSelectionObservation<'_>,
    ) -> Self {
        let direct_selection = phase == "ChampSelect";
        let signed_in = signed_in_player(observed.player);
        let select = observed.session.filter(|value| value.is_object());
        let select_game = select.and_then(|select| positive_id(&select["gameId"]));
        let draft_queue = select.and_then(|select| positive_id(&select["queueId"]));
        let config = lobby.and_then(|lobby| lobby.get("gameConfig"));
        let same_pick = || observed.current_champion.and_then(normalize_champion).is_some_and(|id| {
            select.and_then(|select| draft_player(select, signed_in))
                .and_then(|row| champion_id(&row["championId"])) == Some(id)
        });
        // These three LCU endpoints advance independently after a ready check.
        // Only promote a late phase using this sample's active timer and matching game.
        let promoted = matches!(phase, "" | "Matchmaking" | "ReadyCheck")
            && select.is_some_and(live_draft)
            && flow.is_some_and(|flow| {
                let flow_game = game_id(flow);
                let same_game = select_game.is_some() && select_game == flow_game;
                let same_aram_queue = matches!(draft_queue, Some(450 | 2400))
                    && flow.queue().and_then(|queue| u64::try_from(queue.id?).ok()) == draft_queue;
                let confirms_live_pick = same_pick()
                    && (phase.is_empty() || (signed_in.is_some() && same_aram_queue));
                compatible_ids(select_game, flow_game)
                    && ((flow.phase == "ChampSelect" && (same_game || confirms_live_pick))
                        || (flow.phase == "ReadyCheck" && same_game))
            });
        if promoted && config.is_some_and(|config| {
            config["isCustom"] == true
                || config["gameMode"].as_str().is_some_and(|mode| !matches!(mode, "" | "ARAM" | "KIWI"))
        }) {
            return Self::Unconfirmed;
        }
        let phase = if promoted { "ChampSelect" } else { phase };
        let mismatched_phase = flow.filter(|flow| !flow.phase.is_empty() && flow.phase != phase);
        let phase = match phase {
            "Lobby" => AuxPhase::Lobby,
            "Matchmaking" => AuxPhase::Matchmaking,
            "ReadyCheck" => AuxPhase::ReadyCheck,
            "ChampSelect" => AuxPhase::ChampSelect,
            "" if flow.is_some_and(|flow| matches!(flow.phase.as_str(), "ChampSelect" | "ReadyCheck")) => return Self::Unconfirmed,
            _ => return Self::Inactive,
        };
        let in_selection = phase == AuxPhase::ChampSelect;
        let config = config.filter(|_| !in_selection);
        let flow = match mismatched_phase {
            Some(old)
                if in_selection
                    && matches!(old.phase.as_str(), "None" | "Lobby" | "Matchmaking" | "ReadyCheck") =>
            {
                let same_game = select_game.is_some() && game_id(old) == select_game;
                if same_game
                    && old.game_data.as_ref().is_some_and(|data| data.is_custom_game == Some(true))
                {
                    return Self::Inactive;
                }
                if same_game && old.aux_game_mode().is_some_and(|mode| !matches!(mode, "ARAM" | "KIWI"))
                {
                    return Self::Unconfirmed;
                }
                // An older phase may fill in a missing queue for the same game.
                // If ChampSelect already reports its queue, use that observation.
                (same_game && draft_queue.is_none()).then_some(old)
            }
            Some(old) if direct_selection && matches!(draft_queue, Some(450 | 2400))
                && matches!(old.phase.as_str(), "InProgress" | "WaitingForStats" | "PreEndOfGame" | "EndOfGame")
                && game_id(old).zip(select_game).is_some_and(|(old, current)| old != current)
                && signed_in.is_some() && select.is_some_and(live_draft) && same_pick() => None,
            Some(_) => return Self::Unconfirmed,
            None => flow,
        };
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
            draft_queue
        } else {
            lobby_queue.or(search_queue)
        };
        let flow_queue = flow.and_then(GameflowSession::queue);
        let flow_queue_id = flow_queue
            .and_then(|queue| u64::try_from(queue.id?).ok())
            .filter(|&id| id > 0);
        if in_selection && !compatible_ids(selected_queue, flow_queue_id) {
            return Self::Unconfirmed;
        }
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
        let lobby_mode = config
            .and_then(|config| config["gameMode"].as_str().filter(|mode| !mode.is_empty()));
        let queue_mode = match queue {
            Some(450) => Some("ARAM"),
            Some(2400) => Some("KIWI"),
            _ => None,
        };
        let declared_mode = lobby_mode.or_else(|| matching_flow.and_then(GameflowSession::queue)
            .and_then(|q| q.game_mode.as_deref()).filter(|mode| !mode.is_empty()));
        // A map can be reused by several queues. It is weaker than the actual queue.
        let mode = if in_selection && queue.is_some() {
            declared_mode.or(queue_mode)
        } else {
            declared_mode.or_else(|| matching_flow.and_then(GameflowSession::aux_game_mode))
        };
        let related_queue_label = in_selection && queue == Some(2400) && mode == Some("ARAM");
        if !related_queue_label && mode
            .zip(queue_mode)
            .is_some_and(|(mode, queue_mode)| mode != queue_mode)
        {
            return Self::Unconfirmed;
        }
        let same_game_mode = observed.previous_game_mode.filter(|_| {
            direct_selection && signed_in.is_some() && select_game.is_some() && queue.is_none()
                && select.is_some_and(live_draft)
        });
        let game_mode = match queue_mode.or(mode).or(same_game_mode) {
            Some("ARAM") => "ARAM",
            Some("KIWI") => "KIWI",
            Some(_) => return Self::Inactive,
            None if queue.is_some() => return Self::Inactive,
            None => return Self::Unconfirmed,
        };

        let mut view = AuxView {
            phase,
            game_id: matching_flow.filter(|_| phase == AuxPhase::ReadyCheck).and_then(game_id),
            queue_id: queue,
            selection_player: signed_in.filter(|_| phase == AuxPhase::ReadyCheck).map(str::to_owned),
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
            selection_actionable: false,
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
            let flow_game = matching_flow.and_then(game_id);
            if !compatible_ids(flow_game, select_game) {
                return Self::Unconfirmed;
            }
            view.game_id = select_game.or(flow_game);
            view.selection_player = signed_in.map(str::to_owned);
            let select = select.unwrap_or(&Value::Null);
            let cell = select["localPlayerCellId"]
                .as_i64()
                .filter(|&cell| cell >= 0);
            let player = draft_player(select, signed_in);
            let row_player = signed_in_player(player.and_then(|player| player["puuid"].as_str()));
            if row_player
                .zip(signed_in)
                .is_some_and(|(roster, current)| roster != current)
            {
                return Self::Unconfirmed;
            }
            let roster_cell = player.and_then(|row| row["cellId"].as_i64()).filter(|&id| id >= 0);
            view.local_player_cell_id = roster_cell;
            let roster_pick = player.and_then(|player| champion_id(&player["championId"]));
            view.champion_id = observed.current_champion.and_then(normalize_champion).or(roster_pick);
            // Use only the synchronized full session for write eligibility.
            view.selection_actionable = direct_selection && !related_queue_label
                && (draft_queue.is_some() || flow_queue_id.is_some() || matching_flow.and_then(GameflowSession::aux_game_mode).is_some())
                && matching_flow.is_some_and(|flow| flow.phase == "ChampSelect")
                && signed_in.is_some()
                && cell.is_some() && cell == roster_cell
                && observed.current_champion.is_none_or(|id| normalize_champion(id) == roster_pick);
            view.timer_phase = select["timer"]["phase"]
                .as_str()
                .filter(|phase| matches!(*phase, "PLANNING" | "BAN_PICK" | "FINALIZATION" | "GAME_STARTING"))
                .map(str::to_owned);
            view.allow_subset_champion_picks = select["allowSubsetChampionPicks"] == true;
            view.allow_rerolling = select["allowRerolling"] == true;
            view.rerolls_remaining = select["rerollsRemaining"]
                .as_u64()
                .filter(|&count| count <= 99)
                .unwrap_or(0) as u32;
            if let Some(cell) = roster_cell {
                view.local_actions = local_draft_actions(select, cell);
            }
            if let Some(extras) = observed.extras {
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
            view.bench_enabled = select["benchEnabled"]
                .as_bool()
                .unwrap_or(!view.bench.is_empty());
            view.remaining_seconds = select["timer"]["adjustedTimeLeftInPhase"]
                .as_u64()
                .filter(|&milliseconds| select["timer"]["isInfinite"] != true && milliseconds < 86_400_000)
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

impl AuxView {
    pub(crate) fn confirmed_mode(&self) -> Option<ConfirmedDraftMode> {
        if !matches!(self.phase, AuxPhase::ReadyCheck | AuxPhase::ChampSelect) {
            return None;
        }
        Some(ConfirmedDraftMode {
            game_id: self.game_id?,
            player: self.selection_player.clone()?,
            mode: self.game_mode,
        })
    }

    pub(crate) fn confirms_selection(&self, original: &Self) -> bool {
        self.selection_actionable
            && original.selection_actionable
            && self.game_mode == original.game_mode
            && self.selection_player == original.selection_player
            && original.queue_id.is_none_or(|id| self.queue_id == Some(id))
            && original.game_id.is_none_or(|id| self.game_id == Some(id))
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

fn signed_in_player(player: Option<&str>) -> Option<&str> {
    player.filter(|player| !player.is_empty() && *player != "00000000-0000-0000-0000-000000000000")
}

fn compatible_ids(left: Option<u64>, right: Option<u64>) -> bool {
    left.zip(right).is_none_or(|(left, right)| left == right)
}

fn live_draft(select: &Value) -> bool {
    let timer = &select["timer"];
    matches!(timer["phase"].as_str(), Some("PLANNING" | "BAN_PICK" | "FINALIZATION" | "GAME_STARTING"))
        && timer["adjustedTimeLeftInPhase"].as_u64().is_some_and(|ms| ms < 86_400_000)
        && select["myTeam"].as_array().is_some_and(|team| !team.is_empty())
}

fn draft_player<'a>(select: &'a Value, signed_in: Option<&str>) -> Option<&'a Value> {
    let team = select["myTeam"].as_array()?;
    let cell = select["localPlayerCellId"].as_i64().filter(|&cell| cell >= 0);
    if let Some(row) = cell.and_then(|cell| team.iter().find(|row| row["cellId"].as_i64() == Some(cell))) {
        return Some(row);
    }
    let puuid = signed_in?;
    let mut own = team.iter().filter(|row| row["puuid"].as_str() == Some(puuid));
    let first = own.next()?;
    own.next().is_none().then_some(first)
}

fn game_id(flow: &GameflowSession) -> Option<u64> {
    flow.game_data.as_ref()?.game_id.filter(|&id| id > 0)
}

fn positive_id(value: &Value) -> Option<u64> {
    value.as_u64().filter(|&id| id > 0)
}

fn champion_id(value: &Value) -> Option<u32> {
    normalize_champion(u32::try_from(value.as_u64()?).ok()?)
}

fn normalize_champion(id: u32) -> Option<u32> {
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
        // The canonical map-12 entries for both queues use the same Riot asset.
        let badge = "lol-game-data/assets/content/src/LeagueClient/GameModeAssets/ARAM/img/game-select-icon-hover.png";
        for (id, mode, name) in [(450, "ARAM", "极地大乱斗"), (2400, "KIWI", "海克斯大乱斗")] {
            let flow: GameflowSession = serde_json::from_value(json!({
                "phase": "Lobby",
                "gameData": {"queue":{"id":id,"gameMode":mode,"name":name}},
                "map": {"gameMode":mode,"name":"嚎哭深渊",
                        "assets":{"game-select-icon-hover":badge}}
            }))
            .unwrap();
            let lobby = json!({"gameConfig":{"queueId":id,"gameMode":mode}});
            let data = view(AuxActivity::project(
                "Lobby",
                Some(&flow),
                Some(&lobby),
                None,
                None,
            ));
            assert_eq!(data["queueName"], name);
            assert_eq!(data["mapName"], "嚎哭深渊");
            assert_eq!(data["modeIconPath"], format!("/{badge}"));
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
    fn live_kiwi_selection_is_visible_while_gameflow_session_is_still_in_the_previous_phase() {
        let old_flow: GameflowSession = serde_json::from_value(json!({
            "phase": "ReadyCheck",
            "gameData": {"gameId":100,"queue":{"id":450,"gameMode":"ARAM"}}
        }))
        .unwrap();
        let mut selection = json!({
            "queueId": 2400, "gameId": 0, "isSpectating": false, "isCustomGame": false,
            "localPlayerCellId": 2,
            "myTeam": [{"cellId":2,"championId":103}],
            "benchEnabled": true, "benchChampions": [{"championId":22}],
            "timer": {"phase":"FINALIZATION","adjustedTimeLeftInPhase":20110}
        });
        for game_id in [0, 100] {
            selection["gameId"] = json!(game_id);
            let data = view(AuxActivity::project(
                "ChampSelect",
                Some(&old_flow),
                None,
                None,
                Some(&selection),
            ));
            assert_eq!(data["gameMode"], "KIWI");
            assert_eq!(data["championId"], 103);
            assert_eq!(data["bench"], json!([22]));
            assert_eq!(data["remainingSeconds"], 21);
            assert_eq!(data["selectionActionable"], false);
        }
    }

    #[test]
    fn confirmed_kiwi_draft_does_not_hide_the_panel_when_own_roster_or_selection_is_missing() {
        let flow: GameflowSession = serde_json::from_value(json!({
            "phase": "ChampSelect", "map": {"gameMode":"KIWI"},
            "gameData": {"gameId":9280,"queue":{"id":2400}}
        }))
        .unwrap();
        let selection = json!({
            "localPlayerCellId":2, "myTeam": [{"cellId":1,"championId":55}],
            "benchChampions":[{"championId":22}],
            "timer":{"phase":"FINALIZATION","adjustedTimeLeftInPhase":20110}
        });
        for (sample, bench) in [(Some(&selection), json!([22])), (None, json!([]))] {
            let data = view(AuxActivity::project("ChampSelect", Some(&flow), None, None, sample));
            assert_eq!(data["phase"], "ChampSelect");
            assert_eq!(data["gameMode"], "KIWI");
            assert_eq!(data["championId"], Value::Null);
            assert_eq!(data["bench"], bench);
            assert_eq!(data["selectionActionable"], false);
        }
    }

    #[test]
    fn lagging_gameflow_supplies_the_mode_only_if_the_selection_confirms_its_game_id() {
        let flow: GameflowSession = serde_json::from_value(json!({
            "phase":"ReadyCheck","gameData":{"gameId":7812,"queue":{"id":2400,"gameMode":"KIWI"}}
        })).unwrap();
        let mut selection = json!({"gameId":7812,"localPlayerCellId":2,
            "myTeam":[{"cellId":2,"championId":103}]});
        let data = view(AuxActivity::project("ChampSelect", Some(&flow), None, None, Some(&selection)));
        assert_eq!(data["gameMode"], "KIWI");
        assert_eq!(data["championId"], 103);
        assert_eq!(data["selectionActionable"], false);
        selection["gameId"] = json!(0);
        assert!(AuxActivity::project("ChampSelect", Some(&flow), None, None, Some(&selection)).view().is_none());
    }

    #[test]
    fn direct_current_champion_is_visible_without_a_roster_but_cannot_authorize_writes() {
        let flow: GameflowSession = serde_json::from_value(json!({
            "phase":"ChampSelect", "gameData":{"gameId":88,"isCustomGame":false,"queue":{"id":2400}}
        })).unwrap();
        let mut selection = json!({"gameId":88,"queueId":2400,"localPlayerCellId":2,
            "benchChampions":[{"championId":22}],"myTeam":[{"cellId":1,"championId":55}]});
        let observe = |selection: &Value| {
            AuxActivity::project_observed("ChampSelect", Some(&flow), None, None,
                AuxSelectionObservation { session: Some(selection), current_champion: Some(103),
                    player: Some("local"), ..Default::default() })
        };
        let waiting = view(observe(&selection));
        assert_eq!(waiting["championId"], 103);
        assert_eq!(waiting["bench"], json!([22]));
        assert_eq!(waiting["selectionActionable"], false);
        selection["myTeam"] = json!([{"cellId":2,"puuid":"someone-else","championId":103}]);
        assert!(observe(&selection).view().is_none());
        selection["myTeam"] = json!([{"cellId":2,"puuid":"local","championId":103}]);
        let verified = observe(&selection);
        let verified = verified.view().unwrap();
        assert!(verified.selection_actionable);
        selection["gameId"] = json!(89);
        assert!(observe(&selection).view().is_none());
        selection["gameId"] = json!(88);
        selection["queueId"] = json!(420);
        assert!(observe(&selection).view().is_none());
        assert_eq!(AuxActivity::project("InProgress", Some(&flow), None, None, Some(&selection)),
            AuxActivity::Inactive);
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

    #[test]
    fn fresh_same_game_kiwi_draft_overtakes_a_late_phase_without_authorizing_writes() {
        let flow: GameflowSession = serde_json::from_value(json!({
            "phase":"ChampSelect","gameData":{"gameId":901,"queue":{"id":2400,"gameMode":"KIWI"}}
        })).unwrap();
        let mut select = json!({
            "gameId":901,"queueId":2400,"localPlayerCellId":0,
            "myTeam":[{"cellId":1,"puuid":"ally","championId":55},
                      {"cellId":0,"puuid":"owner","championId":157}],
            "benchChampions":[{"championId":22}],
            "timer":{"phase":"FINALIZATION","adjustedTimeLeftInPhase":15710}
        });
        let observe = |phase, select: &Value| AuxActivity::project_observed(
            phase, Some(&flow), None, None,
            AuxSelectionObservation{session:Some(select),player:Some("owner"),
                current_champion:Some(157), ..Default::default()}
        );
        for phase in ["ReadyCheck", "Matchmaking", ""] {
            let data = view(observe(phase, &select));
            assert_eq!(data["phase"], "ChampSelect");
            assert_eq!(data["championId"], 157);
            assert_eq!(data["bench"], json!([22]));
            assert_eq!(data["remainingSeconds"], 16);
            assert_eq!(data["selectionActionable"], false);
        }
        for phase in ["Lobby", "InProgress", "PreEndOfGame", "None"] {
            assert!(observe(phase, &select).view().is_none());
        }
        select["gameId"] = json!(902);
        assert!(observe("ReadyCheck",&select).view().is_none());
    }

    #[test]
    fn late_ready_check_shows_the_live_kiwi_draft_before_both_game_ids_arrive() {
        let flow: GameflowSession = serde_json::from_value(json!({
            "phase":"ChampSelect","gameData":{"gameId":0,"queue":{"id":2400,"gameMode":"KIWI"}}
        })).unwrap();
        let mut selection = json!({
            "gameId":913,"queueId":2400,"localPlayerCellId":0,
            "myTeam":[{"cellId":0,"puuid":"owner","championId":77}],
            "benchEnabled":true,"benchChampions":[{"championId":268}],
            "timer":{"phase":"FINALIZATION","adjustedTimeLeftInPhase":1950}
        });
        let observe = |selection: &Value, champion, player| AuxActivity::project_observed(
            "ReadyCheck", Some(&flow), None, None,
            AuxSelectionObservation {
                session: Some(selection), current_champion: Some(champion), player,
                ..Default::default()
            },
        );
        let data = view(observe(&selection, 77, Some("owner")));
        assert_eq!(data["championId"], 77);
        assert_eq!(data["bench"], json!([268]));
        assert_eq!(data["remainingSeconds"], 2);
        assert_eq!(data["selectionActionable"], false);
        assert_eq!(observe(&selection, 55, Some("owner")), AuxActivity::Unconfirmed);
        assert_eq!(observe(&selection, 77, Some("different-player")), AuxActivity::Unconfirmed);
        selection["queueId"] = json!(420);
        assert_eq!(observe(&selection, 77, Some("owner")), AuxActivity::Unconfirmed);
    }

    #[test]
    fn draft_roster_supplies_own_pick_when_current_champion_or_local_cell_is_not_ready() {
        let flow: GameflowSession = serde_json::from_value(json!({
            "phase":"ChampSelect","gameData":{"gameId":901,"queue":{"id":2400,"gameMode":"KIWI"}}
        })).unwrap();
        let mut select = json!({"gameId":901,"localPlayerCellId":0,
            "myTeam":[{"cellId":1,"puuid":"ally","championId":55},
                      {"cellId":0,"puuid":"owner","championId":157}]});
        let observe = |select: &Value, player| AuxActivity::project_observed("ChampSelect",
            Some(&flow), None,None, AuxSelectionObservation{session:Some(select),
                player, current_champion:Some(0), ..Default::default()});
        for cell in [json!(0), Value::Null] {
            select["localPlayerCellId"] = cell;
            let data = view(observe(&select, Some("owner")));
            assert_eq!(data["championId"], 157);
            assert_eq!(data["selectionActionable"], false);
        }
        assert!(view(observe(&select, Some("other")))["championId"].is_null());
    }

    #[test]
    fn new_verified_draft_does_not_inherit_last_games_terminal_flow_or_roster() {
        let mut old = json!({"phase":"EndOfGame","gameData":{"gameId":900,"queue":{"id":420,"gameMode":"CLASSIC"}}});
        let select = json!({"gameId":901,"queueId":2400,"localPlayerCellId":0,
            "myTeam":[{"cellId":0,"puuid":"owner","championId":157}],
            "timer":{"phase":"FINALIZATION","adjustedTimeLeftInPhase":16000}});
        let sample = |flow: &Value, current| {
            let flow: GameflowSession = serde_json::from_value(flow.clone()).unwrap();
            AuxActivity::project_observed("ChampSelect", Some(&flow), None, None,
                AuxSelectionObservation{session:Some(&select), player:Some("owner"),
                    current_champion:current, ..Default::default()})
        };
        let shown = view(sample(&old, Some(157)));
        assert_eq!(shown["championId"], 157);
        assert_eq!(shown["selectionActionable"], false);
        assert!(sample(&old, Some(55)).view().is_none());
        old["gameData"]["gameId"] = json!(901);
        assert!(sample(&old, Some(157)).view().is_none());
    }

    #[test]
    fn a_queue_gap_uses_only_the_same_account_and_numbered_draft_without_copying_its_champions() {
        let ready: GameflowSession = serde_json::from_value(json!({"phase":"ReadyCheck",
            "gameData":{"gameId":901,"queue":{"id":2400,"gameMode":"KIWI"}}})).unwrap();
        let previous = AuxActivity::project_observed("ReadyCheck", Some(&ready), None, None,
            AuxSelectionObservation {player:Some("owner"), ..Default::default()});
        let previous = previous.view().unwrap().confirmed_mode().unwrap();
        let mut select = json!({"gameId":901,"localPlayerCellId":0,
            "myTeam":[{"cellId":0,"puuid":"owner","championId":157}],
            "benchChampions":[{"championId":22}],"timer":{"phase":"FINALIZATION","adjustedTimeLeftInPhase":16000}});
        let sample = |select: &Value, player| AuxActivity::project_observed("ChampSelect",
            None, None, None, AuxSelectionObservation {session:Some(select), player,
                previous_game_mode:previous.mode_for_same_game(player, select), ..Default::default()});
        let data = view(sample(&select, Some("owner")));
        assert_eq!(data["gameMode"], "KIWI");
        assert_eq!(data["remainingSeconds"], 16);
        assert_eq!(data["championId"], 157);
        assert_eq!(data["bench"], json!([22]));
        assert_eq!(data["selectionActionable"], false);
        assert!(sample(&select, Some("other")).view().is_none());
        assert!(sample(&select, None).view().is_none());
        select["queueId"] = json!(420);
        assert!(sample(&select, Some("owner")).view().is_none());
        select["queueId"] = Value::Null;
        for id in [0, 902] {
            select["gameId"] = json!(id);
            assert!(sample(&select, Some("owner")).view().is_none());
        }
    }
}
