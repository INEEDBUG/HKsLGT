//! Timers and eligibility for the stock ARAM lounge's opt-in room controls.

use crate::{
    client_discovery::ClientInstance,
    lcu::{aux_activity::AuxPhase, RoomAction},
};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub(crate) const NAMESPACE: &str = "auto-gameflow-main";

pub(crate) fn is_room_setting(key: &str) -> bool {
    matches!(
        key,
        "autoAcceptEnabled"
            | "autoAcceptDelaySeconds"
            | "autoHonorEnabled"
            | "playAgainEnabled"
            | "autoMatchmakingEnabled"
            | "autoMatchmakingDelaySeconds"
            | "autoMatchmakingMinimumMembers"
            | "autoMatchmakingWaitForInvitees"
    )
}

pub(crate) fn valid_number(key: &str, value: &Value) -> bool {
    match key {
        "autoAcceptDelaySeconds" | "autoMatchmakingDelaySeconds" => value
            .as_f64()
            .is_some_and(|value| Duration::try_from_secs_f64(value).is_ok()),
        "autoMatchmakingMinimumMembers" => {
            value.as_f64().is_some_and(|n| (1.0..=99.0).contains(&n))
        }
        _ => true,
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RoomOptions {
    pub(crate) search: bool,
    pub(crate) accept: bool,
    pub(crate) honor: bool,
    pub(crate) play_again: bool,
    pub(crate) search_delay: Duration,
    pub(crate) accept_delay: Duration,
    pub(crate) minimum_members: usize,
    pub(crate) wait_for_invitees: bool,
}

impl Default for RoomOptions {
    fn default() -> Self {
        Self {
            search: false,
            accept: false,
            honor: false,
            play_again: false,
            search_delay: Duration::from_secs(5),
            accept_delay: Duration::ZERO,
            minimum_members: 1,
            wait_for_invitees: true,
        }
    }
}

impl RoomOptions {
    pub(crate) fn from_settings(settings: &Value) -> Self {
        let delay = |name: &str, fallback| {
            settings[name]
                .as_f64()
                .and_then(|n| Duration::try_from_secs_f64(n).ok())
                .unwrap_or(fallback)
        };
        let defaults = Self::default();
        Self {
            search: settings["autoMatchmakingEnabled"] == true,
            accept: settings["autoAcceptEnabled"] == true,
            honor: settings["autoHonorEnabled"] == true,
            play_again: settings["playAgainEnabled"] == true,
            search_delay: delay("autoMatchmakingDelaySeconds", defaults.search_delay),
            accept_delay: delay("autoAcceptDelaySeconds", defaults.accept_delay),
            minimum_members: settings["autoMatchmakingMinimumMembers"]
                .as_f64()
                .filter(|n| (1.0..=99.0).contains(n))
                .map_or(defaults.minimum_members, |n| n.ceil() as usize),
            wait_for_invitees: settings["autoMatchmakingWaitForInvitees"]
                .as_bool()
                .unwrap_or(defaults.wait_for_invitees),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ReadyResponse {
    Pending,
    Accepted,
    Declined,
}

#[derive(Clone, PartialEq)]
pub(crate) struct RoomSample {
    instance: ClientInstance,
    player: String,
    phase: AuxPhase,
    party: Option<String>,
    queue: Option<u64>,
    leader: Option<bool>,
    members: usize,
    invitations_pending: Option<bool>,
    penalty: Option<bool>,
    can_start: Option<bool>,
    ready: Option<ReadyResponse>,
    ready_round: u64,
    ready_game_id: Option<u64>,
}

impl RoomSample {
    pub(crate) fn new(
        instance: ClientInstance,
        phase: AuxPhase,
        player: &str,
        lobby: Option<&Value>,
        search: Option<&Value>,
        search_read: bool,
    ) -> Self {
        let members = lobby.and_then(|lobby| lobby["members"].as_array());
        let leader = members
            .and_then(|members| members.iter().find(|m| m["puuid"].as_str() == Some(player)))
            .and_then(|m| m["isLeader"].as_bool());
        let invitations_pending = lobby
            .and_then(|l| l["invitations"].as_array())
            .map(|invitations| invitations.iter().any(|i| i["state"] == "Pending"));
        let penalty = search_read.then(|| {
            search
                .and_then(|s| s["errors"].as_array())
                .is_some_and(|errors| {
                    errors
                        .iter()
                        .any(|e| e["penaltyTimeRemaining"].as_f64().is_some_and(|n| n > 0.0))
                })
        });
        let ready = search.and_then(|s| {
            let check = &s["readyCheck"];
            if matches!(check["state"].as_str(), Some("Invalid" | "EveryoneReady")) {
                return None;
            }
            match check["playerResponse"].as_str() {
                Some("None") => Some(ReadyResponse::Pending),
                Some("Accepted") => Some(ReadyResponse::Accepted),
                Some("Declined") => Some(ReadyResponse::Declined),
                _ => None,
            }
        });
        Self {
            instance,
            player: player.to_owned(),
            phase,
            leader,
            invitations_pending,
            penalty,
            ready,
            ready_round: 0,
            ready_game_id: None,
            party: lobby
                .and_then(|l| l["partyId"].as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            queue: lobby
                .and_then(|l| l["gameConfig"]["queueId"].as_u64())
                .or_else(|| search.and_then(|s| s["queueId"].as_u64()))
                .filter(|q| *q != 0),
            members: members.map_or(0, Vec::len),
            can_start: lobby.and_then(|l| l["canStartActivity"].as_bool()),
        }
    }

    pub(crate) fn with_ready_round(mut self, number: u64, game_id: Option<u64>) -> Self {
        if self.phase == AuxPhase::ReadyCheck {
            self.ready_round = number;
            self.ready_game_id = game_id;
        }
        self
    }

    pub(crate) fn round_is_current(&self, number: u64, closed: bool) -> bool {
        self.phase != AuxPhase::ReadyCheck || (!closed && self.ready_round == number)
    }

    pub(crate) fn same_room(&self, other: &Self) -> bool {
        self.instance == other.instance
            && self.player == other.player
            && self.phase == other.phase
            && self.ready_round == other.ready_round
            && self
                .ready_game_id
                .zip(other.ready_game_id)
                .is_none_or(|(left, right)| left == right)
            && self
                .party
                .as_ref()
                .zip(other.party.as_ref())
                .is_none_or(|(a, b)| a == b)
            && self.queue.zip(other.queue).is_none_or(|(a, b)| a == b)
    }

    fn status(&self, options: &RoomOptions) -> &'static str {
        match self.leader {
            None => return "unavailable",
            Some(false) => return "not-the-leader",
            Some(true) => {}
        }
        match self.penalty {
            None => return "unavailable",
            Some(true) => return "waiting-for-penalty-time",
            Some(false) => {}
        }
        if options.wait_for_invitees {
            match self.invitations_pending {
                None => return "unavailable",
                Some(true) => return "waiting-for-invitees",
                Some(false) => {}
            }
        }
        if self.members < options.minimum_members {
            return "insufficient-members";
        }
        if self.can_start == Some(true) {
            "can-start-activity"
        } else {
            "cannot-start-activity"
        }
    }

    fn automatic_action(&self, options: &RoomOptions) -> Option<RoomAction> {
        match self.phase {
            AuxPhase::Lobby if options.search && self.status(options) == "can-start-activity" => {
                Some(RoomAction::StartMatchmaking)
            }
            AuxPhase::ReadyCheck
                if options.accept && self.ready == Some(ReadyResponse::Pending) =>
            {
                Some(RoomAction::AcceptReadyCheck)
            }
            _ => None,
        }
    }

    pub(crate) fn allows(&self, action: RoomAction) -> bool {
        match action {
            RoomAction::StartMatchmaking => {
                self.phase == AuxPhase::Lobby
                    && self.leader == Some(true)
                    && self.can_start == Some(true)
            }
            RoomAction::CancelMatchmaking => self.phase == AuxPhase::Matchmaking,
            RoomAction::AcceptReadyCheck => {
                self.phase == AuxPhase::ReadyCheck
                    && matches!(
                        self.ready,
                        Some(ReadyResponse::Pending | ReadyResponse::Declined)
                    )
            }
            RoomAction::DeclineReadyCheck => {
                self.phase == AuxPhase::ReadyCheck
                    && matches!(
                        self.ready,
                        Some(ReadyResponse::Pending | ReadyResponse::Accepted)
                    )
            }
        }
    }
}

struct Pending {
    action: RoomAction,
    since: Instant,
    delay: Duration,
    at: i64,
}

#[derive(Default)]
pub(crate) struct RoomFlow {
    pub(crate) options: RoomOptions,
    sample: Option<RoomSample>,
    trigger: Option<RoomAction>,
    pending: Option<Pending>,
    sent: Option<RoomAction>,
    accept_on_entry: bool,
    suspended: bool,
}

impl RoomFlow {
    pub(crate) fn fields(&self) -> Value {
        let at = |action| {
            self.pending
                .as_ref()
                .filter(|p| p.action == action)
                .map_or(-1, |p| p.at)
        };
        json!({
            "willAcceptAt": at(RoomAction::AcceptReadyCheck), "willSearchMatchAt": at(RoomAction::StartMatchmaking),
            "willSearchMatch": at(RoomAction::StartMatchmaking) >= 0,
            "activityStartStatus": self.sample.as_ref().filter(|_| !self.suspended).map_or("unavailable", |s| s.status(&self.options)),
            "friendsToBeInvited": [],
        })
    }

    pub(crate) fn observe(
        &mut self,
        sample: Option<&RoomSample>,
        now: Instant,
        epoch_ms: i64,
    ) -> Option<RoomAction> {
        self.suspended = false;
        if !self
            .sample
            .as_ref()
            .zip(sample)
            .is_some_and(|(old, new)| old.same_room(new))
        {
            self.pending = None;
            self.trigger = None;
            self.sent = None;
            self.accept_on_entry = self.options.accept;
        }
        if !self.options.accept {
            self.accept_on_entry = false;
        }
        self.sample = sample.cloned();
        let action = sample
            .and_then(|s| s.automatic_action(&self.options))
            .filter(|a| *a != RoomAction::AcceptReadyCheck || self.accept_on_entry);
        if self.trigger != action {
            self.trigger = action;
            self.pending = action.filter(|_| self.sent.is_none()).map(|action| {
                let delay = if action == RoomAction::AcceptReadyCheck {
                    self.options.accept_delay
                } else {
                    self.options.search_delay
                };
                Pending {
                    action,
                    since: now,
                    delay,
                    at: epoch_ms
                        .saturating_add(i64::try_from(delay.as_millis()).unwrap_or(i64::MAX)),
                }
            });
        }
        self.pending
            .as_ref()
            .filter(|p| now.saturating_duration_since(p.since) >= p.delay)
            .map(|p| p.action)
    }

    pub(crate) fn next_poll(&self, now: Instant) -> Option<Duration> {
        self.pending.as_ref().map(|p| {
            let left = p
                .delay
                .saturating_sub(now.saturating_duration_since(p.since));
            if left.is_zero() {
                // A due confirmation whose final read was inconclusive can retry without spinning.
                Duration::from_millis(250)
            } else {
                left
            }
        })
    }

    pub(crate) fn tracked_client(&self) -> Option<ClientInstance> {
        self.sample.as_ref().map(|sample| sample.instance)
    }

    pub(crate) fn tracks_stage(
        &self,
        instance: ClientInstance,
        phase: &str,
        player: Option<&str>,
    ) -> bool {
        self.sample.as_ref().is_some_and(|sample| {
            sample.instance == instance
                && player.is_none_or(|player| player == sample.player)
                && matches!(
                    (sample.phase, phase),
                    (AuxPhase::Lobby, "Lobby")
                        | (AuxPhase::Matchmaking, "Matchmaking")
                        | (AuxPhase::ReadyCheck, "ReadyCheck")
                )
        })
    }

    pub(crate) fn suspend(&mut self) {
        self.suspended = true;
        if self.pending.take().is_some() {
            self.trigger = None;
        }
    }

    pub(crate) fn cancel(&mut self, action: RoomAction) {
        if self.pending.as_ref().is_some_and(|p| p.action == action) {
            self.pending = None;
        }
    }

    pub(crate) fn current(&self, fresh: &RoomSample, action: RoomAction, automatic: bool) -> bool {
        // A confirmed response unlocks the opposite stock button while this check is still open.
        let change_answer = !automatic
            && matches!(
                (self.sent, fresh.ready, action),
                (
                    Some(RoomAction::AcceptReadyCheck),
                    Some(ReadyResponse::Accepted),
                    RoomAction::DeclineReadyCheck
                ) | (
                    Some(RoomAction::DeclineReadyCheck),
                    Some(ReadyResponse::Declined),
                    RoomAction::AcceptReadyCheck
                )
            );
        !self.suspended
            && self.sample.as_ref().is_some_and(|s| s.same_room(fresh))
            && (self.sent.is_none() || change_answer)
            && (!automatic
                || (fresh.automatic_action(&self.options) == Some(action)
                    && self.pending.as_ref().is_some_and(|p| p.action == action)))
    }

    pub(crate) fn mark_sent(&mut self, action: RoomAction) {
        self.sent = Some(action);
        self.pending = None;
    }

    pub(crate) fn manual_retry_after_error(&mut self, fresh: &RoomSample, action: RoomAction) {
        if self.sent == Some(action) && self.sample.as_ref().is_some_and(|s| s.same_room(fresh)) {
            self.sent = None;
        }
    }
}

#[cfg(test)]
#[path = "room_flow_tests.rs"]
mod tests;
