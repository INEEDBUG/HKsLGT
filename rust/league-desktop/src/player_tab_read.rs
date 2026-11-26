//! The match history screen only needs these authenticated LCU reads.

use super::{
    LcuError, LcuTarget, LcuTransport,
    player_search::{PlayerSearchShard, Puuid, may_request_history, shard_from_platform},
};
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

const PROFILE: &str = "/lol-summoner/v2/summoners/puuid/";
const CURRENT_PROFILE: &str = "/lol-summoner/v1/current-summoner";
const HISTORY: &str = "/lol-match-history/v1/products/lol/";
const DETAIL: &str = "/lol-match-history/v1/games/";
const TIMELINE: &str = "/lol-match-history/v1/game-timelines/";
const RANKED: &str = "/lol-ranked/v1/ranked-stats/";
const RECOMMENDED_POSITIONS: &str = "/lol-perks/v1/recommended-champion-positions";
const MAX_PAGE_SIZE: u32 = 200;
const MAX_GAME_ID: u64 = (1 << 53) - 1;
const MAX_RANKED_BYTES: usize = 64 * 1024;
const MAX_RECOMMENDED_POSITIONS_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Page {
    beg_index: u32,
    end_index: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RankedStatsShape {
    queue_map: HashMap<String, Option<Value>>,
    queues: Vec<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecommendedPositionsRow {
    recommended_positions: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PlayerTabRead(Read);

#[derive(Debug, PartialEq, Eq)]
enum Read {
    Profile(Puuid),
    Matches(Puuid, Page),
    RankedStats(Puuid),
    RecommendedPositions,
    Detail(u64),
    Timeline(u64),
}

impl PlayerTabRead {
    pub(crate) fn parse(path: &str, params: &Value) -> Option<Self> {
        if path == RECOMMENDED_POSITIONS {
            return params.is_null().then_some(Self(Read::RecommendedPositions));
        }
        if let Some(id) = path.strip_prefix(PROFILE).filter(|_| params.is_null()) {
            return Some(Self(Read::Profile(Puuid::parse(id)?)));
        }
        if let Some(id) = path
            .strip_prefix(HISTORY)
            .and_then(|rest| rest.strip_suffix("/matches"))
        {
            let page = Page::deserialize(params).ok()?;
            (page.end_index <= i32::MAX as u32
                && page.end_index.checked_sub(page.beg_index)? < MAX_PAGE_SIZE)
                .then_some(())?;
            return Some(Self(Read::Matches(Puuid::parse(id)?, page)));
        }
        if let Some(id) = path.strip_prefix(RANKED).filter(|_| params.is_null()) {
            return Some(Self(Read::RankedStats(Puuid::parse(id)?)));
        }
        if !params.is_null() {
            return None;
        }
        let (path, id) = path.rsplit_once('/')?;
        let id_number = id.parse::<u64>().ok()?;
        if !(1..=MAX_GAME_ID).contains(&id_number) || id != id_number.to_string() {
            return None;
        }
        match path {
            "/lol-match-history/v1/games" => Some(Self(Read::Detail(id_number))),
            "/lol-match-history/v1/game-timelines" => Some(Self(Read::Timeline(id_number))),
            _ => None,
        }
    }

    pub(crate) fn detail_id(&self) -> Option<u64> {
        match self.0 {
            Read::Detail(id) | Read::Timeline(id) => Some(id),
            _ => None,
        }
    }

    pub(crate) fn is_page(&self) -> bool {
        matches!(self.0, Read::Matches(..))
    }
}

fn validate_page(
    mut reply: Value,
    page: &Page,
    shard: &PlayerSearchShard,
) -> Result<Value, LcuError> {
    let matches_shard = reply["platformId"]
        .as_str()
        .and_then(|platform| shard_from_platform(platform).ok())
        .is_some_and(|observed| observed.sgp_server_id == shard.sgp_server_id);
    let games = reply
        .get_mut("games")
        .and_then(|page| page.get_mut("games"))
        .and_then(Value::as_array_mut)
        .ok_or(LcuError::InvalidPayload)?;
    if games.is_empty() {
        return Ok(reply);
    }
    if games.len() > (page.end_index - page.beg_index + 1) as usize || !matches_shard {
        return Err(LcuError::InvalidPayload);
    }
    games.retain(|game| {
        game["gameId"]
            .as_u64()
            .is_some_and(|id| (1..=MAX_GAME_ID).contains(&id))
    });
    if games.is_empty() {
        return Err(LcuError::InvalidPayload);
    }
    Ok(reply)
}

fn validate_ranked_stats(reply: Option<Value>) -> Result<Value, LcuError> {
    let Some(reply) = reply else {
        return Ok(Value::Null);
    };
    if reply.is_null() {
        return Ok(Value::Null);
    }
    let shape: RankedStatsShape =
        serde_json::from_value(reply.clone()).map_err(|_| LcuError::InvalidPayload)?;
    if shape.queues.iter().any(|entry| !entry.is_object())
        || shape
            .queue_map
            .values()
            .flatten()
            .any(|entry| !entry.is_object())
    {
        return Err(LcuError::InvalidPayload);
    }
    Ok(reply)
}

fn validate_recommended_positions(reply: Option<Value>) -> Result<Value, LcuError> {
    let reply = reply.ok_or(LcuError::UnexpectedStatus(404))?;
    let entries: BTreeMap<String, RecommendedPositionsRow> =
        serde_json::from_value(reply).map_err(|_| LcuError::InvalidPayload)?;
    let mut projected = BTreeMap::new();
    for (id, row) in entries {
        let numeric_id = id.parse::<i64>().map_err(|_| LcuError::InvalidPayload)?;
        if numeric_id.to_string() != id {
            return Err(LcuError::InvalidPayload);
        }
        // The client may include non-champion placeholder entries.
        if numeric_id <= 0 {
            continue;
        }
        projected.insert(id, row);
    }
    serde_json::to_value(projected).map_err(|_| LcuError::InvalidPayload)
}

impl LcuTransport {
    async fn read_tab_profile(
        &self,
        target: &LcuTarget<'_>,
        player: &Puuid,
        own: &Puuid,
    ) -> Result<Value, LcuError> {
        let by_id = format!("{PROFILE}{}", player.0);
        let path = if player == own {
            CURRENT_PROFILE
        } else {
            &by_id
        };
        let data: Value = Self::read_json(self.local_json_get(target, path), 32 * 1024)
            .await?
            .ok_or(LcuError::UnexpectedStatus(404))?;
        if data["puuid"].as_str().and_then(Puuid::parse).as_ref() != Some(player) {
            return Err(LcuError::InvalidPayload);
        }
        Ok(data)
    }

    async fn authorize_tab_player(
        &self,
        target: &LcuTarget<'_>,
        player: &Puuid,
        own: &Puuid,
    ) -> Result<(), LcuError> {
        if player != own {
            let profile = self.read_tab_profile(target, player, own).await?;
            if !may_request_history(own, player, profile["privacy"].as_str().unwrap_or("")) {
                return Err(LcuError::AuthenticationRejected);
            }
        }
        Ok(())
    }

    pub(crate) async fn read_player_tab(
        &self,
        target: &LcuTarget<'_>,
        request: &PlayerTabRead,
        own_puuid: &str,
        shard: &PlayerSearchShard,
    ) -> Result<Value, LcuError> {
        let own = Puuid::parse(own_puuid).ok_or(LcuError::InvalidPayload)?;
        match &request.0 {
            Read::Profile(player) => self.read_tab_profile(target, player, &own).await,
            Read::Matches(player, page) => {
                self.authorize_tab_player(target, player, &own).await?;
                let path = format!(
                    "{HISTORY}{}/matches?begIndex={}&endIndex={}",
                    player.0, page.beg_index, page.end_index,
                );
                let request = self.local_json_get(target, &path);
                let reply = Self::read_json(request, 8 * 1024 * 1024)
                    .await?
                    .ok_or(LcuError::UnexpectedStatus(404))?;
                validate_page(reply, page, shard)
            }
            Read::RankedStats(player) => {
                self.authorize_tab_player(target, player, &own).await?;
                let path = format!("{RANKED}{}", player.0);
                let reply =
                    Self::read_json(self.local_json_get(target, &path), MAX_RANKED_BYTES).await?;
                validate_ranked_stats(reply)
            }
            Read::RecommendedPositions => {
                let reply = Self::read_json(
                    self.local_json_get(target, RECOMMENDED_POSITIONS),
                    MAX_RECOMMENDED_POSITIONS_BYTES,
                )
                .await?;
                validate_recommended_positions(reply)
            }
            Read::Detail(id) | Read::Timeline(id) => {
                let timeline = matches!(request.0, Read::Timeline(_));
                let prefix = if timeline { TIMELINE } else { DETAIL };
                let request = self.local_json_get(target, &format!("{prefix}{id}"));
                let reply: Value = Self::read_json(request, 4 * 1024 * 1024)
                    .await?
                    .ok_or(LcuError::UnexpectedStatus(404))?;
                (if timeline {
                    reply["frames"].is_array()
                } else {
                    reply["gameId"].as_u64() == Some(*id)
                })
                .then_some(reply)
                .ok_or(LcuError::InvalidPayload)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PLAYER: &str = "6f541254-473e-4ddb-a9b2-e96908061989";

    #[test]
    fn validates_the_pinned_renderer_get_paths_and_page_sizes() {
        let query = json!({"begIndex": 20, "endIndex": 219});
        let history = format!("{HISTORY}{PLAYER}/matches");
        assert!(PlayerTabRead::parse(&history, &query).unwrap().is_page());
        assert!(PlayerTabRead::parse(&format!("{PROFILE}{PLAYER}"), &Value::Null).is_some());
        assert_eq!(
            PlayerTabRead::parse(&format!("{DETAIL}131"), &Value::Null)
                .unwrap()
                .detail_id(),
            Some(131)
        );
        assert!(PlayerTabRead::parse(&format!("{TIMELINE}131"), &Value::Null).is_some());
        for invalid in [
            json!({"begIndex": 20, "endIndex": 220}),
            json!({"begIndex": 20, "endIndex": 19}),
            json!({"begIndex": 0, "endIndex": 19, "other": "foo"}),
            json!({"begIndex": 0, "endIndex": "19"}),
            Value::Null,
        ] {
            assert!(PlayerTabRead::parse(&history, &invalid).is_none());
        }
        for invalid in [
            "https://example.com/lol-summoner/v1/current-summoner",
            "/lol-summoner/v2/summoners/puuid/../v1/current-summoner",
            "/lol-match-history/v1/games/1?x=1",
            "/lol-match-history/v1/game-timelines/+1",
            "/lol-match-history/v1/games/9007199254740992",
            "/lol-gameflow/v1/session",
        ] {
            assert!(PlayerTabRead::parse(invalid, &Value::Null).is_none());
        }
    }

    #[test]
    fn allowlists_pinned_ranked_stats_get_for_a_valid_puuid() {
        let path = format!("/lol-ranked/v1/ranked-stats/{PLAYER}");
        assert!(PlayerTabRead::parse(&path, &Value::Null).is_some());
        assert!(PlayerTabRead::parse(&path, &json!({"puuid": PLAYER})).is_none());
        assert!(
            PlayerTabRead::parse("/lol-ranked/v1/current-ranked-stats", &Value::Null).is_none()
        );
        assert!(
            PlayerTabRead::parse(
                &format!("/lol-ranked/v1/ranked-stats/../current-ranked-stats"),
                &Value::Null
            )
            .is_none()
        );
    }

    #[test]
    fn allowlists_only_the_exact_recommended_positions_get() {
        let path = "/lol-perks/v1/recommended-champion-positions";
        assert!(PlayerTabRead::parse(path, &Value::Null).is_some());
        for invalid in [
            "/lol-perks/v1/recommended-champion-positions/",
            "/lol-perks/v1/recommended-champion-positions?x=1",
            "/lol-perks/v1/recommended-champion-positions/1",
            "https://example.com/lol-perks/v1/recommended-champion-positions",
        ] {
            assert!(PlayerTabRead::parse(invalid, &Value::Null).is_none());
        }
        assert!(PlayerTabRead::parse(path, &json!({"puuid": PLAYER})).is_none());
    }

    #[test]
    fn projects_bounded_recommended_positions_records_without_filtering_ids() {
        let reply = json!({
            "67": {"recommendedPositions": ["MIDDLE", "BOTTOM"]},
            "266": {"recommendedPositions": ["TOP"]},
            "210000": {"recommendedPositions": ["FUTURE_POSITION", ""]},
            "99999": {"recommendedPositions": []},
            "0": {"recommendedPositions": ["UNASSIGNED"]},
            "-1": {"recommendedPositions": ["DUMMY"]}
        });
        let expected = json!({
            "67": {"recommendedPositions": ["MIDDLE", "BOTTOM"]},
            "266": {"recommendedPositions": ["TOP"]},
            "210000": {"recommendedPositions": ["FUTURE_POSITION", ""]},
            "99999": {"recommendedPositions": []}
        });
        assert_eq!(validate_recommended_positions(Some(reply)), Ok(expected));
        assert_eq!(
            validate_recommended_positions(None),
            Err(LcuError::UnexpectedStatus(404))
        );

        for invalid in [
            json!([]),
            json!({"01": {"recommendedPositions": ["TOP"]}}),
            json!({"67": {"recommendedPositions": "MIDDLE"}}),
            json!({"not-a-champion-id": {"recommendedPositions": ["TOP"]}}),
        ] {
            assert_eq!(
                validate_recommended_positions(Some(invalid)),
                Err(LcuError::InvalidPayload)
            );
        }
    }

    fn ranked_entry(queue_type: &str) -> Value {
        json!({
            "queueType": queue_type,
            "tier": "GOLD",
            "division": "II",
            "highestTier": "PLATINUM",
            "highestDivision": "IV",
            "previousSeasonEndTier": "SILVER",
            "previousSeasonEndDivision": "I",
            "previousSeasonHighestTier": "GOLD",
            "previousSeasonHighestDivision": "I",
            "wins": 12,
            "losses": 8,
            "leaguePoints": 54
        })
    }

    #[test]
    fn validates_renderer_ranked_stats_shape_and_keeps_the_original_json() {
        let entry = ranked_entry("RANKED_SOLO_5x5");
        let reply = json!({
            "queueMap": {"RANKED_SOLO_5x5": entry.clone()},
            "queues": [entry],
            "unconsumedUpstreamField": true
        });
        assert_eq!(validate_ranked_stats(Some(reply.clone())), Ok(reply));
        assert_eq!(validate_ranked_stats(None), Ok(Value::Null));
        assert_eq!(validate_ranked_stats(Some(Value::Null)), Ok(Value::Null));

        let sparse_entry = json!({"queueType": "RANKED_SOLO_5x5", "tier": "NA"});
        let sparse = json!({
            "queueMap": {"RANKED_SOLO_5x5": sparse_entry.clone(), "RANKED_FLEX_SR": null},
            "queues": [sparse_entry]
        });
        assert_eq!(validate_ranked_stats(Some(sparse.clone())), Ok(sparse));

        for invalid in [
            json!({"queueMap": {}, "queues": "not-an-array"}),
            json!({"queueMap": "not-an-object", "queues": []}),
            json!({"queueMap": {}, "queues": [null]}),
            json!({"queueMap": {"RANKED_SOLO_5x5": "not-an-object"}, "queues": []}),
        ] {
            assert_eq!(
                validate_ranked_stats(Some(invalid)),
                Err(LcuError::InvalidPayload)
            );
        }
    }

    #[test]
    fn keeps_a_tencent_aram_page_and_distinguishes_empty_and_unreadable() {
        let page = Page {
            beg_index: 20,
            end_index: 39,
        };
        let game = json!({"gameId": 123, "gameMode": "KIWI", "platformId": "HN1"});
        let shard = shard_from_platform("HN1").unwrap();
        let reply = json!({"platformId": "HN1", "games": {"games": [game]}});
        assert_eq!(
            validate_page(reply.clone(), &page, &shard),
            Ok(reply.clone())
        );
        let other = shard_from_platform("HN10").unwrap();
        assert_eq!(
            validate_page(reply.clone(), &page, &other),
            Err(LcuError::InvalidPayload)
        );
        let mixed = json!({"platformId": "HN1", "games": {"games": [
            {"gameId": 123, "gameMode": "KIWI", "platformId": "HN1"}, null, {}
        ]}});
        assert_eq!(validate_page(mixed, &page, &shard), Ok(reply));
        let empty = json!({"platformId": "HN1", "games": {"games": []}});
        assert_eq!(validate_page(empty.clone(), &page, &shard), Ok(empty));
        let error = json!({"errorCode": "RPC_ERROR"});
        assert_eq!(
            validate_page(error, &page, &shard),
            Err(LcuError::InvalidPayload)
        );
    }
}
