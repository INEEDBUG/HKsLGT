//! Exercise post-game settings and their fixed LCU requests through the real Windows runtime.
use super::{
    check_renderer, mini_client_fixture::PLAYER, AppHandle, ClientRuntime, DesktopProfile, Manager,
    MiniClient, WebviewWindow,
};
use crate::client_discovery::LocalClient;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tauri::{WebviewUrl, WebviewWindowBuilder};

const PARTY_ALLY: &str = "11111111-2222-4333-8444-555555555556";
const OTHER_ALLY: &str = "11111111-2222-4333-8444-555555555557";
const ROBOT: &str = "11111111-2222-4333-8444-555555555558";
const ALREADY_HONORED: &str = "11111111-2222-4333-8444-555555555559";
const OPPONENT: &str = "11111111-2222-4333-8444-555555555560";
const HONOR: &str = "/lol-honor/v1/honor";
const BALLOT: &str = "/lol-honor/v1/ballot";
const PLAY_AGAIN: &str = "/lol-lobby/v2/play-again";
const MISSION: &str = "/lol-pre-end-of-game/v1/complete/missions-celebration";
const PRE_END: &str = "PreEndOfGame";
const END: &str = "EndOfGame";
const SETTINGS: &str = "auto-gameflow-main";

pub(super) fn requested() -> bool {
    std::env::var_os("HKSLGT_POST_GAME_ONLY").is_some()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Write {
    path: &'static str,
    status: u16,
    recipient: Option<String>,
}

pub(super) struct PostGameFixture {
    game_id: u64,
    pending_votes: u32,
    honored: Vec<String>,
    fail_first_vote: bool,
    mission_done: bool,
    ballot_posted: bool,
    played_again: bool,
    writes: Vec<Write>,
}

impl PostGameFixture {
    fn new(game_id: u64, fail_first_vote: bool) -> Self {
        Self {
            game_id,
            pending_votes: 2,
            honored: vec![ALREADY_HONORED.to_owned()],
            fail_first_vote,
            mission_done: false,
            ballot_posted: false,
            played_again: false,
            writes: Vec::new(),
        }
    }

    pub fn flow(&self, phase: &str) -> Value {
        json!({"phase":phase,"gameData":{"gameId":self.game_id,"isCustomGame":false,
            "queue":{"id":450,"name":"ARAM","gameMode":"ARAM","type":"ARAM"}}})
    }

    pub fn ballot(&self, phase: &str) -> Option<Value> {
        if self.ballot_posted || !matches!(phase, PRE_END | END) {
            return None;
        }
        let entry = |id: &str, bot: bool| json!({"puuid":id,"botPlayer":bot});
        Some(json!({
            "gameId":self.game_id,
            "eligibleAllies":[entry(PLAYER,false),entry(ROBOT,true),entry(ALREADY_HONORED,false),
                entry(OTHER_ALLY,false),entry(PARTY_ALLY,false)],
            "eligibleOpponents":[entry(OPPONENT,false)],
            "honoredPlayers":self.honored.iter().map(|id| json!({"honorType":"HEART","recipientPuuid":id})).collect::<Vec<_>>(),
            "votePool":{"fromGamePlayed":2,"fromHighHonor":0,"fromRecentHonors":0,"fromRollover":0,"votes":self.pending_votes}
        }))
    }

    pub fn party(&self) -> Value {
        json!({"eogPlayers":[PLAYER],"readyPlayers":[PARTY_ALLY],"leftPlayers":[],"partySize":2})
    }

    pub fn sequence(&self) -> Option<Value> {
        (!self.mission_done).then(|| json!({"name":"missions-celebration"}))
    }

    pub fn apply(
        &mut self,
        phase: &'static str,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Option<(u16, &'static str)> {
        if method != "POST" {
            return None;
        }
        let (canonical, status, next, recipient) = match path {
            MISSION => {
                let status = if phase == PRE_END && !self.mission_done {
                    self.mission_done = true;
                    204
                } else {
                    409
                };
                (MISSION, status, phase, None)
            }
            HONOR => {
                let data: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
                let recipient = data["recipientPuuid"].as_str();
                let valid = phase == PRE_END
                    && !self.ballot_posted
                    && self.pending_votes > 0
                    && data["honorType"] == "HEART"
                    && matches!(recipient, Some(PARTY_ALLY | OTHER_ALLY))
                    && !self
                        .honored
                        .iter()
                        .any(|old| Some(old.as_str()) == recipient);
                let status = if !valid {
                    409
                } else if std::mem::take(&mut self.fail_first_vote) {
                    503
                } else {
                    self.honored.push(recipient.unwrap().to_owned());
                    self.pending_votes -= 1;
                    204
                };
                (HONOR, status, phase, recipient.map(str::to_owned))
            }
            BALLOT => {
                let allowed = phase == PRE_END && self.pending_votes == 0 && !self.ballot_posted;
                if allowed {
                    self.ballot_posted = true;
                }
                (
                    BALLOT,
                    if allowed { 204 } else { 409 },
                    if allowed { END } else { phase },
                    None,
                )
            }
            PLAY_AGAIN => {
                let allowed =
                    phase == END && self.ballot_posted && self.mission_done && !self.played_again;
                if allowed {
                    self.played_again = true;
                }
                (
                    PLAY_AGAIN,
                    if allowed { 204 } else { 409 },
                    if allowed { "Lobby" } else { phase },
                    None,
                )
            }
            _ => return None,
        };
        self.writes.push(Write {
            path: canonical,
            status,
            recipient,
        });
        Some((status, next))
    }
}

async fn set_switch(
    main: &WebviewWindow,
    app: &AppHandle,
    section: &str,
    key: &str,
    expected: bool,
) {
    let control = format!(
        "Array.from(document.querySelectorAll('.settings-section')).find(s => s.querySelector('.settings-section-title')?.textContent.trim() === {})?.querySelector('[role=switch]')",
        json!(section)
    );
    check_renderer(
        main,
        &format!(
            "({control})?.getAttribute('aria-checked') === '{}'",
            !expected
        ),
        "the post-game stock setting before click",
        || {},
    )
    .await;
    main.eval(format!("({control}).click()")).unwrap();
    check_renderer(
        main,
        &format!("({control})?.getAttribute('aria-checked') === '{expected}'"),
        "the post-game stock setting after click",
        || {},
    )
    .await;
    let profile = app.state::<DesktopProfile>();
    let db = profile.database.lock().unwrap();
    assert_eq!(
        super::bootstrap::backend_pref(SETTINGS, key)
            .unwrap()
            .read(&db)
            .unwrap(),
        json!(expected)
    );
}

pub(super) async fn check(app: &AppHandle) {
    let server = app.state::<MiniClient>();
    let runtime = app.state::<ClientRuntime>();
    let profile = app.state::<DesktopProfile>();
    super::bootstrap::backend_pref("self-update-main", "autoCheckUpdates")
        .unwrap()
        .write(&profile.database.lock().unwrap(), &json!(false))
        .unwrap();
    let client =
        Arc::new(LocalClient::fixture_at(server.port).expect("bind owned post-game TLS listener"));
    {
        let mut scene = server.scene.lock().unwrap();
        scene.phase = PRE_END;
        scene.post_game = Some(PostGameFixture::new(9111, true));
    }
    let main = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("main-window.html".into()))
        .data_directory(profile.directory.join("PostGameWebView2"))
        .inner_size(1120.0, 840.0)
        .decorations(false)
        .on_navigation(super::super::bundled_navigation)
        .build()
        .expect("create isolated post-game WebView2");
    runtime.sample_fixture(app, &client).await;
    check_renderer(
        &main,
        "!!document.querySelector('.menu-item[data-key=automation]')",
        "normal main automation navigation",
        || {},
    )
    .await;
    main.eval("document.querySelector('.menu-item[data-key=automation]').click()")
        .unwrap();
    assert!(
        server.scene.lock().unwrap().writes.is_empty(),
        "both default-disabled settings must leave League untouched"
    );
    set_switch(&main, app, "自动点赞", "autoHonorEnabled", true).await;
    set_switch(&main, app, "返回房间", "playAgainEnabled", true).await;
    let completed = tokio::time::timeout(Duration::from_secs(16), async {
        loop {
            runtime.sample_fixture(app, &client).await;
            if server
                .scene
                .lock()
                .unwrap()
                .post_game
                .as_ref()
                .unwrap()
                .played_again
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(175)).await;
        }
    })
    .await;
    let writes = server
        .scene
        .lock()
        .unwrap()
        .post_game
        .as_ref()
        .unwrap()
        .writes
        .clone();
    assert!(
        completed.is_ok(),
        "real post-game sequence did not finish: {writes:?}"
    );
    let honors: Vec<_> = writes
        .iter()
        .filter(|write| write.path == HONOR)
        .map(|write| (write.status, write.recipient.as_deref()))
        .collect();
    assert_eq!(
        honors,
        [
            (503, Some(PARTY_ALLY)),
            (204, Some(PARTY_ALLY)),
            (204, Some(OTHER_ALLY))
        ]
    );
    assert_eq!(
        writes
            .iter()
            .filter(|write| write.path == MISSION && write.status == 204)
            .count(),
        1
    );
    assert_eq!(
        writes
            .iter()
            .filter(|write| write.path == BALLOT && write.status == 204)
            .count(),
        1
    );
    assert_eq!(
        writes
            .iter()
            .filter(|write| write.path == PLAY_AGAIN && write.status == 204)
            .count(),
        1
    );
    assert_eq!(writes.len(), 6, "no extra votes, confirmations or requeues");
    assert!(
        writes.iter().position(|write| write.path == BALLOT)
            < writes.iter().position(|write| write.path == PLAY_AGAIN)
    );

    // A duplicated end-of-game observation must not replay any successful writes.
    server.scene.lock().unwrap().phase = END;
    for _ in 0..3 {
        runtime.sample_fixture(app, &client).await;
    }
    assert_eq!(
        server
            .scene
            .lock()
            .unwrap()
            .post_game
            .as_ref()
            .unwrap()
            .writes,
        writes
    );
    set_switch(&main, app, "自动点赞", "autoHonorEnabled", false).await;
    {
        let mut scene = server.scene.lock().unwrap();
        scene.phase = END;
        scene.post_game = Some(PostGameFixture::new(9222, false));
    }
    runtime.sample_fixture(app, &client).await;
    tokio::time::sleep(Duration::from_millis(1750)).await;
    runtime.sample_fixture(app, &client).await;
    assert!(
        server
            .scene
            .lock()
            .unwrap()
            .post_game
            .as_ref()
            .unwrap()
            .writes
            .is_empty(),
        "return-to-room must wait for an active manual ballot"
    );
    set_switch(&main, app, "返回房间", "playAgainEnabled", false).await;
    {
        let mut scene = server.scene.lock().unwrap();
        scene.phase = PRE_END;
        scene.post_game = Some(PostGameFixture::new(9333, false));
    }
    for _ in 0..3 {
        runtime.sample_fixture(app, &client).await;
    }
    assert!(server
        .scene
        .lock()
        .unwrap()
        .post_game
        .as_ref()
        .unwrap()
        .writes
        .is_empty());
    main.destroy().expect("close the isolated post-game window");
    println!("HKSLGT_POST_GAME_PASS: normal UI preferences, first vote recovered from HTTP 503, party ally before other ally, one confirmation before one return, and no writes with a manual ballot or both settings off");
}
