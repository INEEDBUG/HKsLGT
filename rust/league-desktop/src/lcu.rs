//! Narrow HTTPS reads from an already verified local League client.

use base64::Engine as _;
use crate::aram_automation::{AramOptions, ScheduledStep, SelectionLists};
use maxgamestudio_league_core::{auto_select::AutoSelectAction, resg::select_resg_champion};
use reqwest::{redirect::Policy, Client, RequestBuilder, StatusCode};
use rustls::pki_types::{pem::PemObject, CertificateDer};
use rustls::{client::Resumption, crypto::ring, ClientConfig, RootCertStore};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fmt, future::Future, num::NonZeroU16, sync::Arc, time::Duration};

// Byte-for-byte match to the CA bundled by League Akari and Riot's League Director.
// https://github.com/RiotGames/leaguedirector/blob/main/resources/riotgames.pem
// SHA-256 of the DER: CA8C9D325B4CDC464C6C94A585C85E91EC23D40BA5BF3AE2822B951A4A504EA3
const RIOT_ROOT_CA_PEM: &[u8] = b"-----BEGIN CERTIFICATE-----
MIIEIDCCAwgCCQDJC+QAdVx4UDANBgkqhkiG9w0BAQUFADCB0TELMAkGA1UEBhMC
VVMxEzARBgNVBAgTCkNhbGlmb3JuaWExFTATBgNVBAcTDFNhbnRhIE1vbmljYTET
MBEGA1UEChMKUmlvdCBHYW1lczEdMBsGA1UECxMUTG9MIEdhbWUgRW5naW5lZXJp
bmcxMzAxBgNVBAMTKkxvTCBHYW1lIEVuZ2luZWVyaW5nIENlcnRpZmljYXRlIEF1
dGhvcml0eTEtMCsGCSqGSIb3DQEJARYeZ2FtZXRlY2hub2xvZ2llc0ByaW90Z2Ft
ZXMuY29tMB4XDTEzMTIwNDAwNDgzOVoXDTQzMTEyNzAwNDgzOVowgdExCzAJBgNV
BAYTAlVTMRMwEQYDVQQIEwpDYWxpZm9ybmlhMRUwEwYDVQQHEwxTYW50YSBNb25p
Y2ExEzARBgNVBAoTClJpb3QgR2FtZXMxHTAbBgNVBAsTFExvTCBHYW1lIEVuZ2lu
ZWVyaW5nMTMwMQYDVQQDEypMb0wgR2FtZSBFbmdpbmVlcmluZyBDZXJ0aWZpY2F0
ZSBBdXRob3JpdHkxLTArBgkqhkiG9w0BCQEWHmdhbWV0ZWNobm9sb2dpZXNAcmlv
dGdhbWVzLmNvbTCCASIwDQYJKoZIhvcNAQEBBQADggEPADCCAQoCggEBAKoJemF/
6PNG3GRJGbjzImTdOo1OJRDI7noRwJgDqkaJFkwv0X8aPUGbZSUzUO23cQcCgpYj
21ygzKu5dtCN2EcQVVpNtyPuM2V4eEGr1woodzALtufL3Nlyh6g5jKKuDIfeUBHv
JNyQf2h3Uha16lnrXmz9o9wsX/jf+jUAljBJqsMeACOpXfuZy+YKUCxSPOZaYTLC
y+0GQfiT431pJHBQlrXAUwzOmaJPQ7M6mLfsnpHibSkxUfMfHROaYCZ/sbWKl3lr
ZA9DbwaKKfS1Iw0ucAeDudyuqb4JntGU/W0aboKA0c3YB02mxAM4oDnqseuKV/CX
8SQAiaXnYotuNXMCAwEAATANBgkqhkiG9w0BAQUFAAOCAQEAf3KPmddqEqqC8iLs
lcd0euC4F5+USp9YsrZ3WuOzHqVxTtX3hR1scdlDXNvrsebQZUqwGdZGMS16ln3k
WObw7BbhU89tDNCN7Lt/IjT4MGRYRE+TmRc5EeIXxHkQ78bQqbmAI3GsW+7kJsoO
q3DdeE+M+BUJrhWorsAQCgUyZO166SAtKXKLIcxa+ddC49NvMQPJyzm3V+2b1roP
SvD2WV8gRYUnGmy/N0+u6ANq5EsbhZ548zZc+BI4upsWChTLyxt2RxR7+uGlS1+5
EcGfKZ+g024k/J32XP4hdho7WYAS2xMiV83CfLR/MNi8oSMaVQTdKD8cpgiWJk3L
XWehWA==
-----END CERTIFICATE-----";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
const AUX_SELECTION_READ_TIMEOUT: Duration = Duration::from_millis(500);

#[path = "player_search.rs"]
pub(crate) mod player_search;

#[path = "game_data.rs"]
pub(crate) mod game_data;

#[path = "game_art.rs"]
pub(crate) mod game_art;

#[path = "aux_activity.rs"]
pub(crate) mod aux_activity;

#[path = "player_tab_read.rs"]
pub(crate) mod player_tab_read;

#[path = "lcu_post_game.rs"]
pub(crate) mod post_game;

/// Borrow the port and token that trusted desktop discovery verified for the same client generation.
pub(crate) struct LcuTarget<'a> {
    port: NonZeroU16,
    token: &'a str,
}

pub(crate) type LcuEventStream = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

impl<'a> LcuTarget<'a> {
    pub(crate) fn new(port: u16, token: &'a str) -> Result<Self, LcuError> {
        let port = NonZeroU16::new(port).ok_or(LcuError::InvalidTarget)?;
        if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(LcuError::InvalidTarget);
        }

        Ok(Self { port, token })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LcuError {
    InvalidTarget,
    ClientConfiguration,
    Timeout,
    Transport,
    AuthenticationRejected,
    UnexpectedStatus(u16),
    ResponseTooLarge,
    InvalidPayload,
}

impl fmt::Display for LcuError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidTarget => "Invalid local League client endpoint or token",
            Self::ClientConfiguration => "Could not initialize League client HTTPS",
            Self::Timeout => "League client read timed out",
            Self::Transport => "League client HTTPS connection or read failed",
            Self::AuthenticationRejected => "League client rejected authentication",
            Self::UnexpectedStatus(status) => {
                return write!(formatter, "League client returned HTTP {status}")
            }
            Self::ResponseTooLarge => "League client response exceeded the endpoint size limit",
            Self::InvalidPayload => "Invalid League client response",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for LcuError {}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CurrentSummoner {
    pub(crate) puuid: String,
    pub(crate) game_name: Option<String>,
    pub(crate) tag_line: Option<String>,
    pub(crate) display_name: Option<String>,
    pub(crate) summoner_id: Option<i64>,
    pub(crate) profile_icon_id: Option<i32>,
    pub(crate) summoner_level: Option<i32>,
    pub(crate) xp_since_last_level: Option<i32>,
    pub(crate) xp_until_next_level: Option<i32>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GameflowSession {
    pub(crate) phase: String,
    pub(crate) game_data: Option<GameflowGameData>,
    #[serde(skip_serializing)]
    map: Option<GameflowMap>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GameflowMap {
    game_mode: Option<String>,
    name: Option<String>,
    assets: Option<Value>,
}

impl GameflowSession {
    pub(crate) fn aux_game_mode(&self) -> Option<&str> {
        self.game_mode()
            .filter(|mode| !mode.is_empty())
            .or_else(|| self.map.as_ref()?.game_mode.as_deref())
            .filter(|mode| !mode.is_empty())
    }

    pub(crate) fn is_aram(&self) -> bool {
        matches!(self.game_mode(), Some("ARAM" | "KIWI"))
    }

    pub(crate) fn is_hextech_aram(&self) -> bool {
        self.game_mode() == Some("KIWI")
    }

    pub(crate) fn queue(&self) -> Option<&GameflowQueue> {
        self.game_data.as_ref()?.queue.as_ref()
    }

    pub(crate) fn aux_queue_name(&self) -> Option<&str> {
        self.queue()?
            .name
            .as_deref()
            .filter(|name| !name.is_empty())
    }

    pub(crate) fn aux_map_name(&self) -> Option<&str> {
        self.map
            .as_ref()?
            .name
            .as_deref()
            .filter(|name| !name.is_empty())
    }

    pub(crate) fn aux_mode_icon_path(&self) -> Option<String> {
        let path = self
            .map
            .as_ref()?
            .assets
            .as_ref()?
            .get("game-select-icon-hover")?
            .as_str()?;
        game_art::GameArtPath::parse_mode_icon(path).map(|path| path.path().to_owned())
    }

    fn game_mode(&self) -> Option<&str> {
        self.queue()?.game_mode.as_deref()
    }

    pub(crate) fn resg_champion(&self, selection: &Value, puuid: Option<&str>) -> Option<u32> {
        let game = self.game_data.as_ref()?;
        let private_sample = json!({"gameData": {
            "queue": {"gameMode": self.game_mode()},
            "playerChampionSelections": game.player_champion_selections,
            "teamOne": game.team_one,
            "teamTwo": game.team_two,
        }});
        select_resg_champion(Some(&self.phase), &private_sample, selection, puuid)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GameflowGameData {
    pub(crate) game_id: Option<u64>,
    pub(crate) is_custom_game: Option<bool>,
    pub(crate) queue: Option<GameflowQueue>,
    // Keep rosters inside the native sampler. The renderer only needs the local chart target.
    #[serde(default, skip_serializing)]
    player_champion_selections: Value,
    #[serde(default, skip_serializing)]
    team_one: Value,
    #[serde(default, skip_serializing)]
    team_two: Value,
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GameflowQueue {
    pub(crate) id: Option<i32>,
    pub(crate) game_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(rename = "type")]
    pub(crate) queue_type: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IncomingChampionTrade {
    pub(crate) id: u32,
    pub(crate) champion: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoomAction {
    StartMatchmaking,
    CancelMatchmaking,
    AcceptReadyCheck,
    DeclineReadyCheck,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuxChampSelectAction {
    Pick {
        action_id: u64,
        champion_id: u32,
        complete: bool,
    },
    BenchSwap {
        champion_id: u32,
    },
    Reroll,
    SelectSkin {
        skin_id: u32,
    },
    Leave,
}

fn incoming_champion_trade(value: &Value) -> Option<IncomingChampionTrade> {
    if value["type"] != "CHAMPION"
        || value["state"] != "RECEIVED"
        || value["initiatedByLocalPlayer"] != false
    {
        return None;
    }
    let id = u32::try_from(value["id"].as_u64()?).ok()?;
    let champion = i32::try_from(value["requesterChampionId"].as_u64()?).ok()?;
    (1..100_000)
        .contains(&champion)
        .then_some(IncomingChampionTrade { id, champion })
}

#[derive(Clone, Copy)]
enum ReadEndpoint {
    Phase,
    CurrentSummoner,
    GameflowSession,
    ChampSelect,
    CurrentChampion,
    AuxLobby,
    AuxMatchmaking,
    AuxReadyCheck,
    IncomingChampionTrade,
    ChampionGrid,
    Pickable,
    Bannable,
    Subset,
    SkinSelectorInfo,
    SkinCarouselSkins,
    GameData(game_data::GameDataEndpoint),
}

impl ReadEndpoint {
    fn path(self) -> &'static str {
        match self {
            Self::Phase => "/lol-gameflow/v1/gameflow-phase",
            Self::CurrentSummoner => "/lol-summoner/v1/current-summoner",
            Self::GameflowSession => "/lol-gameflow/v1/session",
            Self::ChampSelect => "/lol-champ-select/v1/session",
            Self::CurrentChampion => "/lol-champ-select/v1/current-champion",
            Self::AuxLobby => "/lol-lobby/v2/lobby",
            Self::AuxMatchmaking => "/lol-matchmaking/v1/search",
            Self::AuxReadyCheck => "/lol-matchmaking/v1/ready-check",
            Self::IncomingChampionTrade => "/lol-champ-select/v1/ongoing-champion-swap",
            Self::ChampionGrid => "/lol-champ-select/v1/all-grid-champions",
            Self::Pickable => "/lol-champ-select/v1/pickable-champion-ids",
            Self::Bannable => "/lol-champ-select/v1/bannable-champion-ids",
            Self::Subset => "/lol-lobby-team-builder/champ-select/v1/subset-champion-list",
            Self::SkinSelectorInfo => "/lol-champ-select/v1/skin-selector-info",
            Self::SkinCarouselSkins => "/lol-champ-select/v1/skin-carousel-skins",
            Self::GameData(endpoint) => endpoint.path(),
        }
    }

    fn body_limit(self) -> usize {
        match self {
            Self::Phase | Self::CurrentChampion => 1024,
            Self::CurrentSummoner => 16 * 1024,
            Self::GameflowSession | Self::ChampSelect => 256 * 1024,
            Self::AuxLobby => 128 * 1024,
            Self::AuxMatchmaking => 32 * 1024,
            Self::AuxReadyCheck => 8 * 1024,
            Self::IncomingChampionTrade => 8 * 1024,
            Self::ChampionGrid => 512 * 1024,
            Self::Pickable | Self::Bannable | Self::Subset => 16 * 1024,
            Self::SkinSelectorInfo => 8 * 1024,
            Self::SkinCarouselSkins => 64 * 1024,
            Self::GameData(endpoint) => endpoint.body_limit(),
        }
    }
}

fn positive_u32(value: &Value) -> Option<u32> {
    u32::try_from(value.as_u64()?).ok().filter(|&id| id > 0)
}

fn valid_ids(value: Vec<u32>) -> Vec<u32> {
    let mut ids = Vec::new();
    for id in value.into_iter().filter(|&id| (1..100_000).contains(&id)) {
        if !ids.contains(&id) {
            ids.push(id);
        }
        if ids.len() == 200 {
            break;
        }
    }
    ids
}

fn skin_options(rows: &Value) -> Vec<aux_activity::AuxSkin> {
    let mut result = Vec::new();
    let Some(rows) = rows.as_array() else {
        return result;
    };
    for row in rows {
        push_skin_option(&mut result, row, false);
        if let Some(children) = row["childSkins"].as_array() {
            for child in children {
                push_skin_option(&mut result, child, true);
                if result.len() >= 100 {
                    return result;
                }
            }
        }
        if result.len() >= 100 {
            break;
        }
    }
    result
}

fn push_skin_option(result: &mut Vec<aux_activity::AuxSkin>, row: &Value, chroma: bool) {
    if row["unlocked"] != true || row["disabled"] == true {
        return;
    }
    let Some(id) = positive_u32(&row["id"]) else {
        return;
    };
    if result.iter().any(|skin| skin.id == id) {
        return;
    }
    let name = row["name"]
        .as_str()
        .filter(|name| !name.is_empty() && name.len() <= 160)
        .map(str::to_owned)
        .unwrap_or_else(|| id.to_string());
    let preview_path = if chroma {
        row["chromaPreviewPath"]
            .as_str()
            .or_else(|| row["splashPath"].as_str())
    } else {
        row["splashPath"].as_str()
    }
    .and_then(game_art::GameArtPath::parse)
    .map(|path| path.path().to_owned());
    result.push(aux_activity::AuxSkin {
        id,
        name,
        is_chroma: chroma,
        preview_path,
    });
}

#[derive(Clone)]
pub(crate) struct LcuTransport {
    client: Client,
    event_tls: Arc<ClientConfig>,
}

impl LcuTransport {
    pub(crate) fn new() -> Result<Self, LcuError> {
        let root = CertificateDer::from_pem_slice(RIOT_ROOT_CA_PEM)
            .map_err(|_| LcuError::ClientConfiguration)?;
        Self::with_root(root)
    }

    pub(super) fn with_root(root: CertificateDer<'static>) -> Result<Self, LcuError> {
        let mut roots = RootCertStore::empty();
        roots.add(root).map_err(|_| LcuError::ClientConfiguration)?;
        let mut tls = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|_| LcuError::ClientConfiguration)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        // Each local read authenticates its current peer across client restarts.
        tls.resumption = Resumption::disabled();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let event_tls = Arc::new(tls.clone());

        let client = Client::builder()
            .tls_backend_preconfigured(tls)
            .https_only(true)
            .http1_only()
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .pool_max_idle_per_host(0)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| LcuError::ClientConfiguration)?;

        Ok(Self { client, event_tls })
    }

    pub(crate) async fn connect_ready_events(
        &self,
        target: &LcuTarget<'_>,
    ) -> Result<LcuEventStream, LcuError> {
        use tokio_tungstenite::{
            connect_async_tls_with_config,
            tungstenite::{client::IntoClientRequest, http::header::{HeaderValue, AUTHORIZATION}},
            Connector,
        };

        let mut request = format!("wss://127.0.0.1:{}/", target.port)
            .into_client_request()
            .map_err(|_| LcuError::InvalidTarget)?;
        let authorization = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(
            format!("riot:{}", target.token)
        ));
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&authorization).map_err(|_| LcuError::InvalidTarget)?,
        );
        let (stream, _response) = tokio::time::timeout(
            Duration::from_secs(3),
            connect_async_tls_with_config(
                request,
                None,
                true,
                Some(Connector::Rustls(Arc::clone(&self.event_tls))),
            ),
        )
        .await
        .map_err(|_| LcuError::Timeout)?
        .map_err(|error| {
            if matches!(error, tokio_tungstenite::tungstenite::Error::Tls(_)) {
                LcuError::AuthenticationRejected
            } else {
                LcuError::Transport
            }
        })?;
        Ok(stream)
    }

    pub(crate) async fn read_phase(
        &self,
        target: &LcuTarget<'_>,
    ) -> Result<Option<String>, LcuError> {
        self.read(target, ReadEndpoint::Phase).await
    }

    pub(crate) async fn read_window_zoom(&self, target: &LcuTarget<'_>) -> Result<f64, LcuError> {
        let zoom = Self::read_json(self.local_json_get(target, "/riotclient/zoom-scale"), 64)
            .await?
            .unwrap_or(1.0);
        (0.15..=4.0)
            .contains(&zoom)
            .then_some(zoom)
            .ok_or(LcuError::InvalidPayload)
    }

    pub(crate) async fn read_current_summoner(
        &self,
        target: &LcuTarget<'_>,
    ) -> Result<Option<CurrentSummoner>, LcuError> {
        self.read(target, ReadEndpoint::CurrentSummoner).await
    }

    pub(crate) async fn read_gameflow_session(
        &self,
        target: &LcuTarget<'_>,
    ) -> Result<Option<GameflowSession>, LcuError> {
        self.read(target, ReadEndpoint::GameflowSession).await
    }

    pub(crate) async fn read_champ_select(
        &self,
        target: &LcuTarget<'_>,
    ) -> Result<Value, LcuError> {
        Ok(self
            .read(target, ReadEndpoint::ChampSelect)
            .await?
            .unwrap_or(Value::Null))
    }

    pub(crate) async fn read_aux_current_champion(
        &self,
        target: &LcuTarget<'_>,
    ) -> Result<Option<u32>, LcuError> {
        Ok(self
            .read::<u32>(target, ReadEndpoint::CurrentChampion)
            .await?
            .filter(|&id| id < 100_000))
    }

    pub(crate) async fn read_aux_lobby(
        &self,
        target: &LcuTarget<'_>,
    ) -> Result<Option<Value>, LcuError> {
        self.read(target, ReadEndpoint::AuxLobby).await
    }

    pub(crate) async fn read_aux_matchmaking(
        &self,
        target: &LcuTarget<'_>,
    ) -> Result<Option<Value>, LcuError> {
        self.read(target, ReadEndpoint::AuxMatchmaking).await
    }

    pub(crate) async fn read_aux_ready_check(
        &self,
        target: &LcuTarget<'_>,
    ) -> Result<Option<Value>, LcuError> {
        self.read(target, ReadEndpoint::AuxReadyCheck).await
    }

    pub(crate) async fn read_champion_trade(
        &self,
        target: &LcuTarget<'_>,
    ) -> Result<Option<IncomingChampionTrade>, LcuError> {
        Ok(self
            .read::<Value>(target, ReadEndpoint::IncomingChampionTrade)
            .await?
            .as_ref()
            .and_then(incoming_champion_trade))
    }

    pub(crate) async fn read_selection_lists(
        &self,
        target: &LcuTarget<'_>,
        options: &AramOptions,
        has_subset: bool,
    ) -> Result<SelectionLists, LcuError> {
        let mut lists = SelectionLists::default();
        if options.want_pick() || options.want_ban() {
            lists.grid = self
                .read(target, ReadEndpoint::ChampionGrid)
                .await?
                .unwrap_or(Value::Null);
        }
        if options.want_pick() {
            lists.pickable = self
                .read(target, ReadEndpoint::Pickable)
                .await?
                .unwrap_or(Value::Null);
        }
        if has_subset && (options.want_pick() || options.want_trade()) {
            lists.subset = self
                .read(target, ReadEndpoint::Subset)
                .await?
                .unwrap_or(Value::Null);
        }
        if options.want_ban() {
            lists.bannable = self
                .read(target, ReadEndpoint::Bannable)
                .await?
                .unwrap_or(Value::Null);
        }
        Ok(lists)
    }

    pub(crate) async fn read_aux_selection_extras(
        &self,
        target: &LcuTarget<'_>,
        selection: &Value,
    ) -> Result<aux_activity::AuxSelectionExtras, LcuError> {
        let pickable = self.read(target, ReadEndpoint::Pickable);
        let subset = async {
            if selection["allowSubsetChampionPicks"] == true {
                self.read(target, ReadEndpoint::Subset).await
            } else {
                Ok(None)
            }
        };
        let skin_info = self.read::<Value>(target, ReadEndpoint::SkinSelectorInfo);
        let skin_rows = self.read::<Value>(target, ReadEndpoint::SkinCarouselSkins);
        Ok(collect_aux_selection_extras(
            pickable,
            subset,
            skin_info,
            skin_rows,
            AUX_SELECTION_READ_TIMEOUT,
        )
        .await)
    }

    pub(crate) async fn apply_selection(
        &self,
        target: &LcuTarget<'_>,
        step: ScheduledStep,
    ) -> Result<(), LcuError> {
        let response = self
            .selection_request(target, step)
            .send()
            .await
            .map_err(request_error)?;
        if !response.status().is_success() {
            return Err(LcuError::UnexpectedStatus(response.status().as_u16()));
        }
        Ok(())
    }

    pub(crate) async fn apply_room_action(
        &self,
        target: &LcuTarget<'_>,
        action: RoomAction,
    ) -> Result<(), LcuError> {
        self.send_action(self.room_action_request(target, action))
            .await
    }

    fn room_action_request(&self, target: &LcuTarget<'_>, action: RoomAction) -> RequestBuilder {
        let address = format!("https://127.0.0.1:{}", target.port);
        let request = match action {
            RoomAction::StartMatchmaking => self
                .client
                .post(format!("{address}/lol-lobby/v2/lobby/matchmaking/search")),
            RoomAction::CancelMatchmaking => self
                .client
                .delete(format!("{address}/lol-lobby/v2/lobby/matchmaking/search")),
            RoomAction::AcceptReadyCheck => self
                .client
                .post(format!("{address}/lol-matchmaking/v1/ready-check/accept")),
            RoomAction::DeclineReadyCheck => self
                .client
                .post(format!("{address}/lol-matchmaking/v1/ready-check/decline")),
        }
        .basic_auth("riot", Some(target.token))
        .timeout(Duration::from_secs(1));
        request
    }

    pub(crate) async fn apply_aux_champ_select_action(
        &self,
        target: &LcuTarget<'_>,
        action: AuxChampSelectAction,
    ) -> Result<(), LcuError> {
        self.send_action(self.aux_champ_select_request(target, action))
            .await
    }

    fn aux_champ_select_request(
        &self,
        target: &LcuTarget<'_>,
        action: AuxChampSelectAction,
    ) -> RequestBuilder {
        let address = format!("https://127.0.0.1:{}", target.port);
        let patch_action = |action_id, body: Value| {
            self.client
                .patch(format!(
                    "{address}/lol-champ-select/v1/session/actions/{action_id}"
                ))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_string())
        };
        let request = match action {
            AuxChampSelectAction::Pick {
                action_id,
                champion_id,
                complete,
            } => {
                let body =
                    json!({"championId": champion_id, "type": "pick", "completed": complete});
                patch_action(action_id, body)
            }
            AuxChampSelectAction::BenchSwap { champion_id } => self.client.post(format!(
                "{address}/lol-champ-select/v1/session/bench/swap/{champion_id}"
            )),
            AuxChampSelectAction::Reroll => self.client.post(format!(
                "{address}/lol-champ-select/v1/session/my-selection/reroll"
            )),
            AuxChampSelectAction::SelectSkin { skin_id } => self
                .client
                .patch(format!(
                    "{address}/lol-champ-select/v1/session/my-selection"
                ))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(json!({"selectedSkinId":skin_id}).to_string()),
            AuxChampSelectAction::Leave => {
                let mut url =
                    reqwest::Url::parse(&format!("{address}/lol-login/v1/session/invoke"))
                        .expect("constructed from a validated loopback port");
                url.query_pairs_mut()
                    .append_pair("destination", "lcdsServiceProxy")
                    .append_pair("method", "call")
                    .append_pair("args", r#"["", "teambuilder-draft", "quitV2", ""]"#);
                self.client
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(json!({"data":["", "teambuilder-draft", "quitV2", ""]}).to_string())
            }
        };
        request
            .basic_auth("riot", Some(target.token))
            .timeout(Duration::from_secs(1))
    }

    async fn send_action(&self, request: RequestBuilder) -> Result<(), LcuError> {
        let response = request.send().await.map_err(request_error)?;
        if !response.status().is_success() {
            return Err(LcuError::UnexpectedStatus(response.status().as_u16()));
        }
        Ok(())
    }

    fn selection_request(&self, target: &LcuTarget<'_>, step: ScheduledStep) -> RequestBuilder {
        let address = format!("https://127.0.0.1:{}", target.port);
        let patch = |id: i64, body: Value| {
            self.client
                .patch(format!(
                    "{address}/lol-champ-select/v1/session/actions/{id}"
                ))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_string())
        };
        match step {
            ScheduledStep::Draft { action, champion } => match action {
                AutoSelectAction::Intent { action_id } => {
                    patch(action_id, json!({"championId": champion}))
                }
                AutoSelectAction::Pick {
                    action_id,
                    completed,
                } => patch(
                    action_id,
                    json!({"championId": champion, "type": "pick", "completed": completed}),
                ),
                AutoSelectAction::Ban {
                    action_id,
                    completed,
                } => patch(
                    action_id,
                    json!({"championId": champion, "type": "ban", "completed": completed}),
                ),
                AutoSelectAction::BenchSwap => self.client.post(format!(
                    "{address}/lol-champ-select/v1/session/bench/swap/{champion}"
                )),
            },
            ScheduledStep::Trade { offer, accept, .. } => {
                let id = offer.id;
                let response = if accept { "accept" } else { "decline" };
                self.client.post(format!(
                    "{address}/lol-champ-select/v1/session/champion-swaps/{id}/{response}"
                ))
            }
        }
        .basic_auth("riot", Some(target.token))
        .timeout(Duration::from_secs(1))
    }

    fn request(&self, target: &LcuTarget<'_>, read: ReadEndpoint) -> RequestBuilder {
        self.local_json_get(target, read.path())
    }

    fn local_json_get(&self, target: &LcuTarget<'_>, path: &str) -> RequestBuilder {
        self.client
            .get(format!("https://127.0.0.1:{}{path}", target.port))
            .header(reqwest::header::ACCEPT, "application/json")
            .basic_auth("riot", Some(target.token))
    }

    async fn read<T: DeserializeOwned>(
        &self,
        target: &LcuTarget<'_>,
        read: ReadEndpoint,
    ) -> Result<Option<T>, LcuError> {
        Self::read_json(self.request(target, read), read.body_limit()).await
    }

    async fn read_json<T: DeserializeOwned>(
        request: RequestBuilder,
        limit: usize,
    ) -> Result<Option<T>, LcuError> {
        let mut response = request.send().await.map_err(request_error)?;
        if !response_has_body(response.status())? {
            return Ok(None);
        }
        let declared_length = response.content_length().unwrap_or_default();
        if declared_length > limit as u64 {
            return Err(LcuError::ResponseTooLarge);
        }
        let mut body = Vec::with_capacity(declared_length as usize);
        while let Some(chunk) = response.chunk().await.map_err(request_error)? {
            append_chunk(&mut body, &chunk, limit)?;
        }
        decode_body(&body)
    }
}

async fn optional_aux_read<F, T>(future: F, timeout: Duration) -> Option<T>
where
    F: Future<Output = Result<Option<T>, LcuError>>,
{
    tokio::time::timeout(timeout, future).await.ok()?.ok()?
}

async fn collect_aux_selection_extras<P, S, I, R>(
    pickable: P,
    subset: S,
    skin_info: I,
    skin_rows: R,
    timeout: Duration,
) -> aux_activity::AuxSelectionExtras
where
    P: Future<Output = Result<Option<Vec<u32>>, LcuError>>,
    S: Future<Output = Result<Option<Vec<u32>>, LcuError>>,
    I: Future<Output = Result<Option<Value>, LcuError>>,
    R: Future<Output = Result<Option<Value>, LcuError>>,
{
    let (pickable, subset, skin_info, skin_rows) = futures_util::future::join4(
        optional_aux_read(pickable, timeout),
        optional_aux_read(subset, timeout),
        optional_aux_read(skin_info, timeout),
        optional_aux_read(skin_rows, timeout),
    )
    .await;
    let info = skin_info.unwrap_or(Value::Null);
    let rows = skin_rows.unwrap_or(Value::Null);
    aux_activity::AuxSelectionExtras {
        pickable_champion_ids: pickable.map(valid_ids),
        subset_champion_ids: subset.map(valid_ids),
        selected_skin_id: positive_u32(&info["selectedSkinId"]),
        selected_skin_champion_id: positive_u32(&info["selectedChampionId"]),
        skin_selector_available: info["showSkinSelector"] != false
            && rows.as_array().is_some_and(|rows| !rows.is_empty()),
        skin_selection_disabled: info["skinSelectionDisabled"] == true,
        skins: skin_options(&rows),
    }
}

fn request_error(error: reqwest::Error) -> LcuError {
    if error.is_timeout() {
        LcuError::Timeout
    } else {
        LcuError::Transport
    }
}

fn response_has_body(status: StatusCode) -> Result<bool, LcuError> {
    match status {
        StatusCode::OK => Ok(true),
        StatusCode::NO_CONTENT | StatusCode::NOT_FOUND => Ok(false),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(LcuError::AuthenticationRejected),
        _ => Err(LcuError::UnexpectedStatus(status.as_u16())),
    }
}

fn append_chunk(body: &mut Vec<u8>, chunk: &[u8], limit: usize) -> Result<(), LcuError> {
    if chunk.len() > limit - body.len() {
        return Err(LcuError::ResponseTooLarge);
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn decode_body<T: DeserializeOwned>(body: &[u8]) -> Result<Option<T>, LcuError> {
    serde_json::from_slice(body).map_err(|_| LcuError::InvalidPayload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{ErrorKind, Read, Write},
        net::{Ipv4Addr, TcpListener},
        thread,
        time::Instant,
    };

    #[test]
    fn target_requires_a_valid_port_and_sensible_authentication_text() {
        assert!(LcuTarget::new(0, "fixture-only").is_err());
        assert!(LcuTarget::new(31111, "").is_err());
        assert!(LcuTarget::new(31111, "fixture\nonly").is_err());
        assert!(LcuTarget::new(31111, "fixture-only").is_ok());
    }

    #[test]
    fn requests_are_allowlisted_loopback_gets_with_sensitive_authentication() {
        let transport = LcuTransport::new().unwrap();
        let target = LcuTarget::new(31111, "fixture-only").unwrap();

        for (read, path) in [
            (ReadEndpoint::Phase, "/lol-gameflow/v1/gameflow-phase"),
            (
                ReadEndpoint::CurrentSummoner,
                "/lol-summoner/v1/current-summoner",
            ),
            (ReadEndpoint::GameflowSession, "/lol-gameflow/v1/session"),
            (ReadEndpoint::ChampSelect, "/lol-champ-select/v1/session"),
            (ReadEndpoint::AuxLobby, "/lol-lobby/v2/lobby"),
            (ReadEndpoint::AuxMatchmaking, "/lol-matchmaking/v1/search"),
            (
                ReadEndpoint::AuxReadyCheck,
                "/lol-matchmaking/v1/ready-check",
            ),
            (
                ReadEndpoint::SkinSelectorInfo,
                "/lol-champ-select/v1/skin-selector-info",
            ),
            (
                ReadEndpoint::SkinCarouselSkins,
                "/lol-champ-select/v1/skin-carousel-skins",
            ),
            (
                ReadEndpoint::IncomingChampionTrade,
                "/lol-champ-select/v1/ongoing-champion-swap",
            ),
        ] {
            let request = transport.request(&target, read).build().unwrap();
            assert_eq!(request.method(), reqwest::Method::GET);
            assert_eq!(request.url().scheme(), "https");
            assert_eq!(request.url().host_str(), Some("127.0.0.1"));
            assert_eq!(request.url().port(), Some(31111));
            assert_eq!(request.url().path(), path);
            assert!(request.url().username().is_empty());
            assert!(request.url().password().is_none());
            assert!(request.headers()[reqwest::header::AUTHORIZATION].is_sensitive());
        }
    }

    #[test]
    fn manual_aux_actions_are_fixed_to_the_known_lcu_routes_and_payloads() {
        let transport = LcuTransport::new().unwrap();
        let target = LcuTarget::new(31111, "fixture-only").unwrap();
        for (action, method, path) in [
            (
                RoomAction::StartMatchmaking,
                reqwest::Method::POST,
                "/lol-lobby/v2/lobby/matchmaking/search",
            ),
            (
                RoomAction::CancelMatchmaking,
                reqwest::Method::DELETE,
                "/lol-lobby/v2/lobby/matchmaking/search",
            ),
            (
                RoomAction::AcceptReadyCheck,
                reqwest::Method::POST,
                "/lol-matchmaking/v1/ready-check/accept",
            ),
            (
                RoomAction::DeclineReadyCheck,
                reqwest::Method::POST,
                "/lol-matchmaking/v1/ready-check/decline",
            ),
        ] {
            let request = transport
                .room_action_request(&target, action)
                .build()
                .unwrap();
            assert_eq!(*request.method(), method);
            assert_eq!(request.url().path(), path);
            assert!(request.headers()[reqwest::header::AUTHORIZATION].is_sensitive());
            assert!(request.body().is_none());
        }

        let pick = transport
            .aux_champ_select_request(
                &target,
                AuxChampSelectAction::Pick {
                    action_id: 41,
                    champion_id: 57,
                    complete: true,
                },
            )
            .build()
            .unwrap();
        assert_eq!(
            pick.body().and_then(|body| body.as_bytes()),
            Some(&br#"{"championId":57,"completed":true,"type":"pick"}"#[..])
        );

        for (action, method, path) in [
            (
                AuxChampSelectAction::BenchSwap { champion_id: 57 },
                reqwest::Method::POST,
                "/lol-champ-select/v1/session/bench/swap/57",
            ),
            (
                AuxChampSelectAction::Reroll,
                reqwest::Method::POST,
                "/lol-champ-select/v1/session/my-selection/reroll",
            ),
            (
                AuxChampSelectAction::SelectSkin { skin_id: 1001 },
                reqwest::Method::PATCH,
                "/lol-champ-select/v1/session/my-selection",
            ),
        ] {
            let request = transport
                .aux_champ_select_request(&target, action)
                .build()
                .unwrap();
            assert_eq!(*request.method(), method);
            assert_eq!(request.url().path(), path);
            assert!(request.headers()[reqwest::header::AUTHORIZATION].is_sensitive());
            if matches!(action, AuxChampSelectAction::SelectSkin { .. }) {
                assert_eq!(
                    request.body().and_then(|body| body.as_bytes()),
                    Some(&br#"{"selectedSkinId":1001}"#[..])
                );
            } else {
                assert!(request.body().is_none());
            }
        }

        let leave = transport
            .aux_champ_select_request(&target, AuxChampSelectAction::Leave)
            .build()
            .unwrap();
        assert_eq!(*leave.method(), reqwest::Method::POST);
        assert_eq!(leave.url().path(), "/lol-login/v1/session/invoke");
        let query = leave
            .url()
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            query.get("destination").map(|v| v.as_ref()),
            Some("lcdsServiceProxy")
        );
        assert_eq!(query.get("method").map(|v| v.as_ref()), Some("call"));
        assert_eq!(
            query.get("args").map(|v| v.as_ref()),
            Some(r#"["", "teambuilder-draft", "quitV2", ""]"#)
        );
        assert_eq!(
            leave.body().and_then(|body| body.as_bytes()),
            Some(&br#"{"data":["","teambuilder-draft","quitV2",""]}"#[..])
        );
    }

    #[test]
    fn projection_skin_options_keep_only_owned_enabled_rows_and_children() {
        let rows = json!([
            {"id": 1001, "name":"Unlocked", "unlocked":true, "disabled":false,
             "splashPath":"/lol-game-data/assets/ASSETS/Characters/Ashe/Skins/Base/Images/ashe_splash_centered_0.jpg",
             "childSkins":[
                {"id":1002,"name":"Owned chroma","unlocked":true,"disabled":false,
                 "chromaPreviewPath":"/lol-game-data/assets/ASSETS/Characters/Ashe/Skins/Skin62/ChromaPreview.ChromaAssetMigration.png"},
                {"id":1003,"name":"Locked chroma","unlocked":false,"disabled":false}
             ]},
            {"id": 1004, "name":"Disabled", "unlocked":true, "disabled":true},
            {"id": 1005, "name":"Locked", "unlocked":false, "disabled":false}
        ]);
        assert_eq!(
            skin_options(&rows),
            vec![
                aux_activity::AuxSkin { id: 1001, name: "Unlocked".into(), is_chroma: false,
                    preview_path: Some("/lol-game-data/assets/ASSETS/Characters/Ashe/Skins/Base/Images/ashe_splash_centered_0.jpg".into()) },
                aux_activity::AuxSkin { id: 1002, name: "Owned chroma".into(), is_chroma: true,
                    preview_path: Some("/lol-game-data/assets/ASSETS/Characters/Ashe/Skins/Skin62/ChromaPreview.ChromaAssetMigration.png".into()) },
            ]
        );
    }

    #[test]
    fn incoming_champion_exchange_uses_only_the_pinned_local_champion_routes() {
        let sample = br#"{
            "id":42,"initiatedByLocalPlayer":false,"otherSummonerIndex":1,
            "requesterChampionId":63,"responderIndex":3,"state":"RECEIVED","type":"CHAMPION"
        }"#;
        let value = decode_body::<Value>(sample).unwrap().unwrap();
        let offer = incoming_champion_trade(&value).unwrap();
        assert_eq!(
            offer,
            IncomingChampionTrade {
                id: 42,
                champion: 63
            }
        );

        for (field, replacement) in [
            ("id", json!(-1)),
            ("id", json!("42/decline")),
            ("initiatedByLocalPlayer", json!(true)),
            ("state", json!("SENT")),
            ("state", json!("ACCEPTED")),
            ("type", json!("POSITION")),
            ("requesterChampionId", json!(0)),
        ] {
            let mut invalid = value.clone();
            invalid[field] = replacement;
            assert!(incoming_champion_trade(&invalid).is_none(), "{field}");
        }

        let transport = LcuTransport::new().unwrap();
        let target = LcuTarget::new(31111, "fixture-only").unwrap();
        for (accept, suffix) in [(true, "accept"), (false, "decline")] {
            let request = transport
                .selection_request(
                    &target,
                    ScheduledStep::Trade {
                        offer,
                        held: 57,
                        accept,
                    },
                )
                .build()
                .unwrap();
            assert_eq!(request.method(), reqwest::Method::POST);
            assert_eq!(
                request.url().as_str(),
                format!("https://127.0.0.1:31111/lol-champ-select/v1/session/champion-swaps/42/{suffix}")
            );
            assert!(request.body().is_none());
            assert!(request.headers()[reqwest::header::AUTHORIZATION].is_sensitive());
        }
    }

    #[test]
    fn refuses_a_plaintext_service_on_the_verified_transport_path() {
        let transport = LcuTransport::new().unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let target = LcuTarget::new(listener.local_addr().unwrap().port(), "fixture-only").unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + REQUEST_TIMEOUT;
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("Synthetic listener did not accept a connection: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(CONNECT_TIMEOUT)).unwrap();
            stream.set_write_timeout(Some(CONNECT_TIMEOUT)).unwrap();
            let mut first_record = [0; 5];
            stream.read_exact(&mut first_record).unwrap();
            assert_eq!(
                &first_record[..2],
                &[22, 3],
                "TLS handshake must precede HTTP"
            );
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .unwrap();
        });

        let result = tauri::async_runtime::block_on(transport.read_phase(&target));
        server.join().unwrap();
        assert_eq!(result, Err(LcuError::Transport));
    }

    #[test]
    fn missing_data_is_distinct_from_authentication_failure_and_redirect() {
        assert_eq!(response_has_body(StatusCode::NO_CONTENT), Ok(false));
        assert_eq!(response_has_body(StatusCode::NOT_FOUND), Ok(false));
        assert_eq!(response_has_body(StatusCode::OK), Ok(true));
        assert_eq!(
            response_has_body(StatusCode::UNAUTHORIZED),
            Err(LcuError::AuthenticationRejected)
        );
        assert_eq!(
            response_has_body(StatusCode::FORBIDDEN),
            Err(LcuError::AuthenticationRejected)
        );
        assert_eq!(
            response_has_body(StatusCode::TEMPORARY_REDIRECT),
            Err(LcuError::UnexpectedStatus(307))
        );
    }

    #[test]
    fn optional_skin_failures_do_not_discard_pickable_ids() {
        let extras = tauri::async_runtime::block_on(async {
            tokio::time::timeout(
                Duration::from_millis(750),
                collect_aux_selection_extras(
                    async { Ok::<_, LcuError>(Some(vec![22, 103])) },
                    async { Ok::<_, LcuError>(Some(vec![22, 103])) },
                    async { Err::<Option<Value>, _>(LcuError::Transport) },
                    async {
                        tokio::time::sleep(Duration::from_millis(800)).await;
                        Ok::<_, LcuError>(Some(json!([])))
                    },
                    AUX_SELECTION_READ_TIMEOUT,
                ),
            )
            .await
            .expect("sample's 750ms read budget is sufficient")
        });

        assert_eq!(extras.pickable_champion_ids, Some(vec![22, 103]));
        assert_eq!(extras.subset_champion_ids, Some(vec![22, 103]));
        assert!(extras.skins.is_empty());
        assert!(!extras.skin_selector_available);
    }

    #[test]
    fn body_size_checks_total_across_chunks_before_appending() {
        let mut body = Vec::new();
        append_chunk(&mut body, b"ab", 4).unwrap();
        append_chunk(&mut body, b"cd", 4).unwrap();
        assert_eq!(
            append_chunk(&mut body, b"e", 4),
            Err(LcuError::ResponseTooLarge)
        );
        assert_eq!(&body, b"abcd");
    }

    #[test]
    fn phase_and_old_summoner_name_shapes_are_preserved() {
        assert_eq!(
            decode_body::<String>(br#""ChampSelect""#)
                .unwrap()
                .as_deref(),
            Some("ChampSelect")
        );
        assert_eq!(decode_body::<String>(b"null").unwrap(), None);
        assert_eq!(decode_body::<String>(b"{}"), Err(LcuError::InvalidPayload));

        let player = decode_body::<CurrentSummoner>(
            br#"{"puuid":"fixture-player","displayName":"Fixture","summonerId":41,"profileIconId":7,
                 "xpSinceLastLevel":200,"xpUntilNextLevel":1000}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(player.display_name.as_deref(), Some("Fixture"));
        assert!(player.game_name.is_none());
        assert!(player.tag_line.is_none());
        assert_eq!(player.summoner_id, Some(41));
        assert_eq!(player.xp_since_last_level, Some(200));
        assert_eq!(player.xp_until_next_level, Some(1000));
    }

    #[test]
    fn session_keeps_aram_queue_and_discards_private_game_data() {
        let session = decode_body::<GameflowSession>(
            br#"{
                "phase":"ChampSelect",
                "gameData":{
                    "gameId":9001123,
                    "isCustomGame":false,
                    "password":"synthetic",
                    "spectatorKey":"synthetic",
                    "queue":{"id":2400,"gameMode":"KIWI","type":"ARAM_UNRANKED_5x5"},
                    "teamOne":[{"puuid":"fixture-opponent"}]
                },
                "gameClient":{"serverIp":"192.0.2.1"}
            }"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            serde_json::to_value(session).unwrap(),
            serde_json::json!({
                "phase": "ChampSelect",
                "gameData": {
                    "gameId": 9001123,
                    "isCustomGame": false,
                    "queue": { "id": 2400, "gameMode": "KIWI", "type": "ARAM_UNRANKED_5x5" }
                }
            })
        );
    }

    #[test]
    fn resg_uses_the_local_champion_without_exposing_the_roster() {
        let mut source = serde_json::json!({
            "phase": "ChampSelect",
            "gameData": {
                "gameId": 19,
                "queue": { "gameMode": "KIWI" },
                "teamOne": [{ "puuid": "other-fixture", "championId": 21 }],
                "playerChampionSelections": [{ "puuid": "self-fixture", "championId": 22 }]
            }
        });
        let select = serde_json::json!({
            "localPlayerCellId": 4,
            "myTeam": [{ "cellId": 4, "championId": 23 }],
            "actions": [[{ "actorCellId": 4, "type": "pick", "championId": 24 }]]
        });
        let session: GameflowSession = serde_json::from_value(source.clone()).unwrap();
        assert_eq!(session.resg_champion(&select, None), Some(23));
        let renderer = serde_json::to_value(session).unwrap();
        assert!(renderer["gameData"].get("teamOne").is_none());
        assert!(renderer["gameData"]
            .get("playerChampionSelections")
            .is_none());

        source["phase"] = serde_json::json!("InProgress");
        let session: GameflowSession = serde_json::from_value(source.clone()).unwrap();
        assert_eq!(
            session.resg_champion(&select, Some("self-fixture")),
            Some(22)
        );
        assert_eq!(
            session.resg_champion(&select, Some("missing-fixture")),
            None
        );

        source["gameData"]["queue"]["gameMode"] = serde_json::json!("ARAM");
        let session: GameflowSession = serde_json::from_value(source).unwrap();
        assert_eq!(session.resg_champion(&select, Some("self-fixture")), None);
    }
}
