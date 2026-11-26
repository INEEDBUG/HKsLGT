use super::*;
use serde_json::json;

fn snapshot(
    phase: AuxPhase,
    queue: u64,
    local: &str,
    lobby: &Value,
    search: Option<&Value>,
) -> RoomSample {
    let config = json!({"queueId": queue});
    let mut lobby = lobby.clone();
    lobby["gameConfig"] = config;
    RoomSample::new(
        ClientInstance {
            pid: 44,
            created_at: 1234,
        },
        phase,
        local,
        Some(&lobby),
        search,
        true,
    )
}

fn party() -> Value {
    json!({"partyId":"test-party", "canStartActivity":true,
        "members":[{"puuid":"tester","isLeader":true}],
        "invitations":[]})
}

#[test]
fn search_status_honors_the_actual_leader_penalty_pending_invite_and_party_size_in_order() {
    let options = RoomOptions {
        search: true,
        minimum_members: 2,
        ..Default::default()
    };
    let mut lobby = party();
    lobby["invitations"] = json!([{"state":"Pending"}]);
    let penalty = json!({"errors":[{"penaltyTimeRemaining":0},{"penaltyTimeRemaining":4.25}]});
    let status = |lobby: &Value, search: Option<&Value>| {
        snapshot(AuxPhase::Lobby, 450, "tester", lobby, search).status(&options)
    };
    lobby["members"][0]["isLeader"] = json!(false);
    assert_eq!(status(&lobby, Some(&penalty)), "not-the-leader");
    lobby["members"][0]["isLeader"] = json!(true);
    assert_eq!(status(&lobby, Some(&penalty)), "waiting-for-penalty-time");
    assert_eq!(status(&lobby, None), "waiting-for-invitees");
    lobby["invitations"][0]["state"] = json!("Accepted");
    assert_eq!(status(&lobby, None), "insufficient-members");
    lobby["members"]
        .as_array_mut()
        .unwrap()
        .push(json!({"puuid":"friend","isLeader":false}));
    assert_eq!(status(&lobby, None), "can-start-activity");
    lobby["canStartActivity"] = json!(false);
    assert_eq!(status(&lobby, None), "cannot-start-activity");
}

#[test]
fn cancellation_survives_unchanged_polls_and_rearms_for_new_lobby_or_ready_check() {
    let mut room = RoomFlow::default();
    room.options.search = true;
    room.options.accept = true;
    room.options.search_delay = Duration::from_secs(5);
    room.options.accept_delay = Duration::from_secs(2);
    let now = Instant::now();
    let lobby = party();
    let room_sample = snapshot(AuxPhase::Lobby, 450, "tester", &lobby, None);
    assert_eq!(room.observe(Some(&room_sample), now, 1000), None);
    assert_eq!(room.fields()["willSearchMatchAt"], 6000);
    room.cancel(RoomAction::StartMatchmaking);
    assert_eq!(
        room.observe(Some(&room_sample), now + Duration::from_secs(8), 9000),
        None
    );
    let ready = snapshot(
        AuxPhase::ReadyCheck,
        450,
        "tester",
        &lobby,
        Some(&json!({"readyCheck":{"playerResponse":"None"}})),
    );
    assert_eq!(room.observe(Some(&ready), now, 1000), None);
    assert_eq!(room.fields()["willAcceptAt"], 3000);
    assert_eq!(
        room.observe(Some(&ready), now + Duration::from_secs(2), 3000),
        Some(RoomAction::AcceptReadyCheck)
    );
    room.cancel(RoomAction::AcceptReadyCheck);
    assert_eq!(
        room.observe(Some(&ready), now + Duration::from_secs(3), 4000),
        None
    );
    room.observe(Some(&room_sample), now, 1000);
    assert_eq!(
        room.observe(Some(&room_sample), now + Duration::from_secs(5), 6000),
        Some(RoomAction::StartMatchmaking)
    );
}

#[test]
fn an_answered_check_and_a_changed_party_never_consume_a_previous_timer() {
    let mut room = RoomFlow::default();
    room.options.accept = true;
    room.options.accept_delay = Duration::from_secs(8);
    let now = Instant::now();
    let mut lobby = party();
    let waiting = json!({"readyCheck":{"playerResponse":"None"}});
    let accepted = json!({"readyCheck":{"playerResponse":"Accepted"}});
    let a = snapshot(AuxPhase::ReadyCheck, 2400, "tester", &lobby, Some(&waiting));
    room.observe(Some(&a), now, 100);
    let b = snapshot(
        AuxPhase::ReadyCheck,
        2400,
        "tester",
        &lobby,
        Some(&accepted),
    );
    assert_eq!(
        room.observe(Some(&b), now + Duration::from_secs(8), 8100),
        None
    );
    assert_eq!(room.fields()["willAcceptAt"], -1);
    lobby["partyId"] = json!("second-party");
    let second = snapshot(AuxPhase::ReadyCheck, 2400, "tester", &lobby, Some(&waiting));
    assert_eq!(
        room.observe(Some(&second), now + Duration::from_secs(9), 9100),
        None
    );
    assert_eq!(room.fields()["willAcceptAt"], 17100);
    room.observe(None, now, 100);
    assert_eq!(room.fields()["willAcceptAt"], -1);
}

#[test]
fn a_manual_answer_prevents_a_second_or_conflicting_answer_in_the_same_check() {
    let mut room = RoomFlow::default();
    room.options.accept = true;
    let check = snapshot(
        AuxPhase::ReadyCheck,
        2400,
        "tester",
        &party(),
        Some(&json!({"readyCheck":{"playerResponse":"None"}})),
    );
    let now = Instant::now();
    assert_eq!(
        room.observe(Some(&check), now, 500),
        Some(RoomAction::AcceptReadyCheck)
    );
    assert!(room.current(&check, RoomAction::DeclineReadyCheck, false));
    room.mark_sent(RoomAction::DeclineReadyCheck);
    assert!(!room.current(&check, RoomAction::AcceptReadyCheck, true));
    assert!(!room.current(&check, RoomAction::AcceptReadyCheck, false));
    assert_eq!(room.observe(Some(&check), now, 500), None);
    room.manual_retry_after_error(&check, RoomAction::DeclineReadyCheck);
    assert!(room.current(&check, RoomAction::AcceptReadyCheck, false));
    assert!(!room.current(&check, RoomAction::AcceptReadyCheck, true));
}

#[test]
fn enabling_auto_accept_during_a_ready_check_applies_to_the_next_check() {
    let mut room = RoomFlow::default();
    let waiting = snapshot(
        AuxPhase::ReadyCheck,
        450,
        "tester",
        &party(),
        Some(&json!({"readyCheck":{"playerResponse":"None"}})),
    );
    let now = Instant::now();
    assert_eq!(room.observe(Some(&waiting), now, 100), None);
    room.options.accept = true;
    assert_eq!(room.observe(Some(&waiting), now, 100), None);
    let searching = snapshot(AuxPhase::Matchmaking, 450, "tester", &party(), None);
    room.observe(Some(&searching), now, 100);
    assert_eq!(
        room.observe(Some(&waiting), now, 100),
        Some(RoomAction::AcceptReadyCheck)
    );
}

#[test]
fn either_manual_response_can_be_reversed_after_the_client_confirms_it() {
    let now = Instant::now();
    let mut room = RoomFlow::default();
    let lobby = party();
    let sample = |response| {
        snapshot(
            AuxPhase::ReadyCheck,
            450,
            "tester",
            &lobby,
            Some(&json!({"readyCheck":{"state":"InProgress","playerResponse":response}})),
        )
    };
    let pending = sample("None");
    room.observe(Some(&pending), now, 100);
    room.mark_sent(RoomAction::AcceptReadyCheck);
    assert!(!room.current(&pending, RoomAction::DeclineReadyCheck, false));
    let accepted = sample("Accepted");
    assert!(accepted.allows(RoomAction::DeclineReadyCheck));
    assert!(!accepted.allows(RoomAction::AcceptReadyCheck));
    assert!(room.current(&accepted, RoomAction::DeclineReadyCheck, false));
    room.mark_sent(RoomAction::DeclineReadyCheck);
    assert!(!room.current(&accepted, RoomAction::DeclineReadyCheck, false));
    let declined = sample("Declined");
    assert!(declined.allows(RoomAction::AcceptReadyCheck));
    assert!(room.current(&declined, RoomAction::AcceptReadyCheck, false));
    assert!(!room.current(&declined, RoomAction::AcceptReadyCheck, true));
}

#[test]
fn a_failed_penalty_read_cannot_start_a_search_and_invitee_wait_can_be_disabled() {
    let mut lobby = party();
    lobby["invitations"] = json!([{"state":"Pending"}]);
    let options = RoomOptions {
        search: true,
        wait_for_invitees: false,
        ..Default::default()
    };
    let room = snapshot(AuxPhase::Lobby, 2400, "tester", &lobby, None);
    assert_eq!(
        room.automatic_action(&options),
        Some(RoomAction::StartMatchmaking)
    );
    let unread = RoomSample::new(
        room.instance,
        AuxPhase::Lobby,
        "tester",
        Some(&lobby),
        None,
        false,
    );
    assert_eq!(unread.automatic_action(&options), None);
    let unknown_player = snapshot(AuxPhase::Lobby, 2400, "another", &lobby, None);
    assert_eq!(unknown_player.automatic_action(&options), None);
}

#[test]
fn an_unconfirmed_poll_does_not_repeat_a_write_or_revive_a_user_cancelled_timer() {
    let mut room = RoomFlow::default();
    room.options.accept = true;
    let now = Instant::now();
    let pending = snapshot(
        AuxPhase::ReadyCheck,
        450,
        "tester",
        &party(),
        Some(&json!({"readyCheck":{"playerResponse":"None"}})),
    );
    room.observe(Some(&pending), now, 10);
    room.mark_sent(RoomAction::AcceptReadyCheck);
    room.suspend();
    assert_eq!(room.fields()["activityStartStatus"], "unavailable");
    assert_eq!(room.observe(Some(&pending), now, 10), None);
    room.observe(None, now, 10);
    room.observe(Some(&pending), now, 10);
    room.cancel(RoomAction::AcceptReadyCheck);
    room.suspend();
    assert_eq!(room.observe(Some(&pending), now, 10), None);
}
