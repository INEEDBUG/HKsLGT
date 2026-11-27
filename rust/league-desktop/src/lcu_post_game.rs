//! Fixed local LCU reads and writes used by the post-game flow.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PLAYER_A: &str = "11111111-2222-4333-8444-555555555555";
    const PLAYER_B: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

    #[test]
    fn ballot_projection_keeps_only_valid_human_ids_and_vote_state() {
        let raw = json!({
            "gameId": 123456,
            "votePool": {"votes": 2, "fromGamePlayed": 1},
            "honoredPlayers": [{"honorType": "HEART", "recipientPuuid": PLAYER_B}],
            "eligibleAllies": [
                {"botPlayer": false, "puuid": PLAYER_A, "summonerName": "Synthetic"},
                {"botPlayer": true, "puuid": "not-a-player-id"}
            ],
            "eligibleOpponents": [{"botPlayer": false, "puuid": PLAYER_B}]
        });
        assert_eq!(
            parse_ballot(Some(raw)),
            Ok(Some(PostGameBallot {
                game_id: 123456,
                votes: 2,
                eligible_allies: vec![PLAYER_A.into()],
                eligible_opponents: vec![PLAYER_B.into()],
                honored_puuids: vec![PLAYER_B.into()],
            }))
        );
        assert_eq!(parse_ballot(None), Ok(None));
        assert_eq!(parse_ballot(Some(json!({"gameId": 0}))), Ok(None));
    }

    #[test]
    fn ballot_projection_rejects_malformed_vote_state_and_puuid_records() {
        for raw in [
            json!({"gameId": 1, "votePool": {}, "honoredPlayers": [], "eligibleAllies": [], "eligibleOpponents": []}),
            json!({"gameId": 1, "votePool": {"votes": 1}, "honoredPlayers": [], "eligibleAllies": "bad", "eligibleOpponents": []}),
            json!({"gameId": 1, "votePool": {"votes": 1}, "honoredPlayers": [], "eligibleAllies": [{"botPlayer": false, "puuid": "bad"}], "eligibleOpponents": []}),
            json!({"gameId": 1, "votePool": {"votes": 1}, "honoredPlayers": [{"recipientPuuid": "bad"}], "eligibleAllies": [], "eligibleOpponents": []}),
        ] {
            assert_eq!(parse_ballot(Some(raw)), Err(LcuError::InvalidPayload));
        }
    }

    #[test]
    fn optional_party_list_is_a_deduplicated_union_of_eog_groups() {
        let raw = json!({
            "eogPlayers": [PLAYER_A],
            "leftPlayers": [PLAYER_B, PLAYER_A],
            "readyPlayers": [PLAYER_B]
        });
        assert_eq!(
            parse_party_members(Some(raw)),
            Ok(Some(vec![PLAYER_A.into(), PLAYER_B.into()]))
        );
        assert_eq!(parse_party_members(None), Ok(None));
    }

    #[test]
    fn only_the_exact_missions_celebration_sequence_is_eligible() {
        assert_eq!(
            parse_sequence(Some(json!({"name": "missions-celebration"}))),
            Ok(Some(SequenceEvent::MissionsCelebration))
        );
        assert_eq!(
            parse_sequence(Some(json!({"name": "honor"}))),
            Ok(Some(SequenceEvent::Other))
        );
        assert_eq!(parse_sequence(None), Ok(None));
    }

    #[test]
    fn requests_are_fixed_https_loopback_operations() {
        let transport = LcuTransport::new().unwrap();
        let target = LcuTarget::new(31111, "fixture-only").unwrap();
        for (request, method, path) in [
            (
                transport.ballot_request(&target),
                reqwest::Method::GET,
                V2_BALLOT_PATH,
            ),
            (
                transport.party_members_request(&target),
                reqwest::Method::GET,
                PARTY_MEMBERS_PATH,
            ),
            (
                transport.sequence_request(&target),
                reqwest::Method::GET,
                SEQUENCE_PATH,
            ),
            (
                transport.complete_sequence_request(&target),
                reqwest::Method::POST,
                COMPLETE_SEQUENCE_PATH,
            ),
            (
                transport.ballot_submit_request(&target),
                reqwest::Method::POST,
                SUBMIT_BALLOT_PATH,
            ),
            (
                transport.play_again_request(&target),
                reqwest::Method::POST,
                PLAY_AGAIN_PATH,
            ),
        ] {
            let request = request.build().unwrap();
            assert_eq!(request.method(), method);
            assert_eq!(request.url().scheme(), "https");
            assert_eq!(request.url().host_str(), Some("127.0.0.1"));
            assert_eq!(request.url().port(), Some(31111));
            assert_eq!(request.url().path(), path);
            assert!(request.url().query().is_none());
            assert!(request.headers()[reqwest::header::AUTHORIZATION].is_sensitive());
            assert!(request.body().is_none());
        }
        let request = transport
            .honor_request(&target, PLAYER_A)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(request.url().path(), HONOR_PATH);
        assert_eq!(request.url().host_str(), Some("127.0.0.1"));
        assert!(request.headers()[reqwest::header::AUTHORIZATION].is_sensitive());
        let body: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"honorType":"HEART", "recipientPuuid":PLAYER_A})
        );
        assert!(transport.honor_request(&target, "bad/payload").is_err());
    }
}

use super::{LcuError, LcuTarget, LcuTransport, player_search::Puuid};
use reqwest::RequestBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;

const V2_BALLOT_PATH: &str = "/lol-honor-v2/v1/ballot/";
const SUBMIT_BALLOT_PATH: &str = "/lol-honor/v1/ballot";
const PARTY_MEMBERS_PATH: &str = "/lol-lobby/v2/party/eog-status";
const SEQUENCE_PATH: &str = "/lol-pre-end-of-game/v1/currentSequenceEvent";
const COMPLETE_SEQUENCE_PATH: &str = "/lol-pre-end-of-game/v1/complete/missions-celebration";
const HONOR_PATH: &str = "/lol-honor/v1/honor";
const PLAY_AGAIN_PATH: &str = "/lol-lobby/v2/play-again";
const POST_GAME_BODY_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PostGameBallot {
    pub(crate) game_id: u64,
    pub(crate) votes: u32,
    pub(crate) eligible_allies: Vec<String>,
    pub(crate) eligible_opponents: Vec<String>,
    pub(crate) honored_puuids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BallotPayload {
    game_id: u64,
    vote_pool: VotePool,
    eligible_allies: Vec<EligiblePlayer>,
    eligible_opponents: Vec<EligiblePlayer>,
    honored_players: Vec<HonoredPlayer>,
}

#[derive(Deserialize)]
struct VotePool {
    votes: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EligiblePlayer {
    bot_player: bool,
    puuid: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HonoredPlayer {
    recipient_puuid: String,
}

#[derive(Debug, PartialEq, Eq)]
enum SequenceEvent {
    MissionsCelebration,
    Other,
}

#[derive(Deserialize)]
struct SequencePayload {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EogStatus {
    eog_players: Vec<String>,
    left_players: Vec<String>,
    ready_players: Vec<String>,
}

fn parse_ballot(raw: Option<Value>) -> Result<Option<PostGameBallot>, LcuError> {
    let Some(raw) = raw.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let game_id = raw
        .get("gameId")
        .and_then(Value::as_u64)
        .ok_or(LcuError::InvalidPayload)?;
    if game_id == 0 {
        return Ok(None);
    }
    let ballot: BallotPayload =
        serde_json::from_value(raw).map_err(|_| LcuError::InvalidPayload)?;
    let humans = |players: Vec<EligiblePlayer>| {
        let mut seen = HashSet::new();
        let mut output = Vec::new();
        for player in players {
            if player.bot_player {
                continue;
            }
            let puuid = Puuid::parse(player.puuid.as_deref().unwrap_or_default())
                .ok_or(LcuError::InvalidPayload)?
                .0;
            if seen.insert(puuid.clone()) {
                output.push(puuid);
            }
        }
        Ok(output)
    };
    let mut honored_seen = HashSet::new();
    let mut honored_puuids = Vec::new();
    for player in ballot.honored_players {
        let puuid = Puuid::parse(&player.recipient_puuid)
            .ok_or(LcuError::InvalidPayload)?
            .0;
        if honored_seen.insert(puuid.clone()) {
            honored_puuids.push(puuid);
        }
    }
    Ok(Some(PostGameBallot {
        game_id: ballot.game_id,
        votes: ballot.vote_pool.votes,
        eligible_allies: humans(ballot.eligible_allies)?,
        eligible_opponents: humans(ballot.eligible_opponents)?,
        honored_puuids,
    }))
}

fn parse_party_members(raw: Option<Value>) -> Result<Option<Vec<String>>, LcuError> {
    let Some(raw) = raw.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let status: EogStatus = serde_json::from_value(raw).map_err(|_| LcuError::InvalidPayload)?;
    let mut seen = HashSet::new();
    let mut members = Vec::new();
    for puuid in status
        .eog_players
        .into_iter()
        .chain(status.left_players)
        .chain(status.ready_players)
    {
        let normalized = Puuid::parse(&puuid).ok_or(LcuError::InvalidPayload)?.0;
        if seen.insert(normalized.clone()) {
            members.push(normalized);
        }
    }
    Ok(Some(members))
}

fn parse_sequence(raw: Option<Value>) -> Result<Option<SequenceEvent>, LcuError> {
    let Some(raw) = raw.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let sequence: SequencePayload =
        serde_json::from_value(raw).map_err(|_| LcuError::InvalidPayload)?;
    Ok(Some(if sequence.name == "missions-celebration" {
        SequenceEvent::MissionsCelebration
    } else {
        SequenceEvent::Other
    }))
}

impl LcuTransport {
    fn ballot_request(&self, target: &LcuTarget<'_>) -> RequestBuilder {
        self.local_json_get(target, V2_BALLOT_PATH)
    }

    fn party_members_request(&self, target: &LcuTarget<'_>) -> RequestBuilder {
        self.local_json_get(target, PARTY_MEMBERS_PATH)
    }

    fn sequence_request(&self, target: &LcuTarget<'_>) -> RequestBuilder {
        self.local_json_get(target, SEQUENCE_PATH)
    }

    fn complete_sequence_request(&self, target: &LcuTarget<'_>) -> RequestBuilder {
        self.local_json_post(target, COMPLETE_SEQUENCE_PATH)
    }

    fn honor_request(
        &self,
        target: &LcuTarget<'_>,
        recipient_puuid: &str,
    ) -> Result<RequestBuilder, LcuError> {
        let puuid = Puuid::parse(recipient_puuid).ok_or(LcuError::InvalidPayload)?;
        Ok(self
            .local_json_post(target, HONOR_PATH)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(
                json!({
                    "honorType": "HEART",
                    "recipientPuuid": puuid.0,
                })
                .to_string(),
            ))
    }

    fn ballot_submit_request(&self, target: &LcuTarget<'_>) -> RequestBuilder {
        self.local_json_post(target, SUBMIT_BALLOT_PATH)
    }

    fn play_again_request(&self, target: &LcuTarget<'_>) -> RequestBuilder {
        self.local_json_post(target, PLAY_AGAIN_PATH)
    }

    fn local_json_post(&self, target: &LcuTarget<'_>, path: &str) -> RequestBuilder {
        self.client
            .post(format!("https://127.0.0.1:{}{path}", target.port))
            .header(reqwest::header::ACCEPT, "application/json")
            .basic_auth("riot", Some(target.token))
    }

    pub(crate) async fn read_post_game_ballot(
        &self,
        target: &LcuTarget<'_>,
    ) -> Result<Option<PostGameBallot>, LcuError> {
        let raw = Self::read_json(self.ballot_request(target), POST_GAME_BODY_LIMIT).await?;
        parse_ballot(raw)
    }

    pub(crate) async fn read_post_game_party_members(
        &self,
        target: &LcuTarget<'_>,
    ) -> Result<Option<Vec<String>>, LcuError> {
        let raw = Self::read_json(self.party_members_request(target), POST_GAME_BODY_LIMIT).await?;
        parse_party_members(raw)
    }

    pub(crate) async fn missions_celebration_pending(
        &self,
        target: &LcuTarget<'_>,
    ) -> Result<bool, LcuError> {
        let raw = Self::read_json(self.sequence_request(target), 8 * 1024).await?;
        Ok(parse_sequence(raw)? == Some(SequenceEvent::MissionsCelebration))
    }

    pub(crate) async fn send_missions_celebration_complete(
        &self,
        target: &LcuTarget<'_>,
    ) -> Result<(), LcuError> {
        self.send_action(self.complete_sequence_request(target)).await
    }

    pub(crate) async fn submit_post_game_honor(
        &self,
        target: &LcuTarget<'_>,
        recipient_puuid: &str,
    ) -> Result<(), LcuError> {
        self.send_action(self.honor_request(target, recipient_puuid)?)
            .await
    }

    pub(crate) async fn submit_post_game_ballot(
        &self,
        target: &LcuTarget<'_>,
    ) -> Result<(), LcuError> {
        self.send_action(self.ballot_submit_request(target)).await
    }

    pub(crate) async fn play_again(&self, target: &LcuTarget<'_>) -> Result<(), LcuError> {
        self.send_action(self.play_again_request(target)).await
    }
}
