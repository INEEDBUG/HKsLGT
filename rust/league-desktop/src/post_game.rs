//! Finish the current ARAM ballot and return its party to the lobby.

use super::{
    ClientInstance, ClientRuntime, LcuError, LcuTarget, LocalClient, RoomOptions, Snapshot,
};
use crate::lcu::post_game::PostGameBallot;
use futures_util::future::join3;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

#[derive(Clone, PartialEq, Eq)]
struct Game {
    instance: ClientInstance,
    player: String,
    id: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    WaitingForStats,
    PreEndOfGame,
    EndOfGame,
}

impl Stage {
    fn parse(phase: &str) -> Option<Self> {
        match phase {
            "WaitingForStats" => Some(Self::WaitingForStats),
            "PreEndOfGame" => Some(Self::PreEndOfGame),
            "EndOfGame" => Some(Self::EndOfGame),
            _ => None,
        }
    }

    fn return_delay(self) -> Duration {
        Duration::from_millis(match self {
            Self::WaitingForStats => 10_000,
            Self::PreEndOfGame => 3_250,
            Self::EndOfGame => 1_575,
        })
    }
}

pub(super) struct Turn {
    game: Game,
    stage: Stage,
}

#[derive(Default)]
pub(super) struct PostGame {
    game: Option<Game>,
    stage: Option<(Stage, Instant)>,
    next_poll: Option<Instant>,
    // Keep the same recipients on retry, even if a client repeats an old vote count.
    recipients: Option<Vec<String>>,
    accepted: Vec<String>,
    saw_ballot: bool,
    ballot_submitted: bool,
    celebration_completed: bool,
    returned: bool,
    failures: u8,
}

fn aram_queue(mode: Option<&str>, queue: Option<i64>) -> Option<bool> {
    match mode.filter(|s| !s.is_empty()) {
        Some(mode) => Some(matches!(mode, "ARAM" | "KIWI")),
        None => queue
            .filter(|id| *id > 0)
            .map(|id| matches!(id, 450 | 2400)),
    }
}

// A disappearing session is common between the ballot and the scoreboard. Only
// a ballot with this game's ID can stand in for a fresh, identified ARAM session.
fn confirms_end_game(session: Option<&crate::lcu::GameflowSession>, turn: &Turn) -> Option<bool> {
    let session = session?;
    let id = session
        .game_data
        .as_ref()
        .and_then(|data| data.game_id)
        .filter(|id| *id > 0);
    let aram = aram_queue(
        session.aux_game_mode(),
        session.queue().and_then(|q| q.id).map(i64::from),
    );
    if id.is_some_and(|id| id != turn.game.id)
        || aram == Some(false)
        || Stage::parse(&session.phase) != Some(turn.stage)
    {
        return Some(false);
    }
    (id == Some(turn.game.id) && aram == Some(true)).then_some(true)
}

impl PostGame {
    pub(super) fn observe(&mut self, snapshot: &Snapshot, now: Instant) {
        let Some(client) = &snapshot.active else {
            if self
                .game
                .as_ref()
                .is_some_and(|g| snapshot.known_clients.contains(&g.instance))
            {
                self.stage = None;
            } else {
                *self = Self::default();
            }
            return;
        };
        let phase = client.phase.as_str();
        let stage = Stage::parse(phase);
        if stage.is_none()
            && !matches!(
                phase,
                "ChampSelect" | "GameStart" | "InProgress" | "Reconnect"
            )
        {
            *self = Self::default();
            return;
        }
        let data = &client.session["gameData"];
        let mode = aram_queue(
            data["queue"]["gameMode"].as_str(),
            data["queue"]["id"].as_i64(),
        );
        let id = data["gameId"].as_u64().filter(|id| *id > 0);
        let player = client.summoner["puuid"].as_str().filter(|s| !s.is_empty());
        if mode == Some(false)
            || self.game.as_ref().is_some_and(|g| {
                client.instance != g.instance
                    || id.is_some_and(|id| id != g.id)
                    || player.is_some_and(|p| !p.eq_ignore_ascii_case(&g.player))
            })
        {
            *self = Self::default();
        }
        if self.game.is_none() && mode == Some(true) {
            if let (Some(player), Some(id)) = (player, id) {
                self.game = Some(Game {
                    instance: client.instance,
                    player: player.to_ascii_lowercase(),
                    id,
                });
            }
        }
        let stage = stage.filter(|_| self.game.is_some() && player.is_some());
        if self.stage.map(|s| s.0) != stage {
            self.stage = stage.map(|s| (s, now));
            self.next_poll = None;
        }
    }

    fn current(&self, turn: &Turn) -> bool {
        self.game.as_ref() == Some(&turn.game) && self.stage.is_some_and(|s| s.0 == turn.stage)
    }

    fn turn(&mut self, options: &RoomOptions, now: Instant) -> Option<Turn> {
        if self.returned
            || !(options.play_again || (options.honor && !self.ballot_submitted))
            || self.next_poll.is_some_and(|due| now < due)
        {
            return None;
        }
        let (stage, _) = self.stage?;
        let game = self.game.clone()?;
        self.next_poll = Some(now + Duration::from_secs(1));
        Some(Turn { game, stage })
    }
}

fn select_recipients(ballot: &PostGameBallot, party: &[String], player: &str) -> Vec<String> {
    let mut recipients = Vec::new();
    let allies = &ballot.eligible_allies;
    for id in allies
        .iter()
        .filter(|id| party.contains(id))
        .chain(allies.iter().filter(|id| !party.contains(id)))
        .chain(
            ballot
                .eligible_opponents
                .iter()
                .filter(|id| !party.contains(id)),
        )
    {
        if recipients.len() >= ballot.votes as usize {
            break;
        }
        if !id.eq_ignore_ascii_case(player)
            && !ballot.honored_puuids.contains(id)
            && !recipients.contains(id)
        {
            recipients.push(id.clone());
        }
    }
    recipients
}

impl ClientRuntime {
    pub(super) fn post_game_turn(&self) -> Option<Turn> {
        let mut state = self.state.lock().ok()?;
        let state = &mut *state;
        state
            .post_game
            .turn(&state.room_flow.options, Instant::now())
    }

    fn with_post_game<R>(
        &self,
        generation: u64,
        turn: &Turn,
        f: impl FnOnce(&mut PostGame, &RoomOptions) -> R,
    ) -> Option<R> {
        let mut state = self.state.lock().ok()?;
        let state = &mut *state;
        if state.generation != generation
            || !state.post_game.current(turn)
            || !state
                .art_client
                .as_ref()
                .is_some_and(|c| c.instance == turn.game.instance && c.alive())
        {
            return None;
        }
        Some(f(&mut state.post_game, &state.room_flow.options))
    }

    pub(super) async fn process_post_game(
        &self,
        app: &AppHandle,
        client: &LocalClient,
        generation: u64,
        turn: Turn,
    ) {
        let Ok(_writer) = self.post_game_writes.try_lock() else {
            return;
        };
        if self.with_post_game(generation, &turn, |_, _| ()).is_none() {
            return;
        }
        let Ok(target) = client.target() else { return };
        let result = self.run_post_game(&target, generation, &turn).await;
        // A failing endpoint is retried while this match is current. Show one useful
        // error after consecutive failures, without generating a toast per poll.
        let notify = self.with_post_game(generation, &turn, |work, _| {
            if result.is_err() {
                work.failures = work.failures.saturating_add(1);
                work.next_poll = Some(Instant::now() + Duration::from_secs(2));
                work.failures == 3
            } else {
                work.failures = 0;
                false
            }
        });
        if let (Some(true), Err((event, error))) = (notify, result) {
            if let Some(window) = app.get_webview_window("main") {
                let _ = super::emit(
                    &window,
                    super::room_flow::NAMESPACE,
                    event,
                    serde_json::json!([error.to_string()]),
                );
            }
        }
    }

    async fn run_post_game(
        &self,
        target: &LcuTarget<'_>,
        generation: u64,
        turn: &Turn,
    ) -> Result<(), (&'static str, LcuError)> {
        const HONOR: &str = "error-auto-honor";
        const RETURN: &str = "error-play-again";
        let (phase, owner, session) = join3(
            self.transport.read_phase(target),
            self.transport.read_current_summoner(target),
            self.transport.read_gameflow_session(target),
        )
        .await;
        if phase
            .map_err(|e| (RETURN, e))?
            .as_deref()
            .and_then(Stage::parse)
            != Some(turn.stage)
            || !owner
                .map_err(|e| (RETURN, e))?
                .is_some_and(|s| s.puuid.eq_ignore_ascii_case(&turn.game.player))
        {
            return Ok(());
        }
        let session = session.map_err(|e| (RETURN, e))?;
        let flow = confirms_end_game(session.as_ref(), turn);
        if flow == Some(false) {
            return Ok(());
        }
        let Some((honor, play, submitted, celebration, previously_identified_ballot)) = self
            .with_post_game(generation, turn, |w, o| {
                (
                    o.honor,
                    o.play_again,
                    w.ballot_submitted,
                    w.celebration_completed,
                    w.saw_ballot,
                )
            })
        else {
            return Ok(());
        };
        if !honor && !play {
            return Ok(());
        }
        let ballot = if submitted {
            None
        } else {
            self.transport
                .read_post_game_ballot(target)
                .await
                .map_err(|e| (HONOR, e))?
        };
        if let Some(ballot) = &ballot {
            if ballot.game_id != turn.game.id {
                return Ok(());
            }
            self.with_post_game(generation, turn, |w, _| w.saw_ballot = true);
        } else if flow != Some(true) && !previously_identified_ballot {
            return Ok(());
        }
        if play
            && !celebration
            && turn.stage == Stage::PreEndOfGame
            && self.with_post_game(generation, turn, |_, o| o.play_again) == Some(true)
        {
            // A missing optional ceremony must not prevent voting or returning.
            if let Ok(true) = self.transport.complete_missions_celebration(target).await {
                self.with_post_game(generation, turn, |w, _| w.celebration_completed = true);
            }
        }
        if let Some(ballot) = ballot {
            if honor {
                self.finish_ballot(target, generation, turn, &ballot)
                    .await
                    .map_err(|e| (HONOR, e))?;
            }
            return Ok(());
        }
        let due = self.with_post_game(generation, turn, |w, o| {
            o.play_again
                && !w.returned
                && w.stage
                    .is_some_and(|(s, since)| since.elapsed() >= s.return_delay())
        });
        if due == Some(true) {
            self.transport
                .play_again(target)
                .await
                .map_err(|e| (RETURN, e))?;
            self.with_post_game(generation, turn, |w, _| w.returned = true);
        }
        Ok(())
    }

    async fn finish_ballot(
        &self,
        target: &LcuTarget<'_>,
        generation: u64,
        turn: &Turn,
        ballot: &PostGameBallot,
    ) -> Result<(), LcuError> {
        let needs_plan = self.with_post_game(generation, turn, |w, _| w.recipients.is_none());
        let party = if needs_plan == Some(true) {
            self.transport
                .read_post_game_party_members(target)
                .await?
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let Some(recipients) = self.with_post_game(generation, turn, |w, _| {
            let plan = w
                .recipients
                .get_or_insert_with(|| select_recipients(ballot, &party, &turn.game.player));
            plan.iter()
                .filter(|id| {
                    !w.accepted.contains(id)
                        && !ballot.honored_puuids.contains(id)
                        && (ballot.eligible_allies.contains(id)
                            || ballot.eligible_opponents.contains(id))
                })
                .take(ballot.votes as usize)
                .cloned()
                .collect::<Vec<_>>()
        }) else {
            return Ok(());
        };
        let can_vote = || {
            self.with_post_game(generation, turn, |w, o| o.honor && !w.ballot_submitted)
                == Some(true)
        };
        for id in recipients {
            if !can_vote() {
                return Ok(());
            }
            self.transport.submit_post_game_honor(target, &id).await?;
            self.with_post_game(generation, turn, |w, _| w.accepted.push(id));
        }
        if can_vote() {
            self.transport.submit_post_game_ballot(target).await?;
            self.with_post_game(generation, turn, |w, _| w.ballot_submitted = true);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_runtime::{ActiveClient, CompanionTargets};
    use crate::lcu::aux_activity::AuxActivity;
    use serde_json::{json, Value};

    const OWNER: &str = "owner";

    fn sample(phase: &str, data: Value, player: &str) -> Snapshot {
        let instance = ClientInstance {
            pid: 10,
            created_at: 20,
        };
        Snapshot {
            known_clients: vec![instance],
            active: Some(ActiveClient {
                instance,
                phase: phase.to_owned(),
                session: json!({"gameData":data}),
                summoner: json!({"puuid":player}),
                companions: CompanionTargets::default(),
                aux_activity: AuxActivity::Inactive,
                room: None,
                room_unconfirmed: false,
                shard: None,
            }),
        }
    }

    #[test]
    fn end_of_game_can_outlive_its_flow_session_but_never_the_account_game_or_queue() {
        let now = Instant::now();
        let options = RoomOptions {
            play_again: true,
            ..Default::default()
        };
        let known = json!({"queue":{"id":2400,"gameMode":"KIWI"},"gameId":42});
        for unsafe_next in [
            sample("EndOfGame", json!({"gameId":43}), OWNER),
            sample(
                "EndOfGame",
                json!({"queue":{"gameMode":"CLASSIC"},"gameId":42}),
                OWNER,
            ),
            sample("EndOfGame", Value::Null, "new-player"),
            sample("Lobby", Value::Null, OWNER),
            Snapshot::default(),
        ] {
            let mut state = PostGame::default();
            state.observe(&sample("InProgress", known.clone(), OWNER), now);
            assert!(state.turn(&options, now).is_none());
            state.observe(&sample("PreEndOfGame", Value::Null, OWNER), now);
            let accepted = state
                .turn(&options, now)
                .expect("same owner's finished KIWI game");
            assert_eq!(accepted.game.id, 42);
            state.observe(&unsafe_next, now);
            assert!(!state.current(&accepted));
            state.observe(&sample("EndOfGame", Value::Null, OWNER), now);
            assert!(state.turn(&options, now).is_none());
        }
    }

    #[test]
    fn zero_votes_and_previously_voted_recipients_do_not_spend_another_vote() {
        let mut ballot = PostGameBallot {
            game_id: 42,
            votes: 2,
            eligible_allies: vec![
                OWNER.into(),
                "already".into(),
                "ordinary".into(),
                "party".into(),
            ],
            eligible_opponents: vec!["party".into(), "opponent".into()],
            honored_puuids: vec!["already".into()],
        };
        let party = ["party".to_owned()];
        assert_eq!(
            select_recipients(&ballot, &party, OWNER),
            ["party", "ordinary"]
        );
        ballot.votes = 3;
        assert_eq!(
            select_recipients(&ballot, &party, OWNER),
            ["party", "ordinary", "opponent"]
        );
        ballot.votes = 0;
        assert!(select_recipients(&ballot, &party, OWNER).is_empty());
    }

    #[test]
    fn a_return_without_a_ballot_requires_a_fresh_matching_aram_scoreboard() {
        let turn = Turn {
            game: Game {
                id: 42,
                player: OWNER.into(),
                instance: ClientInstance {
                    pid: 10,
                    created_at: 20,
                },
            },
            stage: Stage::EndOfGame,
        };
        assert_eq!(confirms_end_game(None, &turn), None);
        for (phase, queue, id, expected) in [
            ("EndOfGame", "KIWI", 42, Some(true)),
            ("EndOfGame", "", 42, None),
            ("EndOfGame", "KIWI", 0, None),
            ("EndOfGame", "KIWI", 41, Some(false)),
            ("InProgress", "KIWI", 42, Some(false)),
            ("EndOfGame", "CLASSIC", 42, Some(false)),
        ] {
            let flow = serde_json::from_value(
                json!({"phase":phase,"gameData":{"gameId":id,"queue":{"gameMode":queue}}}),
            )
            .unwrap();
            assert_eq!(confirms_end_game(Some(&flow), &turn), expected);
        }
    }
}
