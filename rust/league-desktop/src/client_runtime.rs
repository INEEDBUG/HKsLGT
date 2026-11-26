//! Disconnect-safe snapshots from the local League client for the existing renderer.

use crate::{
    aram_automation::{AramOptions, AramSchedule, ScheduledStep, SelectionScope},
    client_discovery::{discover_clients, ClientInstance, LocalClient},
    desktop::{
        aux_window::{AuxObservation, AuxStage, AuxWindow},
        emit,
    },
    lcu::{
        aux_activity::{self, AuxActivity, AuxPhase, AuxSelectionExtras, AuxView},
        game_art::{GameArt, GameArtPath},
        player_search::PlayerSearchShard,
        player_tab_read::PlayerTabRead,
        AuxChampSelectAction, GameflowSession, LcuError, LcuTarget, LcuTransport, RoomAction,
    },
    opgg_window::{self, OpggTarget, OpggWindow},
    resg_window::{ResgAction, ResgWindow},
    room_flow::{self, RoomFlow, RoomOptions, RoomSample},
};
use futures_util::future::join3;
pub(crate) use maxgamestudio_league_core::shell_preferences::LEAGUE_CLIENT as CLIENT;
use serde_json::{json, Map, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager};

const CLIENT_DISCOVERY: &str = "league-client-ux-main";
const SGP: &str = "sgp-main";

#[derive(Default, PartialEq)]
struct Snapshot {
    known_clients: Vec<ClientInstance>,
    active: Option<ActiveClient>,
}

#[derive(PartialEq)]
struct ActiveClient {
    instance: ClientInstance,
    phase: String,
    session: Value,
    summoner: Value,
    companions: CompanionTargets,
    aux_activity: AuxActivity,
    room: Option<RoomSample>,
    room_unconfirmed: bool,
    shard: Option<PlayerSearchShard>,
}

#[derive(Default, PartialEq)]
struct CompanionTargets {
    opgg: Option<OpggTarget>,
    resg: Option<ResgTarget>,
}

#[derive(Clone, Copy, Default)]
struct CompanionDemand {
    aux: bool,
    opgg: bool,
    resg: bool,
}

impl CompanionDemand {
    fn from_app(app: &AppHandle) -> Self {
        Self {
            aux: app.state::<AuxWindow>().enabled(),
            opgg: app.state::<OpggWindow>().enabled(),
            resg: app.state::<ResgWindow>().enabled(),
        }
    }

    fn reads_selection(self, phase: &str, game: Option<&GameflowSession>, automate: bool) -> bool {
        phase == "ChampSelect"
            && ((self.aux
                && game
                    .and_then(GameflowSession::aux_game_mode)
                    .is_none_or(|mode| matches!(mode, "ARAM" | "KIWI")))
                || game.is_some_and(|game| {
                    ((automate || self.opgg) && game.is_aram())
                        || (self.resg && game.is_hextech_aram())
                }))
    }

    fn aux_activity(
        self,
        phase: &str,
        game: Option<&GameflowSession>,
        lobby: Option<&Value>,
        search: Option<&Value>,
        selection: Option<&Value>,
    ) -> AuxActivity {
        if self.aux {
            AuxActivity::project(phase, game, lobby, search, selection)
        } else {
            AuxActivity::Inactive
        }
    }

    fn targets(
        self,
        instance: ClientInstance,
        game: Option<&GameflowSession>,
        selection: Option<&Value>,
        puuid: Option<&str>,
        previous_chart: Option<ResgTarget>,
    ) -> CompanionTargets {
        let Some(game) = game else {
            return CompanionTargets::default();
        };
        let opgg = selection
            .filter(|_| self.opgg && puuid.is_some())
            .and_then(|select| OpggTarget::from_sample(instance, game, select));
        let mut resg = None;
        if self.resg && game.is_hextech_aram() {
            let game_id = game
                .game_data
                .as_ref()
                .and_then(|data| data.game_id)
                .filter(|&id| id != 0);
            let chart_selection = if game.phase == "ChampSelect" {
                selection
            } else {
                Some(&Value::Null)
            };
            resg = if let Some(selection) = chart_selection {
                game.resg_champion(selection, puuid)
                    .map(|champion_id| ResgTarget {
                        instance,
                        game_id,
                        champion_id,
                    })
            } else {
                // Keep the current chart during an optional read timeout for the same verified game.
                previous_chart.filter(|previous| {
                    game_id.is_some()
                        && previous.game_id == game_id
                        && previous.instance == instance
                })
            };
        }
        CompanionTargets { opgg, resg }
    }
}

struct Sample {
    client: ActiveClient,
    selection: Option<(GameflowSession, Value)>,
}

struct FreshAuxSelection {
    client: Arc<LocalClient>,
    generation: u64,
    original: AuxView,
    current: AuxView,
    flow: GameflowSession,
    selection: Value,
    extras: AuxSelectionExtras,
}

#[derive(Default)]
struct RoomReads {
    lobby: Option<Value>,
    search: Option<Value>,
    search_read: bool,
}

impl RoomReads {
    fn sample(
        &self,
        client: ClientInstance,
        player: Option<&str>,
        activity: &AuxActivity,
    ) -> Option<RoomSample> {
        let phase = activity.view()?.phase;
        if phase == AuxPhase::ChampSelect {
            return None;
        }
        Some(RoomSample::new(
            client,
            phase,
            player.filter(|p| !p.is_empty())?,
            self.lobby.as_ref(),
            self.search.as_ref(),
            self.search_read,
        ))
    }
}

fn epoch_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

async fn optional_lcu<T>(
    read: impl std::future::Future<Output = Result<T, LcuError>>,
) -> Option<T> {
    // The optional reads must leave time to publish connected state and phase.
    tokio::time::timeout(Duration::from_millis(750), read)
        .await
        .ok()
        .and_then(Result::ok)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResgTarget {
    pub(crate) instance: ClientInstance,
    pub(crate) game_id: Option<u64>,
    pub(crate) champion_id: u32,
}

fn client_identity(instance: ClientInstance, shard: Option<&PlayerSearchShard>) -> Value {
    json!({
        "pid": instance.pid,
        "processCreationTime": instance.created_at.to_string(),
        "region": shard.map_or("", |shard| shard.region.as_str()),
        "rsoPlatformId": shard.map_or("", |shard| shard.rso_platform.as_str()),
    })
}

pub(crate) fn client_from_view(value: &Value) -> Option<ClientInstance> {
    Some(ClientInstance {
        pid: u32::try_from(value["pid"].as_u64()?).ok()?,
        created_at: value["processCreationTime"].as_str()?.parse().ok()?,
    })
}

impl Snapshot {
    fn observe_room(&self, flow: &mut RoomFlow) -> Option<crate::lcu::RoomAction> {
        let active = self.active.as_ref();
        let sample = active.and_then(|client| client.room.as_ref());
        let same_stage_unconfirmed = match active {
            Some(client) => {
                sample.is_none()
                    && client.room_unconfirmed
                    && flow.tracks_stage(
                        client.instance,
                        &client.phase,
                        client.summoner["puuid"].as_str(),
                    )
            }
            None => flow
                .tracked_client()
                .is_some_and(|instance| self.known_clients.contains(&instance)),
        };
        if same_stage_unconfirmed {
            flow.suspend();
            None
        } else {
            flow.observe(sample, Instant::now(), epoch_millis())
        }
    }

    fn left_draft(&self, next: &Self) -> bool {
        let Some(before) = self.active.as_ref().filter(|a| a.phase == "ChampSelect") else {
            return false;
        };
        let game = |a: &ActiveClient| {
            a.session["gameData"]["gameId"]
                .as_u64()
                .filter(|id| *id > 0)
        };
        next.active.as_ref().is_none_or(|after| {
            after.phase != "ChampSelect"
                || before.instance != after.instance
                || game(before).zip(game(after)).is_some_and(|(a, b)| a != b)
        })
    }

    fn fields(&self, namespace: &str, name: &str) -> Option<Value> {
        let active = self.active.as_ref();
        let shard = active.and_then(|client| client.shard.as_ref());
        match (namespace, name) {
            (CLIENT_DISCOVERY, "state") => Some(json!({
                "launchedClients": self.known_clients.iter().copied().map(|instance| {
                    let known_shard = active.filter(|client| client.instance == instance).and(shard);
                    client_identity(instance, known_shard)
                }).collect::<Vec<_>>(),
            })),
            (CLIENT, "state") => Some(json!({
                "connectionState": if active.is_some() { "connected" } else { "disconnected" },
                "auth": active.map(|client| client_identity(client.instance, shard)),
                "connectingClient": null,
            })),
            (CLIENT, "gameflow") => Some(json!({
                "phase": active.map(|client| client.phase.as_str()),
                "session": active.map(|client| &client.session),
            })),
            (CLIENT, "summoner") => Some(json!({"me": active.map(|client| &client.summoner)})),
            (aux_activity::NAMESPACE, "state") => Some(json!({
                "view": active.and_then(|client| client.aux_activity.view()),
            })),
            (opgg_window::NAMESPACE, "state") => Some(json!({
                "draft": active.and_then(|client| client.companions.opgg).map(OpggTarget::view),
            })),
            (SGP, "state") => Some(json!({
                "availability": {
                    "region": shard.map_or("", |shard| shard.region.as_str()),
                    "rsoPlatform": shard.map_or("", |shard| shard.rso_platform.as_str()),
                    "sgpServerId": shard.map_or("", |shard| shard.sgp_server_id.as_str()),
                    "serversSupported": { "matchHistory": false, "common": false },
                },
                "isTokenReady": false, "supportedQueues": [],
                "connectionSuccessesCounted": 0, "connectionFailuresCounted": 0,
            })),
            _ => None,
        }
    }
}

const PUBLIC_STATES: [(&str, &str); 7] = [
    (CLIENT_DISCOVERY, "state"),
    (CLIENT, "state"),
    (CLIENT, "gameflow"),
    (CLIENT, "summoner"),
    (SGP, "state"),
    (aux_activity::NAMESPACE, "state"),
    (opgg_window::NAMESPACE, "state"),
];

#[derive(Default)]
struct RuntimeState {
    generation: u64,
    revision: u64,
    auto_connect: bool,
    manual_client: Option<ClientInstance>,
    manually_disconnected: bool,
    snapshot: Snapshot,
    aram_options: AramOptions,
    aram_paused: bool,
    aram: AramSchedule,
    room_flow: RoomFlow,
    published_room: Value,
    published_draft: Value,
    data_client: Option<ClientInstance>,
    data_ticket: u64,
    data_task: Option<tauri::async_runtime::JoinHandle<()>>,
    game_data: Map<String, Value>,
    art_client: Option<Arc<LocalClient>>,
    history_access: Option<HistoryAccess>,
    opgg_checked_at: Option<Instant>,
}

#[derive(PartialEq, Eq)]
struct HistorySession {
    instance: ClientInstance,
    generation: u64,
    player: String,
    shard: PlayerSearchShard,
}

struct HistoryAccess {
    session: HistorySession,
    game_ids: VecDeque<u64>,
}

pub(crate) fn empty_game_data() -> Value {
    json!({
        "champions":{},"augments":{},"perks":{},"perkstyles":{"schemaVersion":0,"styles":{}},
        "queues":{},"items":{},"summonerSpells":{},"gameModeMutators":{},"maps":{},
    })
}

impl RuntimeState {
    fn publish_room(&mut self, app: &AppHandle) {
        publish_live_fields(
            app,
            room_flow::NAMESPACE,
            self.room_flow.fields(),
            &mut self.published_room,
            &mut self.revision,
        );
    }

    fn draft_fields(&self) -> Value {
        let mut fields = self.aram.delayed_fields();
        fields["temporarilyDisabled"] = json!(self.aram_paused);
        fields
    }

    fn publish_draft(&mut self, app: &AppHandle) {
        publish_live_fields(
            app,
            "auto-select-main",
            self.draft_fields(),
            &mut self.published_draft,
            &mut self.revision,
        );
    }

    fn history_session(&self) -> Option<HistorySession> {
        let active = self.snapshot.active.as_ref()?;
        Some(HistorySession {
            instance: active.instance,
            generation: self.generation,
            player: active.summoner["puuid"].as_str()?.to_owned(),
            shard: active.shard.clone()?,
        })
    }

    fn scheduled_selection_generation_is_current(&self, generation: u64) -> bool {
        self.generation == generation && !self.aram_paused
    }

    fn claim_manual_aux_selection(
        &mut self,
        generation: u64,
        instance: ClientInstance,
        expected: &AuxView,
    ) -> Option<u64> {
        let active_matches = self.snapshot.active.as_ref().is_some_and(|active| {
            active.instance == instance
                && active.aux_activity.view().is_some_and(|view| {
                    view.phase == expected.phase
                        && view.game_mode == expected.game_mode
                        && (expected.game_id.is_none() || view.game_id == expected.game_id)
                })
        });
        if self.generation != generation || !active_matches {
            return None;
        }
        self.aram.cancel();
        self.generation = self.generation.wrapping_add(1);
        Some(self.generation)
    }

    fn verified_opgg_target(&self) -> Option<OpggTarget> {
        let active = self.snapshot.active.as_ref()?;
        let target = active.companions.opgg?;
        let client = self.art_client.as_ref()?;
        (self.opgg_checked_at?.elapsed() <= Duration::from_secs(5)
            && client.instance == active.instance
            && client.alive())
        .then_some(target)
    }

    fn apply_snapshot(
        &mut self,
        app: &AppHandle,
        snapshot: Snapshot,
        client: Option<Arc<LocalClient>>,
    ) -> Option<crate::lcu::RoomAction> {
        let due = snapshot.observe_room(&mut self.room_flow);
        if self.aram_paused && self.snapshot.left_draft(&snapshot) {
            self.aram_paused = false;
            self.aram.reset();
            self.publish_draft(app);
        }
        self.publish_room(app);
        if self.snapshot != snapshot {
            self.revision += 1;
            publish_changes(app, &self.snapshot, &snapshot, self.revision);
            self.snapshot = snapshot;
        }
        let active = self.snapshot.active.as_ref();
        self.art_client =
            client.filter(|client| active.is_some_and(|active| active.instance == client.instance));
        self.opgg_checked_at =
            active.and_then(|active| active.companions.opgg.map(|_| Instant::now()));
        due
    }

    fn refresh_game_data(&mut self, app: &AppHandle) {
        let active = self.snapshot.active.as_ref().map(|client| client.instance);
        if self.data_client == active {
            return;
        }
        self.data_client = active;
        self.data_ticket += 1;
        if let Some(task) = self.data_task.take() {
            task.abort();
        }
        let defaults = empty_game_data();
        for (key, _) in std::mem::take(&mut self.game_data) {
            publish_property(
                app,
                CLIENT,
                "gameData",
                &key,
                &defaults[&key],
                self.revision,
            );
        }
        if let Some(client) = active {
            self.data_task = Some(tauri::async_runtime::spawn(load_game_data(
                app.clone(),
                client,
                self.data_ticket,
            )));
        }
    }
}

pub(crate) struct ClientRuntime {
    transport: LcuTransport,
    state: Mutex<RuntimeState>,
    wake: tokio::sync::Notify,
    art_reads: tokio::sync::Semaphore,
    room_writes: tokio::sync::Mutex<()>,
    selection_writes: tokio::sync::Mutex<()>,
}

impl ClientRuntime {
    pub(crate) fn new(auto_connect: bool, aram_options: AramOptions) -> Result<Self, LcuError> {
        Ok(Self {
            transport: LcuTransport::new()?,
            state: Mutex::new(RuntimeState {
                auto_connect,
                aram_options,
                ..Default::default()
            }),
            wake: tokio::sync::Notify::new(),
            art_reads: tokio::sync::Semaphore::new(4),
            room_writes: tokio::sync::Mutex::new(()),
            selection_writes: tokio::sync::Mutex::new(()),
        })
    }

    pub(crate) fn fields(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Option<(u64, Value)>, &'static str> {
        let state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
        if (namespace, name) == (room_flow::NAMESPACE, "state") {
            return Ok(Some((state.revision, state.room_flow.fields())));
        }
        if (namespace, name) == (CLIENT, "gameData") {
            let mut fields = empty_game_data();
            fields
                .as_object_mut()
                .expect("known game data fields")
                .extend(state.game_data.clone());
            return Ok(Some((state.revision, fields)));
        }
        Ok(state
            .snapshot
            .fields(namespace, name)
            .map(|fields| (state.revision, fields)))
    }

    pub(crate) fn resg_target(&self) -> Option<ResgTarget> {
        self.state
            .lock()
            .ok()?
            .snapshot
            .active
            .as_ref()?
            .companions
            .resg
    }

    pub(crate) fn aux_observation(&self) -> AuxObservation {
        let Ok(state) = self.state.lock() else {
            return AuxObservation::Unconfirmed;
        };
        let Some(active) = state.snapshot.active.as_ref() else {
            return if state.manually_disconnected {
                AuxObservation::Inactive
            } else {
                AuxObservation::Unconfirmed
            };
        };
        let view = match &active.aux_activity {
            AuxActivity::Inactive => return AuxObservation::Inactive,
            AuxActivity::Unconfirmed => return AuxObservation::Unconfirmed,
            AuxActivity::Active(view) => view,
        };
        let Some(client) = state
            .art_client
            .as_ref()
            .filter(|client| client.instance == active.instance && client.alive())
        else {
            return AuxObservation::Unconfirmed;
        };
        let stage = if view.phase == AuxPhase::ChampSelect {
            AuxStage::Selection
        } else {
            AuxStage::Room
        };
        AuxObservation::Active {
            stage,
            client: Arc::clone(client),
            game_id: view.game_id,
        }
    }

    #[cfg(test)]
    pub(crate) fn use_fixture_transport(
        &mut self,
        root: rustls::pki_types::CertificateDer<'static>,
    ) {
        self.transport = LcuTransport::with_root(root).expect("fixture TLS root");
    }

    #[cfg(test)]
    pub(crate) async fn sample_fixture(&self, app: &AppHandle, client: &Arc<LocalClient>) {
        let sample = self
            .sample(client, CompanionDemand::from_app(app), None, None, false)
            .await
            .expect("sample loopback League fixture")
            .expect("fixture is active");
        let (due, generation) = {
            let mut state = self.state.lock().unwrap();
            let due = state.apply_snapshot(
                app,
                Snapshot {
                    known_clients: vec![client.instance],
                    active: Some(sample.client),
                },
                Some(Arc::clone(client)),
            );
            state.refresh_game_data(app);
            (due, state.generation)
        };
        if let Some(action) = due {
            self.dispatch_due_room(app, client, generation, action)
                .await;
        }
    }

    #[cfg(test)]
    pub(crate) fn apply_companion_fixture(
        &self,
        app: &AppHandle,
        session: GameflowSession,
        selection: Value,
        lobby: Option<Value>,
        search: Option<Value>,
    ) {
        const PLAYER: &str = "fixture-player";
        let client = Arc::new(LocalClient::fixture().expect("retain current fixture process"));
        let instance = client.instance;
        let demand = CompanionDemand::from_app(app);
        let selection = demand
            .reads_selection(&session.phase, Some(&session), false)
            .then_some(&selection);
        let companions = demand.targets(instance, Some(&session), selection, Some(PLAYER), None);
        let aux_activity = demand.aux_activity(
            &session.phase,
            Some(&session),
            lobby.as_ref(),
            search.as_ref(),
            selection,
        );
        let room = RoomReads {
            lobby,
            search,
            search_read: true,
        }
        .sample(instance, Some(PLAYER), &aux_activity);
        let room_unconfirmed = matches!(aux_activity, AuxActivity::Unconfirmed);
        self.state.lock().unwrap().apply_snapshot(
            app,
            Snapshot {
                known_clients: vec![instance],
                active: Some(ActiveClient {
                    instance,
                    phase: session.phase.clone(),
                    session: json!(session),
                    summoner: json!({"puuid": PLAYER}),
                    companions,
                    aux_activity,
                    room,
                    room_unconfirmed,
                    shard: None,
                }),
            },
            Some(client),
        );
    }

    pub(crate) fn opgg_target(&self) -> Option<OpggTarget> {
        self.state.lock().ok()?.verified_opgg_target()
    }

    pub(crate) fn opgg_guide(&self) -> Option<(OpggTarget, String)> {
        let state = self.state.lock().ok()?;
        let target = state.verified_opgg_target()?;
        let alias = target.champion_id.and_then(|id| {
            state
                .game_data
                .get("champions")?
                .get(id.to_string())?
                .get("alias")?
                .as_str()
        });
        Some((target, target.guide_url(alias)))
    }

    pub(crate) fn refresh_soon(&self) {
        self.wake.notify_one();
    }

    pub(crate) async fn game_art(&self, path: &GameArtPath) -> Option<GameArt> {
        let _permit = tokio::time::timeout(Duration::from_secs(3), self.art_reads.acquire())
            .await
            .ok()?
            .ok()?;
        let client = self.state.lock().ok()?.art_client.clone()?;
        let target = client.target().ok()?;
        let art = self.transport.read_game_art(&target, path).await.ok()??;
        self.state
            .lock()
            .ok()?
            .art_client
            .as_ref()
            .is_some_and(|current| current.instance == client.instance && client.alive())
            .then_some(art)
    }

    pub(crate) fn selected_client(&self) -> Option<Arc<LocalClient>> {
        let state = self.state.lock().ok()?;
        let active = state.snapshot.active.as_ref()?;
        let client = state.art_client.as_ref()?;
        (client.instance == active.instance && client.alive()).then(|| Arc::clone(client))
    }

    fn aux_snapshot(&self) -> Result<(Arc<LocalClient>, u64, AuxView), String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "客户端状态不可用。".to_owned())?;
        let active = state
            .snapshot
            .active
            .as_ref()
            .ok_or("请先连接英雄联盟客户端。")?;
        let view = active
            .aux_activity
            .view()
            .cloned()
            .ok_or("当前客户端没有可操作的大乱斗状态。")?;
        let client = state
            .art_client
            .as_ref()
            .filter(|client| client.instance == active.instance && client.alive())
            .cloned()
            .ok_or("客户端已断开，请重新连接。")?;
        Ok((client, state.generation, view))
    }

    fn aux_snapshot_is_current(
        &self,
        client: &LocalClient,
        generation: u64,
        expected: &AuxView,
    ) -> bool {
        self.state.lock().is_ok_and(|state| {
            state.generation == generation
                && state
                    .art_client
                    .as_ref()
                    .is_some_and(|current| current.instance == client.instance && current.alive())
                && state.snapshot.active.as_ref().is_some_and(|active| {
                    active.instance == client.instance
                        && active.aux_activity.view().is_some_and(|view| {
                            view.phase == expected.phase
                                && view.game_mode == expected.game_mode
                                && (expected.game_id.is_none() || view.game_id == expected.game_id)
                        })
                })
        })
    }

    async fn fresh_aux_selection(
        &self,
        client: Arc<LocalClient>,
        generation: u64,
        original: AuxView,
    ) -> Result<FreshAuxSelection, String> {
        if original.phase != AuxPhase::ChampSelect {
            return Err("当前不在大乱斗英雄选择阶段。".into());
        }
        let target = client.target().map_err(|_| "客户端已断开，请重新连接。")?;
        let phase =
            tokio::time::timeout(Duration::from_secs(2), self.transport.read_phase(&target))
                .await
                .map_err(|_| "读取当前游戏阶段超时，请重试。")?
                .map_err(|_| "无法读取当前游戏阶段，请重试。")?;
        if phase.as_deref() != Some("ChampSelect") {
            return Err("英雄选择阶段已经结束，请重新查看状态。".into());
        }
        let (flow, selection) = futures_util::future::join(
            tokio::time::timeout(
                Duration::from_secs(2),
                self.transport.read_gameflow_session(&target),
            ),
            tokio::time::timeout(
                Duration::from_secs(2),
                self.transport.read_champ_select(&target),
            ),
        )
        .await;
        let flow = flow
            .map_err(|_| "读取当前大乱斗会话超时，请重试。")?
            .map_err(|_| "无法读取当前大乱斗会话，请重试。")?
            .ok_or("无法确认当前大乱斗会话，请重试。")?;
        let selection = selection
            .map_err(|_| "读取英雄选择状态超时，请重试。")?
            .map_err(|_| "无法读取英雄选择状态，请重试。")?;
        if flow.phase != "ChampSelect" {
            return Err("英雄选择阶段已经结束，请重新查看状态。".into());
        }
        if flow
            .game_data
            .as_ref()
            .is_some_and(|data| data.is_custom_game == Some(true))
            || selection["isCustomGame"] == true
        {
            return Err("自定义对局不支持此操作。".into());
        }
        let extras = self
            .transport
            .read_aux_selection_extras(&target, &selection)
            .await
            .unwrap_or_default();
        let activity = AuxActivity::project_with_extras(
            "ChampSelect",
            Some(&flow),
            None,
            None,
            Some(&selection),
            Some(&extras),
        );
        let current = activity
            .view()
            .cloned()
            .ok_or("当前大乱斗英雄选择状态不可确认，请重试。")?;
        if current.game_mode != original.game_mode
            || (original.game_id.is_some() && current.game_id != original.game_id)
        {
            return Err("当前对局已变化，请重新查看状态。".into());
        }
        if !self.aux_snapshot_is_current(&client, generation, &original) {
            return Err("当前连接或对局状态已变化，请重新查看。".into());
        }
        Ok(FreshAuxSelection {
            client,
            generation,
            original,
            current,
            flow,
            selection,
            extras,
        })
    }

    async fn aux_room_action(
        &self,
        action: RoomAction,
        expected_phase: AuxPhase,
    ) -> Result<(), String> {
        let (client, generation, view) = self.aux_snapshot()?;
        if view.phase != expected_phase {
            return Err("房间或准备确认状态已经变化，请重新查看。".into());
        }
        self.guarded_room_write(&client, action, generation).await
    }

    pub(crate) async fn aux_action(
        &self,
        app: &AppHandle,
        method: &str,
        args: &[Value],
    ) -> Result<Value, String> {
        match method {
            "cancelMatchmaking" if args.is_empty() => {
                self.aux_room_action(RoomAction::CancelMatchmaking, AuxPhase::Matchmaking)
                    .await?;
            }
            "acceptReadyCheck" if args.is_empty() => {
                self.aux_room_action(RoomAction::AcceptReadyCheck, AuxPhase::ReadyCheck)
                    .await?;
            }
            "declineReadyCheck" if args.is_empty() => {
                self.aux_room_action(RoomAction::DeclineReadyCheck, AuxPhase::ReadyCheck)
                    .await?;
            }
            "pauseAutoSelect" if args.len() == 1 => {
                let paused = args[0].as_bool().ok_or("无效的自动选禁暂停状态。")?;
                let (client, generation, original) = self.aux_snapshot()?;
                let fresh = self
                    .fresh_aux_selection(client, generation, original)
                    .await?;
                if !self.aux_snapshot_is_current(&fresh.client, fresh.generation, &fresh.original) {
                    return Err("当前连接或对局状态已变化，请重新查看。".into());
                }
                self.pause_aram(app, paused)
                    .map_err(|error| error.to_owned())?;
                return Ok(Value::Null);
            }
            "chooseChampion" if args.len() == 2 => {
                let champion_id = aux_champion_arg(&args[0])?;
                let complete = args[1].as_bool().ok_or("无效的选人操作。")?;
                let (client, generation, original) = self.aux_snapshot()?;
                let fresh = self
                    .fresh_aux_selection(client, generation, original)
                    .await?;
                let action = validate_choose_champion(&fresh, champion_id, complete)?;
                self.apply_aux_selection_action(&fresh, action).await?;
            }
            "swapBenchChampion" if args.len() == 1 => {
                let champion_id = aux_champion_arg(&args[0])?;
                let (client, generation, original) = self.aux_snapshot()?;
                let fresh = self
                    .fresh_aux_selection(client, generation, original)
                    .await?;
                let action = validate_bench_swap(&fresh, champion_id)?;
                self.apply_aux_selection_action(&fresh, action).await?;
            }
            "reroll" if args.is_empty() => {
                let (client, generation, original) = self.aux_snapshot()?;
                let fresh = self
                    .fresh_aux_selection(client, generation, original)
                    .await?;
                validate_reroll(&fresh.current)?;
                self.apply_aux_selection_action(&fresh, AuxChampSelectAction::Reroll)
                    .await?;
            }
            "charityReroll" if args.is_empty() => {
                return self.aux_charity_reroll().await;
            }
            "selectSkin" if args.len() == 1 => {
                let skin_id = aux_skin_arg(&args[0])?;
                let (client, generation, original) = self.aux_snapshot()?;
                let fresh = self
                    .fresh_aux_selection(client, generation, original)
                    .await?;
                validate_skin(&fresh, skin_id)?;
                self.apply_aux_selection_action(
                    &fresh,
                    AuxChampSelectAction::SelectSkin { skin_id },
                )
                .await?;
            }
            "leaveGame" if args.is_empty() => {
                let (client, generation, original) = self.aux_snapshot()?;
                let fresh = self
                    .fresh_aux_selection(client, generation, original)
                    .await?;
                if fresh
                    .flow
                    .game_data
                    .as_ref()
                    .and_then(|data| data.is_custom_game)
                    != Some(false)
                {
                    return Err("无法确认这是普通大乱斗对局，已阻止离开操作。".into());
                }
                self.apply_aux_selection_action(&fresh, AuxChampSelectAction::Leave)
                    .await?;
            }
            _ => return Err("此大乱斗操作不可用或参数无效。".into()),
        }
        Ok(Value::Null)
    }

    async fn apply_aux_selection_action(
        &self,
        fresh: &FreshAuxSelection,
        action: AuxChampSelectAction,
    ) -> Result<u64, String> {
        let _writer = self.selection_writes.lock().await;
        let generation =
            {
                let mut state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
                if state.art_client.as_ref().is_none_or(|client| {
                    client.instance != fresh.client.instance || !client.alive()
                }) {
                    return Err("当前连接或对局状态已变化，请重新查看。".into());
                }
                state
                    .claim_manual_aux_selection(
                        fresh.generation,
                        fresh.client.instance,
                        &fresh.original,
                    )
                    .ok_or("当前连接或对局状态已变化，请重新查看。")?
            };
        let target = fresh
            .client
            .target()
            .map_err(|_| "客户端已断开，请重新连接。")?;
        self.transport
            .apply_aux_champ_select_action(&target, action)
            .await
            .map_err(|error| format!("客户端操作失败：{error}"))?;
        self.refresh_soon();
        Ok(generation)
    }

    async fn aux_charity_reroll(&self) -> Result<Value, String> {
        let (client, generation, original) = self.aux_snapshot()?;
        let fresh = self
            .fresh_aux_selection(client, generation, original)
            .await?;
        validate_reroll(&fresh.current)?;
        let held_champion = fresh
            .current
            .champion_id
            .ok_or("当前没有已选择的英雄，无法执行慈善重随。")?;
        let generation = self
            .apply_aux_selection_action(&fresh, AuxChampSelectAction::Reroll)
            .await?;

        let result = match self
            .fresh_aux_selection(
                Arc::clone(&fresh.client),
                generation,
                fresh.original.clone(),
            )
            .await
        {
            Ok(after) => match validate_return_swap(&after, held_champion) {
                Ok(()) => match self
                    .apply_aux_selection_action(
                        &after,
                        AuxChampSelectAction::BenchSwap {
                            champion_id: held_champion,
                        },
                    )
                    .await
                {
                    Ok(_) => json!({"rerolled":true,"grabbedBack":true}),
                    Err(error) => json!({"rerolled":true,"grabbedBack":false,"swapReason":error}),
                },
                Err(error) => json!({"rerolled":true,"grabbedBack":false,"swapReason":error}),
            },
            Err(error) => json!({"rerolled":true,"grabbedBack":false,"swapReason":error}),
        };
        Ok(result)
    }

    pub(crate) async fn resize_client_window(&self, width: u32, height: u32) -> Result<(), String> {
        let client = self.selected_client().ok_or("请先连接英雄联盟客户端。")?;
        let target = client.target().map_err(|_| "客户端已断开，请重新连接。")?;
        let zoom = self
            .transport
            .read_window_zoom(&target)
            .await
            .map_err(|_| "无法读取客户端窗口缩放比例，请重试。")?;
        if self
            .selected_client()
            .is_none_or(|current| current.instance != client.instance)
        {
            return Err("当前连接的客户端已变更，请重试。".into());
        }
        tauri::async_runtime::spawn_blocking(move || {
            crate::windows_tools::resize_league_client_window(&client, width, height, zoom)
        })
        .await
        .map_err(|_| "窗口调整失败。".to_owned())?
    }

    pub(crate) async fn read_player_tab(&self, request: PlayerTabRead) -> Result<Value, String> {
        let (session, client) = {
            let state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
            let session = state
                .history_session()
                .ok_or("请启动并登录英雄联盟客户端。")?;
            let client = state
                .art_client
                .as_ref()
                .filter(|client| client.instance == session.instance)
                .ok_or("客户端已断开，请重新连接。")?;
            if request.detail_id().is_some_and(|id| {
                !state.history_access.as_ref().is_some_and(|history| {
                    history.session == session && history.game_ids.contains(&id)
                })
            }) {
                return Err("请先加载这场对局所在的战绩页。".into());
            }
            (session, Arc::clone(client))
        };
        let target = client.target().map_err(|_| "客户端已断开，请重新连接。")?;
        let data = self
            .transport
            .read_player_tab(&target, &request, &session.player, &session.shard)
            .await
            .map_err(|error| match error {
                LcuError::Timeout => "读取战绩或玩家资料超时，请重试。",
                LcuError::AuthenticationRejected => "当前客户端拒绝读取该玩家的资料或战绩。",
                LcuError::UnexpectedStatus(404) => "客户端没有返回所请求的资料，请稍后重试。",
                _ => "读取战绩或玩家资料失败，请检查客户端连接后重试。",
            })?;
        // The active snapshot is polled. Verify the session at the LCU again
        // before showing results or admitting detail IDs from the page.
        if !self
            .transport
            .read_player_search_shard(&target, &session.player)
            .await
            .is_ok_and(|observed| observed == session.shard)
        {
            return Err("无法确认当前登录账号，请重新查询。".into());
        }
        let mut state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
        if !client.alive() || state.history_session().as_ref() != Some(&session) {
            return Err("客户端或登录账号已变更，请重新查询。".into());
        }
        if request.is_page() {
            if state
                .history_access
                .as_ref()
                .is_none_or(|history| history.session != session)
            {
                state.history_access = Some(HistoryAccess {
                    session,
                    game_ids: VecDeque::new(),
                });
            }
            let history = state.history_access.as_mut().expect("active history");
            if let Some(games) = data["games"]["games"].as_array() {
                for id in games.iter().filter_map(|game| game["gameId"].as_u64()) {
                    if !history.game_ids.contains(&id) {
                        history.game_ids.push_back(id);
                    }
                }
                history
                    .game_ids
                    .drain(..history.game_ids.len().saturating_sub(1000));
            }
        }
        Ok(data)
    }

    pub(crate) fn replace_room_options(
        &self,
        app: &AppHandle,
        options: RoomOptions,
    ) -> Result<(), &'static str> {
        let mut state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
        if state.room_flow.options != options {
            state.room_flow.options = options;
            let state = &mut *state;
            state.snapshot.observe_room(&mut state.room_flow);
            state.publish_room(app);
            state.generation += 1;
            self.refresh_soon();
        }
        Ok(())
    }

    pub(crate) fn cancel_room_timer(
        &self,
        app: &AppHandle,
        action: crate::lcu::RoomAction,
    ) -> Result<(), &'static str> {
        let mut state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
        state.room_flow.cancel(action);
        state.publish_room(app);
        self.refresh_soon();
        Ok(())
    }

    pub(crate) fn replace_aram_options(
        &self,
        app: &AppHandle,
        options: AramOptions,
    ) -> Result<(), &'static str> {
        let mut state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
        state.aram_options = options;
        state.aram.cancel();
        state.publish_draft(app);
        state.generation += 1;
        self.refresh_soon();
        Ok(())
    }

    pub(crate) fn aram_state(&self) -> Result<(u64, Value), &'static str> {
        let state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
        Ok((state.revision, state.draft_fields()))
    }

    pub(crate) fn pause_aram(&self, app: &AppHandle, paused: bool) -> Result<(), &'static str> {
        let mut state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
        if state.aram_paused != paused {
            state.aram_paused = paused;
            state.aram.cancel();
            state.generation += 1;
            state.publish_draft(app);
            self.refresh_soon();
        }
        Ok(())
    }

    pub(crate) async fn lookup_player(&self, shard: &str, input: &str) -> Result<Value, String> {
        let (instance, own_puuid, generation) = {
            let state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
            let active = state
                .snapshot
                .active
                .as_ref()
                .ok_or("请启动并登录英雄联盟客户端。")?;
            let own_puuid = active.summoner["puuid"]
                .as_str()
                .ok_or("请登录英雄联盟客户端后查询。")?;
            if active
                .shard
                .as_ref()
                .is_none_or(|active| active.sgp_server_id != shard)
            {
                return Err("当前区服不可用，请检查客户端登录状态。".to_owned());
            }
            (active.instance, own_puuid.to_owned(), state.generation)
        };
        let local = tauri::async_runtime::spawn_blocking(move || {
            discover_clients()
                .ok()?
                .into_iter()
                .find(|client| client.instance == instance)
        })
        .await
        .map_err(|_| "无法读取当前客户端。")?
        .ok_or("客户端已退出，请重新连接。")?;
        let target = local.target().map_err(|error| error.to_string())?;
        let result = tokio::time::timeout(
            Duration::from_secs(8),
            self.transport
                .lookup_player(&target, &own_puuid, Some(shard), input),
        )
        .await
        .map_err(|_| "玩家查询超时，请重试。")?
        .map_err(|error| error.to_string())?;
        let state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
        let same_account = state.snapshot.active.as_ref().is_some_and(|active| {
            active.instance == instance
                && active.summoner["puuid"].as_str() == Some(own_puuid.as_str())
                && active
                    .shard
                    .as_ref()
                    .is_some_and(|current| current.sgp_server_id == shard)
        });
        if state.generation != generation || !same_account || !local.alive() {
            return Err("客户端或登录账号已变更，请重新查询。".to_owned());
        }
        serde_json::to_value(result).map_err(|error| error.to_string())
    }

    pub(crate) fn set_auto_connect(&self, enabled: bool) -> Result<(), &'static str> {
        let mut state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
        if state.auto_connect != enabled {
            state.auto_connect = enabled;
            if enabled {
                state.manually_disconnected = false;
            }
            state.generation += 1;
            self.wake.notify_one();
        }
        Ok(())
    }

    pub(crate) fn select(
        &self,
        app: &AppHandle,
        selection: Option<ClientInstance>,
    ) -> Result<(), &'static str> {
        let mut state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
        if selection.is_some_and(|client| !state.snapshot.known_clients.contains(&client)) {
            return Err("客户端已退出或尚未完成检测。");
        }
        state.generation += 1;
        state.manual_client = selection;
        state.manually_disconnected = selection.is_none();
        state.art_client = None;
        state.history_access = None;
        state.aram_paused = false;
        state.aram.reset();
        state.publish_draft(app);
        state
            .room_flow
            .observe(None, Instant::now(), epoch_millis());
        state.publish_room(app);
        let previous = Snapshot {
            active: state.snapshot.active.take(),
            known_clients: state.snapshot.known_clients.clone(),
        };
        state.revision += 1;
        publish_changes(app, &previous, &state.snapshot, state.revision);
        state.refresh_game_data(app);
        self.wake.notify_one();
        Ok(())
    }

    async fn read_aux_room(&self, target: &LcuTarget<'_>, phase: &str, enabled: bool) -> RoomReads {
        if !enabled || !matches!(phase, "Lobby" | "Matchmaking" | "ReadyCheck") {
            return RoomReads::default();
        }
        let direct_ready = async {
            if phase != "ReadyCheck" {
                return None;
            }
            optional_lcu(self.transport.read_aux_ready_check(target))
                .await
                .flatten()
        };
        let (lobby, search_result, direct_ready) = join3(
            optional_lcu(self.transport.read_aux_lobby(target)),
            optional_lcu(self.transport.read_aux_matchmaking(target)),
            direct_ready,
        )
        .await;
        let search_read = search_result.is_some();
        let mut search = search_result.flatten();
        if let Some(ready) = direct_ready.filter(Value::is_object) {
            let from_search = search
                .as_ref()
                .and_then(|s| s["readyCheck"]["playerResponse"].as_str());
            let from_ready = ready["playerResponse"].as_str();
            // The dedicated ready-check is authoritative for a confirmed change of answer.
            // A still-pending response must not replace an accepted/declined search event.
            if matches!(from_ready, Some("Accepted" | "Declined"))
                || (!matches!(from_search, Some("Accepted" | "Declined"))
                    && (from_search != Some("None")
                        || matches!(ready["state"].as_str(), Some("Invalid" | "EveryoneReady"))))
            {
                search.get_or_insert_with(|| json!({}))["readyCheck"] = ready;
            }
        }
        RoomReads {
            lobby: lobby.flatten(),
            search,
            search_read,
        }
    }

    async fn sample(
        &self,
        client: &LocalClient,
        demand: CompanionDemand,
        previous_chart: Option<ResgTarget>,
        previous_shard: Option<(String, PlayerSearchShard)>,
        automate: bool,
    ) -> Result<Option<Sample>, LcuError> {
        if !client.alive() {
            return Ok(None);
        }
        let target = client.target()?;
        let Some(phase) = self.transport.read_phase(&target).await? else {
            return Ok(None);
        };
        let room_enabled = self
            .state
            .lock()
            .is_ok_and(|s| s.room_flow.options.search || s.room_flow.options.accept);
        let (summoner, session, reads) = join3(
            self.transport.read_current_summoner(&target),
            optional_lcu(self.transport.read_gameflow_session(&target)),
            self.read_aux_room(&target, &phase, demand.aux || room_enabled),
        )
        .await;
        let summoner = summoner?;
        let flow_snapshot = session.flatten();
        let session = flow_snapshot
            .as_ref()
            .filter(|session| session.phase == phase);
        let mut shard = None;
        if let Some(player) = summoner.as_ref() {
            shard = if let Some((_, shard)) =
                previous_shard.filter(|(puuid, _)| *puuid == player.puuid)
            {
                Some(shard)
            } else {
                tokio::time::timeout(
                    Duration::from_millis(550),
                    self.transport
                        .read_player_search_shard(&target, &player.puuid),
                )
                .await
                .ok()
                .and_then(Result::ok)
            };
        }
        let selection = if demand.reads_selection(&phase, session, automate) {
            optional_lcu(self.transport.read_champ_select(&target)).await
        } else {
            None
        };
        let companions = demand.targets(
            client.instance,
            session,
            selection.as_ref(),
            summoner.as_ref().map(|player| player.puuid.as_str()),
            previous_chart,
        );
        let extras = if demand.aux && phase == "ChampSelect" {
            if let Some(select) = selection.as_ref().filter(|select| select.is_object()) {
                self.transport
                    .read_aux_selection_extras(&target, select)
                    .await
                    .ok()
            } else {
                None
            }
        } else {
            None
        };
        let activity = AuxActivity::project_with_extras(
            &phase,
            flow_snapshot.as_ref(),
            reads.lobby.as_ref(),
            reads.search.as_ref(),
            selection.as_ref(),
            extras.as_ref(),
        );
        let room = reads.sample(
            client.instance,
            summoner.as_ref().map(|s| s.puuid.as_str()),
            &activity,
        );
        let room_unconfirmed = matches!(activity, AuxActivity::Unconfirmed)
            || (room.is_none()
                && activity
                    .view()
                    .is_some_and(|view| view.phase != AuxPhase::ChampSelect));
        let aux_activity = if demand.aux {
            activity
        } else {
            AuxActivity::Inactive
        };
        let gameflow_view = json!(session);
        let use_for_automation = automate && session.is_some();
        Ok(client.alive().then(|| {
            let client = ActiveClient {
                instance: client.instance,
                phase,
                session: gameflow_view,
                summoner: json!(summoner),
                companions,
                aux_activity,
                room,
                room_unconfirmed,
                shard,
            };
            let selection = if use_for_automation {
                flow_snapshot.zip(selection)
            } else {
                None
            };
            Sample { client, selection }
        }))
    }

    async fn refresh(&self, app: &AppHandle, demand: CompanionDemand) {
        let (generation, manual, connected, auto_connect, previous_chart, previous_shard, options) = {
            let Ok(state) = self.state.lock() else {
                return;
            };
            (
                state.generation,
                state.manual_client,
                state.snapshot.active.as_ref().map(|client| client.instance),
                state.auto_connect && !state.manually_disconnected,
                state
                    .snapshot
                    .active
                    .as_ref()
                    .and_then(|client| client.companions.resg),
                state.snapshot.active.as_ref().and_then(|client| {
                    Some((
                        client.summoner["puuid"].as_str()?.to_owned(),
                        client.shard.clone()?,
                    ))
                }),
                (!state.aram_paused && state.aram_options.any_enabled())
                    .then(|| state.aram_options.clone()),
            )
        };
        let clients = tauri::async_runtime::spawn_blocking(discover_clients)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
            .into_iter()
            .map(Arc::new)
            .collect::<Vec<_>>();
        let find_client = |instance| clients.iter().find(|client| client.instance == instance);
        let candidate = if let Some(selection) = manual {
            find_client(selection)
        } else {
            connected
                .and_then(find_client)
                .or_else(|| (auto_connect && clients.len() == 1).then(|| &clients[0]))
        };
        let sample = match candidate {
            Some(client) => tokio::time::timeout(
                Duration::from_secs(4),
                self.sample(
                    client,
                    demand,
                    previous_chart,
                    previous_shard.filter(|_| Some(client.instance) == connected),
                    options.is_some(),
                ),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .flatten(),
            None => None,
        };
        let (active, selection) = match sample {
            Some(sample) => (Some(sample.client), sample.selection),
            None => (None, None),
        };
        let mut known_clients: Vec<_> = clients.iter().map(|client| client.instance).collect();
        known_clients.sort_unstable();
        let snapshot = Snapshot {
            known_clients,
            active,
        };
        let due = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if state.generation != generation {
                return;
            }
            if snapshot.active.is_some()
                || state
                    .manual_client
                    .is_some_and(|client| !snapshot.known_clients.contains(&client))
            {
                state.manual_client = None;
            }
            let due = state.apply_snapshot(app, snapshot, candidate.cloned());
            state.refresh_game_data(app);
            if state
                .snapshot
                .active
                .as_ref()
                .is_none_or(|client| client.phase != "ChampSelect")
            {
                state.aram.reset();
            } else if selection.is_none() {
                state.aram.cancel();
            }
            due
        };
        if let (Some(action), Some(client)) = (due, candidate) {
            self.dispatch_due_room(app, client, generation, action)
                .await;
        }
        if let (Some(client), Some(options), Some((flow, select))) = (candidate, options, selection)
        {
            self.automate_aram(generation, client, &flow, &select, &options)
                .await;
        }
        if let Ok(mut state) = self.state.lock() {
            state.publish_draft(app);
        }
    }

    async fn dispatch_due_room(
        &self,
        app: &AppHandle,
        client: &LocalClient,
        generation: u64,
        action: crate::lcu::RoomAction,
    ) {
        if let Err(error) = self.write_room(client, action, generation, true).await {
            let event = if action == crate::lcu::RoomAction::AcceptReadyCheck {
                "error-accept-match"
            } else {
                "error-matchmaking"
            };
            for label in ["main", "aux"] {
                if let Some(window) = app.get_webview_window(label) {
                    let _ = emit(&window, room_flow::NAMESPACE, event, json!([error]));
                }
            }
        }
        if let Ok(mut state) = self.state.lock() {
            if state.generation == generation {
                state.room_flow.cancel(action);
            }
            state.publish_room(app);
        }
    }

    pub(crate) async fn guarded_room_write(
        &self,
        client: &LocalClient,
        action: crate::lcu::RoomAction,
        generation: u64,
    ) -> Result<(), String> {
        self.write_room(client, action, generation, false).await
    }

    async fn write_room(
        &self,
        client: &LocalClient,
        action: crate::lcu::RoomAction,
        generation: u64,
        automatic: bool,
    ) -> Result<(), String> {
        let _writer = self.room_writes.lock().await;
        let same_client = |state: &RuntimeState| {
            state.generation == generation
                && state.art_client.as_ref().is_some_and(|verified| {
                    verified.instance == client.instance && verified.alive()
                })
        };
        let expected = {
            let state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
            state
                .snapshot
                .active
                .as_ref()
                .filter(|_| same_client(&state))
                .and_then(|a| a.room.clone())
        };
        let stale = || {
            if automatic {
                Ok(())
            } else {
                Err("房间或准备确认状态已改变，请重新查看。".to_string())
            }
        };
        let Some(expected) = expected else {
            return stale();
        };
        let target = client.target().map_err(|e| e.to_string())?;
        let phase = self
            .transport
            .read_phase(&target)
            .await
            .map_err(|e| e.to_string())?;
        let Some(phase) = phase else {
            return stale();
        };
        let (summoner, flow, reads) = join3(
            self.transport.read_current_summoner(&target),
            optional_lcu(self.transport.read_gameflow_session(&target)),
            self.read_aux_room(&target, &phase, true),
        )
        .await;
        let summoner = summoner.map_err(|e| e.to_string())?;
        let activity = AuxActivity::project(
            &phase,
            flow.flatten().as_ref(),
            reads.lobby.as_ref(),
            reads.search.as_ref(),
            None,
        );
        let fresh = reads.sample(
            client.instance,
            summoner.as_ref().map(|s| s.puuid.as_str()),
            &activity,
        );
        let Some(fresh) = fresh.filter(|s| expected.same_room(s) && s.allows(action)) else {
            return stale();
        };
        {
            let mut state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
            if !same_client(&state) || !state.room_flow.current(&fresh, action, automatic) {
                return stale();
            }
            state.room_flow.mark_sent(action);
        }
        let result = self.transport.apply_room_action(&target, action).await;
        if result.is_err() {
            if let Ok(mut state) = self.state.lock() {
                state.room_flow.manual_retry_after_error(&fresh, action);
            }
        }
        self.refresh_soon();
        result.map_err(|e| e.to_string())
    }

    async fn automate_aram(
        &self,
        generation: u64,
        client: &LocalClient,
        flow: &GameflowSession,
        select: &Value,
        options: &AramOptions,
    ) {
        let cancel = |failed_step| {
            if let Ok(mut state) = self.state.lock() {
                if state.generation == generation {
                    if matches!(failed_step, Some(ScheduledStep::Trade { .. })) {
                        state.aram.cancel_trade();
                    } else {
                        state.aram.cancel();
                    }
                }
            }
        };
        let Some(scope) = SelectionScope::from_sample(client.instance, flow, select) else {
            return cancel(None);
        };
        let Ok(target) = client.target() else {
            return cancel(None);
        };
        let Ok(Ok(mut lists)) = tokio::time::timeout(
            Duration::from_millis(1200),
            self.transport.read_selection_lists(
                &target,
                options,
                select["allowSubsetChampionPicks"] == true,
            ),
        )
        .await
        else {
            return cancel(None);
        };
        if options.want_trade() && select["benchEnabled"] == true {
            lists.incoming_trade = tokio::time::timeout(
                Duration::from_millis(250),
                self.transport.read_champion_trade(&target),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .flatten();
        }
        let due = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if state.generation != generation || state.aram_paused {
                return;
            }
            state
                .aram
                .observe(scope, flow, select, &lists, options, Instant::now())
        };
        let Some(due) = due else {
            return;
        };
        let latest = tokio::time::timeout(Duration::from_millis(650), async {
            if self.transport.read_phase(&target).await?.as_deref() != Some("ChampSelect") {
                return Ok(Value::Null);
            }
            let current_flow = if let ScheduledStep::Trade { offer, .. } = due.step {
                let Some(current_flow) = self.transport.read_gameflow_session(&target).await?
                else {
                    return Ok(Value::Null);
                };
                if current_flow.queue() != flow.queue()
                    || self.transport.read_champion_trade(&target).await? != Some(offer)
                {
                    return Ok(Value::Null);
                }
                Some(current_flow)
            } else {
                None
            };
            let latest = self.transport.read_champ_select(&target).await?;
            let latest_flow = current_flow.as_ref().unwrap_or(flow);
            if SelectionScope::from_sample(client.instance, latest_flow, &latest) != Some(due.scope)
            {
                return Ok(Value::Null);
            }
            Ok::<_, LcuError>(latest)
        })
        .await;
        let Ok(Ok(latest)) = latest else {
            return cancel(Some(due.step));
        };
        if !due.still_applies(select, &latest, &lists.subset) {
            return cancel(Some(due.step));
        }
        let Ok(target) = client.target() else {
            return cancel(None);
        };
        let _writer = self.selection_writes.lock().await;
        let authorized = self.state.lock().is_ok_and(|mut state| {
            state.scheduled_selection_generation_is_current(generation)
                && state.aram.mark_attempted(due)
        });
        if authorized {
            let _ = self.transport.apply_selection(&target, due.step).await;
        }
    }
}

async fn load_game_data(app: AppHandle, instance: ClientInstance, ticket: u64) {
    let runtime = app.state::<ClientRuntime>();
    let Ok(Some(client)) = tauri::async_runtime::spawn_blocking(move || {
        discover_clients()
            .ok()?
            .into_iter()
            .find(|client| client.instance == instance)
    })
    .await
    else {
        return;
    };
    let Ok(target) = client.target() else {
        return;
    };
    let Ok(read) = runtime.transport.read_game_data(&target).await else {
        return;
    };
    let Ok(mut state) = runtime.state.lock() else {
        return;
    };
    if state.data_ticket != ticket || state.data_client != Some(instance) || !client.alive() {
        return;
    }
    state.revision += 1;
    state.game_data = read.fields;
    for (key, value) in &state.game_data {
        publish_property(&app, CLIENT, "gameData", key, value, state.revision);
    }
}

fn aux_champion_arg(value: &Value) -> Result<u32, String> {
    let id = value
        .as_u64()
        .and_then(|id| u32::try_from(id).ok())
        .filter(|&id| (1..100_000).contains(&id))
        .ok_or("无效的英雄 ID。")?;
    Ok(id)
}

fn aux_skin_arg(value: &Value) -> Result<u32, String> {
    value
        .as_u64()
        .and_then(|id| u32::try_from(id).ok())
        .filter(|&id| (1..1_000_000).contains(&id))
        .ok_or_else(|| "无效的皮肤 ID。".to_owned())
}

fn require_pickable(fresh: &FreshAuxSelection, champion_id: u32) -> Result<(), String> {
    if !fresh
        .extras
        .pickable_champion_ids
        .as_ref()
        .is_some_and(|ids| ids.contains(&champion_id))
    {
        return Err("无法确认该英雄当前可选，或该英雄已经被占用。".into());
    }
    if fresh.current.allow_subset_champion_picks
        && fresh.current.timer_phase.as_deref() == Some("BAN_PICK")
        && !fresh
            .extras
            .subset_champion_ids
            .as_ref()
            .is_some_and(|ids| ids.contains(&champion_id))
    {
        return Err("该英雄不在当前可选英雄子集内。".into());
    }
    Ok(())
}

fn validate_choose_champion(
    fresh: &FreshAuxSelection,
    champion_id: u32,
    complete: bool,
) -> Result<AuxChampSelectAction, String> {
    if fresh.current.champion_id.is_some() {
        return validate_bench_swap(fresh, champion_id);
    }
    if !fresh.current.bench_enabled || fresh.current.timer_phase.as_deref() != Some("BAN_PICK") {
        return Err("当前阶段没有可进行的英雄选择操作。".into());
    }
    require_pickable(fresh, champion_id)?;
    if !fresh.current.allow_subset_champion_picks && !fresh.current.bench.contains(&champion_id) {
        return Err("该英雄不属于当前备战席。".into());
    }
    let cell = fresh
        .current
        .local_player_cell_id
        .ok_or("无法确认当前玩家的选人位置。")?;
    let action_id = first_local_pick_action(&fresh.selection, cell)
        .ok_or("当前没有可操作的英雄选择动作，请稍后重试。")?;
    Ok(AuxChampSelectAction::Pick {
        action_id,
        champion_id,
        complete,
    })
}

fn first_local_pick_action(selection: &Value, cell: i64) -> Option<u64> {
    let groups = selection["actions"].as_array()?;
    let current_group = groups.iter().filter_map(Value::as_array).find(|group| {
        group
            .iter()
            .filter(|action| action.is_object())
            .any(|action| action["completed"] != true)
    })?;
    current_group
        .iter()
        .find(|action| {
            action["actorCellId"].as_i64() == Some(cell)
                && action["type"] == "pick"
                && action["completed"] != true
        })?
        .get("id")?
        .as_u64()
}

fn validate_bench_swap(
    fresh: &FreshAuxSelection,
    champion_id: u32,
) -> Result<AuxChampSelectAction, String> {
    let phase = fresh.current.timer_phase.as_deref();
    let phase_allows_swap = phase == Some("FINALIZATION")
        || (phase == Some("BAN_PICK") && fresh.current.allow_subset_champion_picks);
    if !fresh.current.bench_enabled || !phase_allows_swap {
        return Err("当前阶段不允许从备战席换取英雄。".into());
    }
    if fresh.current.champion_id == Some(champion_id) {
        return Err("该英雄已经是当前选择。".into());
    }
    require_pickable(fresh, champion_id)?;
    let in_bench = fresh.current.bench.contains(&champion_id);
    let in_subset = phase == Some("BAN_PICK")
        && fresh.current.allow_subset_champion_picks
        && fresh
            .extras
            .subset_champion_ids
            .as_ref()
            .is_some_and(|ids| ids.contains(&champion_id));
    if !in_bench && !in_subset {
        return Err("该英雄不在当前备战席或可选英雄子集中。".into());
    }
    Ok(AuxChampSelectAction::BenchSwap { champion_id })
}

fn validate_reroll(view: &AuxView) -> Result<(), String> {
    if !view.allow_rerolling || view.rerolls_remaining == 0 {
        return Err("客户端当前未明确支持重随或没有剩余次数。".into());
    }
    Ok(())
}

fn validate_skin(fresh: &FreshAuxSelection, skin_id: u32) -> Result<(), String> {
    if !fresh.current.skin_selector_available || fresh.current.skin_selection_disabled {
        return Err("当前英雄或阶段不可切换皮肤。".into());
    }
    let champion_id = fresh.current.champion_id.ok_or("请先选择当前英雄。")?;
    if fresh.extras.selected_skin_champion_id != Some(champion_id)
        || !fresh.extras.skins.iter().any(|skin| skin.id == skin_id)
    {
        return Err("该皮肤当前不可用，或不属于当前已选择英雄。".into());
    }
    Ok(())
}

fn validate_return_swap(fresh: &FreshAuxSelection, champion_id: u32) -> Result<(), String> {
    if !fresh.current.bench.contains(&champion_id) {
        return Err("原英雄没有进入重随后的备战席。".into());
    }
    validate_bench_swap(fresh, champion_id).map(|_| ())
}

pub(crate) fn reconcile_companions(app: &AppHandle) {
    let _ = app.state::<AuxWindow>().reconcile(app);
    // Opening the external RESG page can outlast OP.GG's fresh draft sample.
    if let Err(_error) = app.state::<OpggWindow>().reconcile(app) {
        #[cfg(test)]
        eprintln!("OP.GG reconciliation: {_error}");
    }
    let _ = app
        .state::<ResgWindow>()
        .reconcile(app, ResgAction::Synchronize);
}

pub(crate) async fn monitor(app: AppHandle) {
    let runtime = app.state::<ClientRuntime>();
    loop {
        runtime.refresh(&app, CompanionDemand::from_app(&app)).await;
        reconcile_companions(&app);
        let interval = runtime
            .state
            .lock()
            .map_or(Duration::from_secs(2), |state| {
                let interval = state.aram.next_poll();
                state
                    .room_flow
                    .next_poll(Instant::now())
                    .map_or(interval, |room| interval.min(room))
            });
        let _ = tokio::time::timeout(interval, runtime.wake.notified()).await;
    }
}

fn publish_changes(app: &AppHandle, before: &Snapshot, after: &Snapshot, revision: u64) {
    for (namespace, name) in PUBLIC_STATES {
        let (Some(Value::Object(before)), Some(Value::Object(after))) = (
            before.fields(namespace, name),
            after.fields(namespace, name),
        ) else {
            continue;
        };
        for (key, value) in after {
            if before.get(&key) == Some(&value) {
                continue;
            }
            publish_property(app, namespace, name, &key, &value, revision);
        }
    }
}

fn publish_live_fields(
    app: &AppHandle,
    namespace: &str,
    fields: Value,
    last: &mut Value,
    revision: &mut u64,
) {
    if fields == *last {
        return;
    }
    *revision += 1;
    for (key, value) in fields.as_object().expect("native state properties") {
        if last.get(key) != Some(value) {
            publish_property(app, namespace, "state", key, value, *revision);
        }
    }
    *last = fields;
}

fn publish_property(
    app: &AppHandle,
    namespace: &str,
    name: &str,
    key: &str,
    value: &Value,
    revision: u64,
) {
    let event = format!("update-state-prop/{namespace}:{name}");
    let args = json!([key, value, { "action": "update", "raw": false, "revision": revision }]);
    for label in ["main", "aux", "opgg"] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = emit(&window, "mobx-utils-main", &event, args.clone());
        }
    }
}

#[cfg(test)]
mod companion_tests {
    use super::*;

    #[test]
    fn draft_pause_ends_on_exit_or_new_game_but_survives_the_initial_game_id() {
        let snapshot = |phase: &str, id: u64| Snapshot {
            known_clients: vec![],
            active: Some(ActiveClient {
                instance: ClientInstance {
                    pid: 42,
                    created_at: 1,
                },
                phase: phase.to_owned(),
                session: json!({"gameData":{"gameId":id}}),
                summoner: Value::Null,
                companions: CompanionTargets::default(),
                aux_activity: AuxActivity::Inactive,
                room: None,
                room_unconfirmed: false,
                shard: None,
            }),
        };
        assert!(!snapshot("ChampSelect", 0).left_draft(&snapshot("ChampSelect", 25)));
        assert!(snapshot("ChampSelect", 25).left_draft(&snapshot("Lobby", 0)));
        assert!(snapshot("ChampSelect", 25).left_draft(&snapshot("ChampSelect", 26)));
        assert!(snapshot("ChampSelect", 25).left_draft(&Snapshot::default()));
    }

    #[test]
    fn aux_alone_reads_selection_only_for_aram_drafts() {
        let demand = CompanionDemand {
            aux: true,
            ..CompanionDemand::default()
        };
        for (phase, mode, expected) in [
            ("ChampSelect", "ARAM", true),
            ("ChampSelect", "KIWI", true),
            ("ChampSelect", "CLASSIC", false),
            ("Lobby", "ARAM", false),
            ("InProgress", "KIWI", false),
        ] {
            let game: GameflowSession = serde_json::from_value(json!({
                "phase": phase,
                "gameData": {"queue": {"gameMode": mode}}
            }))
            .unwrap();
            assert_eq!(
                demand.reads_selection(phase, Some(&game), false),
                expected,
                "{phase} / {mode}"
            );
        }
    }
}

#[cfg(test)]
mod aux_action_tests {
    use super::*;

    #[test]
    fn manual_pick_resolves_only_the_current_local_unfinished_pick_and_keeps_zero_based_ids() {
        let current = json!({"actions":[
            [{"id":8,"actorCellId":1,"type":"pick","completed":false}],
            [{"id":0,"actorCellId":0,"type":"pick","completed":false}]
        ]});
        assert_eq!(first_local_pick_action(&current, 1), Some(8));
        assert_eq!(first_local_pick_action(&current, 0), None);

        let local_turn = json!({"actions":[
            [{"id":0,"actorCellId":0,"type":"pick","completed":false}],
            [{"id":1,"actorCellId":0,"type":"pick","completed":false}]
        ]});
        assert_eq!(first_local_pick_action(&local_turn, 0), Some(0));
        let completed = json!({"actions":[[
            {"id":0,"actorCellId":0,"type":"pick","completed":true}
        ]]});
        assert_eq!(first_local_pick_action(&completed, 0), None);
    }

    #[test]
    fn aux_action_ids_are_typed_and_bounded_before_route_construction() {
        assert_eq!(aux_champion_arg(&json!(57)).unwrap(), 57);
        for value in [json!(0), json!(-1), json!(100_000), json!("57")] {
            assert!(aux_champion_arg(&value).is_err());
        }
        assert_eq!(aux_skin_arg(&json!(1001)).unwrap(), 1001);
        for value in [json!(0), json!(-1), json!(1_000_000), json!("1001")] {
            assert!(aux_skin_arg(&value).is_err());
        }
    }

    #[test]
    fn manual_selection_claim_invalidates_a_queued_automatic_write() {
        let client = Arc::new(LocalClient::fixture().expect("test process handle"));
        let instance = client.instance;
        let flow: GameflowSession = serde_json::from_value(json!({
            "phase":"ChampSelect",
            "gameData":{"queue":{"id":450,"gameMode":"ARAM"}}
        }))
        .unwrap();
        let selection = json!({
            "queueId":450,"gameId":0,"localPlayerCellId":0,
            "myTeam":[{"cellId":0,"championId":22}]
        });
        let activity =
            AuxActivity::project("ChampSelect", Some(&flow), None, None, Some(&selection));
        let expected = activity.view().cloned().expect("projected ARAM draft");
        let mut state = RuntimeState {
            generation: 7,
            snapshot: Snapshot {
                active: Some(ActiveClient {
                    instance,
                    phase: "ChampSelect".into(),
                    session: Value::Null,
                    summoner: Value::Null,
                    companions: CompanionTargets::default(),
                    aux_activity: activity,
                    room: None,
                    room_unconfirmed: false,
                    shard: None,
                }),
                ..Snapshot::default()
            },
            art_client: Some(client),
            ..RuntimeState::default()
        };

        assert!(state.scheduled_selection_generation_is_current(7));
        assert_eq!(
            state.claim_manual_aux_selection(7, instance, &expected),
            Some(8)
        );
        assert!(!state.scheduled_selection_generation_is_current(7));
        assert!(state.scheduled_selection_generation_is_current(8));
        assert_eq!(
            state.claim_manual_aux_selection(7, instance, &expected),
            None
        );
    }
}
