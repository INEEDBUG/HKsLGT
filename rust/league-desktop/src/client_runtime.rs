//! Disconnect-safe snapshots from the local League client for the existing renderer.

use crate::{
    aram_automation::{AramOptions, AramSchedule, ScheduledStep, SelectionScope},
    client_discovery::{discover_clients, ClientInstance, LocalClient},
    desktop::{aux_window::AuxWindow, emit},
    lcu::{
        game_art::{GameArt, GameArtPath},
        player_search::PlayerSearchShard,
        player_tab_read::PlayerTabRead,
        GameflowSession, LcuError, LcuTransport,
    },
    opgg_window::{self, OpggTarget, OpggWindow},
    resg_window::{ResgAction, ResgWindow},
};
pub(crate) use maxgamestudio_league_core::shell_preferences::LEAGUE_CLIENT as CLIENT;
use serde_json::{json, Map, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
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
    shard: Option<PlayerSearchShard>,
}

#[derive(Default, PartialEq)]
struct CompanionTargets {
    aux: Option<SelectionScope>,
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
            aux: app.state::<AuxWindow>().auto_enabled(),
            opgg: app.state::<OpggWindow>().enabled(),
            resg: app.state::<ResgWindow>().enabled(),
        }
    }

    fn reads_selection(self, game: &GameflowSession, automate: bool) -> bool {
        game.phase == "ChampSelect"
            && (((automate || self.aux || self.opgg) && game.is_aram())
                || (self.resg && game.is_hextech_aram()))
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
        let aux = selection
            .filter(|_| self.aux)
            .and_then(|select| SelectionScope::from_sample(instance, game, select));
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
        CompanionTargets { aux, opgg, resg }
    }
}

struct Sample {
    client: ActiveClient,
    selection: Option<(GameflowSession, Value)>,
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

const PUBLIC_STATES: [(&str, &str); 6] = [
    (CLIENT_DISCOVERY, "state"),
    (CLIENT, "state"),
    (CLIENT, "gameflow"),
    (CLIENT, "summoner"),
    (SGP, "state"),
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
    fn history_session(&self) -> Option<HistorySession> {
        let active = self.snapshot.active.as_ref()?;
        Some(HistorySession {
            instance: active.instance,
            generation: self.generation,
            player: active.summoner["puuid"].as_str()?.to_owned(),
            shard: active.shard.clone()?,
        })
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
    ) {
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
        })
    }

    pub(crate) fn fields(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Option<(u64, Value)>, &'static str> {
        let state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
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

    pub(crate) fn aux_draft(&self) -> Option<(SelectionScope, Arc<LocalClient>)> {
        let state = self.state.lock().ok()?;
        let active = state.snapshot.active.as_ref()?;
        let client = state.art_client.as_ref()?;
        if client.instance != active.instance || !client.alive() {
            return None;
        }
        Some((active.companions.aux?, Arc::clone(client)))
    }

    pub(crate) fn selection_ended(&self) -> bool {
        self.state.lock().is_ok_and(|state| {
            state.manually_disconnected
                || state.snapshot.active.as_ref().is_some_and(|active| {
                    active.phase != "ChampSelect"
                        || active.session["gameData"]["queue"]["gameMode"]
                            .as_str()
                            .is_some_and(|mode| !matches!(mode, "ARAM" | "KIWI"))
                })
        })
    }

    #[cfg(test)]
    pub(crate) fn apply_companion_fixture(
        &self,
        app: &AppHandle,
        session: GameflowSession,
        selection: Value,
    ) {
        const PLAYER: &str = "fixture-player";
        let client = Arc::new(LocalClient::fixture().expect("retain current fixture process"));
        let instance = client.instance;
        let demand = CompanionDemand::from_app(app);
        let selection = demand
            .reads_selection(&session, false)
            .then_some(&selection);
        let companions = demand.targets(instance, Some(&session), selection, Some(PLAYER), None);
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

    pub(crate) fn replace_aram_options(&self, options: AramOptions) -> Result<(), &'static str> {
        let mut state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
        state.aram_options = options;
        state.aram.cancel();
        state.generation += 1;
        self.refresh_soon();
        Ok(())
    }

    pub(crate) fn aram_paused(&self) -> Result<(u64, bool), &'static str> {
        let state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
        Ok((state.revision, state.aram_paused))
    }

    pub(crate) fn pause_aram(&self, app: &AppHandle, paused: bool) -> Result<(), &'static str> {
        let mut state = self.state.lock().map_err(|_| "客户端状态不可用。")?;
        if state.aram_paused != paused {
            state.aram_paused = paused;
            state.aram.cancel();
            state.generation += 1;
            state.revision += 1;
            if let Some(main) = app.get_webview_window("main") {
                let _ = emit(
                    &main,
                    "mobx-utils-main",
                    "update-state-prop/auto-select-main:state",
                    json!(["temporarilyDisabled", paused, {"action":"update","raw":false,"revision":state.revision}]),
                );
            }
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
        state.aram.reset();
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
        let summoner = self.transport.read_current_summoner(&target).await?;
        let session = self
            .transport
            .read_gameflow_session(&target)
            .await?
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
        let should_read = session
            .as_ref()
            .is_some_and(|game| demand.reads_selection(game, automate));
        let selection = if should_read {
            // The optional reads should leave time to publish connected state.
            tokio::time::timeout(
                Duration::from_millis(750),
                self.transport.read_champ_select(&target),
            )
            .await
            .ok()
            .and_then(Result::ok)
        } else {
            None
        };
        let companions = demand.targets(
            client.instance,
            session.as_ref(),
            selection.as_ref(),
            summoner.as_ref().map(|player| player.puuid.as_str()),
            previous_chart,
        );
        Ok(client.alive().then(|| {
            let client = ActiveClient {
                instance: client.instance,
                phase,
                session: json!(session),
                summoner: json!(summoner),
                companions,
                shard,
            };
            let selection = if automate {
                session.zip(selection)
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
        {
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
            state.apply_snapshot(app, snapshot, candidate.cloned());
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
        }
        if let (Some(client), Some(options), Some((flow, select))) = (candidate, options, selection)
        {
            self.automate_aram(generation, client, &flow, &select, &options)
                .await;
        }
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
        let authorized = self.state.lock().is_ok_and(|mut state| {
            state.generation == generation && !state.aram_paused && state.aram.mark_attempted(due)
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
            .map_or(Duration::from_secs(2), |state| state.aram.next_poll());
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
                demand.reads_selection(&game, false),
                expected,
                "{phase} / {mode}"
            );
        }
    }
}
