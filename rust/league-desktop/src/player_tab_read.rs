//! The match history screen only needs these authenticated LCU reads.

use super::{
    player_search::{may_request_history, shard_from_platform, PlayerSearchShard, Puuid},
    LcuError, LcuTarget, LcuTransport,
};
use serde::Deserialize;
use serde_json::Value;

const PROFILE: &str = "/lol-summoner/v2/summoners/puuid/";
const CURRENT_PROFILE: &str = "/lol-summoner/v1/current-summoner";
const HISTORY: &str = "/lol-match-history/v1/products/lol/";
const DETAIL: &str = "/lol-match-history/v1/games/";
const TIMELINE: &str = "/lol-match-history/v1/game-timelines/";
const MAX_PAGE_SIZE: u32 = 200;
const MAX_GAME_ID: u64 = (1 << 53) - 1;

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Page {
    beg_index: u32,
    end_index: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PlayerTabRead(Read);

#[derive(Debug, PartialEq, Eq)]
enum Read {
    Profile(Puuid),
    Matches(Puuid, Page),
    Detail(u64),
    Timeline(u64),
}

impl PlayerTabRead {
    pub(crate) fn parse(path: &str, params: &Value) -> Option<Self> {
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
                if player != &own {
                    let profile = self.read_tab_profile(target, player, &own).await?;
                    if !may_request_history(&own, player, profile["privacy"].as_str().unwrap_or(""))
                    {
                        return Err(LcuError::AuthenticationRejected);
                    }
                }
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
