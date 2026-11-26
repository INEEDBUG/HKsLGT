//! A loopback HTTPS peer for exercising the real mini-window IPC and League transport.
//! The fixture owns its listening PID, issues its own temporary TLS certificate and never contacts a game.

use rustls::{
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
    ServerConfig, ServerConnection, StreamOwned,
};
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub(super) struct Scene {
    pub phase: &'static str,
    pub response: &'static str,
    pub champion: u32,
    pub bench: u32,
    pub choosing: bool,
    pub intent: u32,
    pub selected_skin: u32,
    pub rerolls_remaining: u32,
    pub reject_next: bool,
    pub writes: Vec<(String, String)>,
    pub partial_kiwi: Option<super::partial_kiwi_selection_smoke::DraftFixture>,
    pub position_reads: usize,
    pub reject_position_once: bool,
    pub post_game: Option<super::post_game_smoke::PostGameFixture>,
}

pub(super) struct MiniClient {
    pub port: u16,
    pub root: CertificateDer<'static>,
    pub scene: Arc<Mutex<Scene>>,
    pub history: Arc<super::history_responsiveness_smoke::HistoryFixture>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl MiniClient {
    pub fn start() -> Self {
        let pair = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let root = pair.cert.der().clone();
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![root.clone()],
                    PrivatePkcs8KeyDer::from(pair.signing_key.serialize_der()).into(),
                )
                .unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let scene = Arc::new(Mutex::new(Scene {
            phase: "Lobby",
            response: "None",
            champion: 22,
            bench: 103,
            choosing: false,
            intent: 0,
            selected_skin: 0,
            rerolls_remaining: 2,
            reject_next: false,
            writes: vec![],
            partial_kiwi: None,
            position_reads: 0,
            reject_position_once: false,
            post_game: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let history = Arc::new(super::history_responsiveness_smoke::HistoryFixture::default());
        let worker = {
            let (scene, stop, config) = (Arc::clone(&scene), Arc::clone(&stop), Arc::new(config));
            let history = Arc::clone(&history);
            thread::spawn(move || {
                thread::scope(|requests| {
                    for socket in listener.incoming().flatten() {
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        let concurrent = history.active();
                        let serve_one = || {
                            socket
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            let mut stream = StreamOwned::new(
                                ServerConnection::new(Arc::clone(&config)).unwrap(),
                                socket,
                            );
                            // The browser can cancel an image while the view switches phases.
                            let _ = serve(&mut stream, &scene, &history);
                        };
                        // A deliberately slow history page must not block the concurrent League
                        // requests for rank, identity or the companion window at this test server.
                        if concurrent {
                            requests.spawn(serve_one);
                        } else {
                            serve_one();
                        }
                    }
                })
            })
        };
        Self {
            port,
            root,
            scene,
            history,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for MiniClient {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        self.worker.take().unwrap().join().unwrap();
    }
}

pub(super) const PLAYER: &str = "11111111-2222-4333-8444-555555555555";
const OWNED_SKIN_PREVIEW: &str =
    "/lol-game-data/assets/ASSETS/Characters/Ahri/Skins/Skin01/Images/ahri_splash_centered_1.jpg";
pub(super) const OWNED_SKIN_ID: u32 = 103001;
pub(super) const LEAVE_TARGET: &str = concat!(
    "/lol-login/v1/session/invoke?destination=lcdsServiceProxy&method=call",
    "&args=%5B%22%22%2C+%22teambuilder-draft%22%2C+%22quitV2%22%2C+%22%22%5D"
);

fn serve(
    stream: &mut StreamOwned<ServerConnection, TcpStream>,
    scene: &Mutex<Scene>,
    history: &super::history_responsiveness_smoke::HistoryFixture,
) -> io::Result<()> {
    let mut reader = BufReader::new(&mut *stream);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let mut request = first.split_whitespace();
    let method = request.next().unwrap_or("");
    let path = request.next().unwrap_or("");
    let mut size = 0;
    let mut authenticated = false;
    for header in (&mut reader).lines() {
        let header = header?;
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                size = value.trim().parse().unwrap_or(0);
            }
            if name.eq_ignore_ascii_case("authorization") {
                authenticated = value.trim() == "Basic cmlvdDpmaXh0dXJlLW9ubHk=";
            }
        }
    }
    if size > 65536 {
        return Err(io::Error::other("unexpected fixture request size"));
    }
    let mut body = vec![0; size];
    reader.read_exact(&mut body)?;
    let history_art = if authenticated && method == "GET" {
        if let Some((status, response)) = history.get(path) {
            let bytes = serde_json::to_vec(&response)?;
            write!(stream,"HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len())?;
            stream.write_all(&bytes)?;
            return stream.flush();
        }
        history.art(path)
    } else {
        None
    };
    let mut scene = scene.lock().unwrap();
    let (status, mime, bytes) = if !authenticated {
        (401, "application/json", vec![])
    } else if method == "GET" && (path == OWNED_SKIN_PREVIEW || history_art == Some("image/jpeg")) {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD.decode(concat!(
            "/9j/4AAQSkZJRgABAQEAYABgAAD/2wBDAAMCAgMCAgMDAwMEAwMEBQgFBQQEBQoHBwYIDAoMDAsKCwsNDhIQDQ4RDgsLEBYQERMUFRUVDA8XGBYUGBIUFRT/",
            "2wBDAQMEBAUEBQkFBQkUDQsNFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBT/wAARCAACAAMDASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/",
            "8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/",
            "8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwD8zn+JnjCV2d/Fetu7HJZtRmJJ9fvUUUVzVPjl6nyWL/3mp/if5n//2Q=="
        )).expect("synthetic fixture JPEG");
        (200, "image/jpeg", bytes)
    } else if method == "GET"
        && path.starts_with("/lol-game-data/assets/")
        && path.ends_with(".png")
    {
        let portrait = path
            .strip_prefix("/lol-game-data/assets/v1/champion-icons/")
            .and_then(|name| name.strip_suffix(".png"))
            .and_then(|id| id.parse::<u32>().ok());
        let legacy_black = super::champion_picker_smoke::requested()
            && matches!(portrait, Some(60022 | 60103 | 60117));
        let bytes = if legacy_black {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.decode(
                "iVBORw0KGgoAAAANSUhEUgAAAAMAAAACCAIAAAASFvFNAAAADklEQVR4nGPghQEGOAsACTgA6+Drjf4AAAAASUVORK5CYII="
            ).expect("synthetic legacy portrait")
        } else { portrait.and_then(|id| {
            let dir = std::env::var_os("HKSLGT_SMOKE_PORTRAIT_DIR")?;
            std::fs::read(std::path::PathBuf::from(dir).join(format!("{id}.png"))).ok()
        }).unwrap_or_else(|| {
            use base64::Engine as _;
            // Hand-generated three-color PNG for offline CI. Local acceptance can supply actual portraits.
            base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAMAAAACCAIAAAASFvFNAAAAFUlEQVR4nGO4pMEvt8BNK+8EA5wFAEKIBtsqErd8AAAAAElFTkSuQmCC").unwrap()
        }) };
        (200, "image/png", bytes)
    } else if method == "GET" && super::champion_picker_smoke::requested()
        && path == "/lol-perks/v1/recommended-champion-positions"
        && std::mem::take(&mut scene.reject_position_once)
    {
        scene.position_reads += 1;
        (503, "application/json", vec![])
    } else if method == "GET" {
        match scene.get(path) {
            Some(json) => (200, "application/json", serde_json::to_vec(&json).unwrap()),
            None => (404, "application/json", vec![]),
        }
    } else {
        let status = scene.mutate(method, path, &body);
        (status, "application/json", vec![])
    };
    drop(scene);
    write!(stream,"HTTP/1.1 {status} Fixture\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len())?;
    stream.write_all(&bytes)?;
    stream.flush()
}

impl Scene {
    fn get(&mut self, path: &str) -> Option<Value> {
        if let Some(game) = self.post_game.as_ref() {
            match path {
                "/lol-chat/v1/me" => return Some(json!({"puuid":PLAYER,"platformId":"HN1"})),
                "/lol-gameflow/v1/session" => return Some(game.flow(self.phase)),
                "/lol-honor-v2/v1/ballot/" => return game.ballot(self.phase),
                "/lol-lobby/v2/party/eog-status" => return Some(game.party()),
                "/lol-pre-end-of-game/v1/currentSequenceEvent" => return game.sequence(),
                _ => {}
            }
        }
        if super::champion_picker_smoke::requested() {
            match path {
                "/lol-chat/v1/me" => return Some(json!({"puuid":PLAYER,"platformId":"HN1"})),
                "/lol-game-data/assets/v1/champion-summary.json" =>
                    return Some(super::champion_picker_smoke::champion_summary()),
                "/lol-perks/v1/recommended-champion-positions" => {
                    self.position_reads += 1;
                    return Some(super::champion_picker_smoke::recommended_positions());
                }
                _ => {}
            }
        }
        if let Some(draft) = self.partial_kiwi.as_ref() {
            match path {
                "/lol-gameflow/v1/gameflow-phase" if draft.phase_missing => return None,
                "/lol-summoner/v1/current-summoner" if draft.profile_loading => return Some(json!({"ready":false})),
                "/lol-gameflow/v1/session" => return draft.gameflow.clone(),
                "/lol-champ-select/v1/session" => return draft.selection.clone(),
                "/lol-champ-select/v1/current-champion" => {
                    return draft.current_champion.map(|id| json!(id));
                }
                _ => {}
            }
        }
        Some(match path {
            "/lol-summoner/v1/current-summoner" => {
                json!({"puuid":PLAYER,"summonerId":1,"gameName":"Window test","tagLine":"TEST"})
            }
            "/lol-gameflow/v1/gameflow-phase" => json!(self.phase),
            "/lol-gameflow/v1/session" => {
                json!({"phase":self.phase,"gameData":{"gameId":0,"isCustomGame":false,"queue":{"id":450,"name":"ARAM","gameMode":"ARAM","type":"ARAM"}},"map":{"name":"Howling Abyss","gameMode":"ARAM","assets":{"game-select-icon-hover":"/lol-game-data/assets/content/src/LeagueClient/GameModeAssets/ARAM/img/game-select-icon-hover.png"}}})
            }
            "/lol-lobby/v2/lobby" => {
                json!({"partyId":"fixture-party","gameConfig":{"queueId":450,"gameMode":"ARAM","isCustom":false},"canStartActivity":true,
                "localMember":{"puuid":PLAYER,"isLeader":true},"members":[{"puuid":PLAYER,"isLeader":true},{"summonerId":2}],"invitations":[]})
            }
            "/lol-matchmaking/v1/search" => {
                json!({"queueId":450,"searchState":if self.phase=="Lobby" {"Invalid"} else {"Searching"},"timeInQueue":27.9,"estimatedQueueTime":54.25,"readyCheck":{"state":"InProgress","playerResponse":self.response}})
            }
            "/lol-matchmaking/v1/ready-check" => {
                json!({"state":"InProgress","playerResponse":self.response})
            }
            "/lol-champ-select/v1/current-champion" if self.phase == "ChampSelect" => {
                json!(self.champion)
            }
            "/lol-champ-select/v1/session" if self.phase == "ChampSelect" => {
                json!({"queueId":450,"gameId":0,"isSpectating":false,"isCustomGame":false,"localPlayerCellId":2,
                "myTeam":[{"cellId":1,"championId":55},{"cellId":2,"puuid":PLAYER,"championId":self.champion,"championPickIntent":self.intent}],"theirTeam":[],
                "benchEnabled":true,"benchChampions":[{"championId":self.bench}],"allowRerolling":true,"rerollsRemaining":self.rerolls_remaining,
                "allowSubsetChampionPicks":self.choosing,
                "timer":{"phase":if self.choosing {"BAN_PICK"} else {"FINALIZATION"},"adjustedTimeLeftInPhase":17810},
                "actions":if self.choosing {json!([[{"id":17,"type":"pick","actorCellId":2,"isInProgress":true,"completed":false,"championId":self.intent}]])} else {json!([])}})
            }
            "/lol-champ-select/v1/pickable-champion-ids" => json!([22, 103, 55]),
            "/lol-champ-select/v1/skin-selector-info" if self.champion == 103 => {
                json!({"selectedChampionId":103,"selectedSkinId":if self.selected_skin==0 {103000} else {self.selected_skin},"showSkinSelector":true,"skinSelectionDisabled":false})
            }
            "/lol-champ-select/v1/skin-carousel-skins" if self.champion == 103 => {
                json!([
                    {"id":103000,"name":"Fixture base Ahri","unlocked":true,"disabled":false},
                    {"id":OWNED_SKIN_ID,"name":"Fixture owned Ahri","unlocked":true,"disabled":false,"splashPath":OWNED_SKIN_PREVIEW},
                    {"id":103099,"name":"Fixture locked Ahri","unlocked":false,"disabled":false}
                ])
            }
            "/lol-lobby-team-builder/champ-select/v1/subset-champion-list" => {
                if self.choosing {
                    json!([22, 103])
                } else {
                    json!([])
                }
            }
            "/lol-champ-select/v1/all-grid-champions" => {
                json!([{"id":22,"freeToPlay":true,"owned":true},{"id":103,"freeToPlay":true,"owned":true}])
            }
            "/lol-game-data/assets/v1/champion-summary.json" => {
                json!([{"id":22,"alias":"Ashe","name":"Ashe"},{"id":103,"alias":"Ahri","name":"Ahri"},{"id":55,"alias":"Katarina","name":"Katarina"}])
            }
            "/lol-game-data/assets/v1/queues.json" => json!([{"id":450,"name":"ARAM"}]),
            _ => return None,
        })
    }

    fn mutate(&mut self, method: &str, path: &str, body: &[u8]) -> u16 {
        self.writes.push((method.into(), path.into()));
        if let Some(game) = self.post_game.as_mut() {
            if let Some((status, phase)) = game.apply(self.phase, method, path, body) {
                self.phase = phase;
                return status;
            }
        }
        if std::mem::take(&mut self.reject_next) {
            return 409;
        }
        match (self.phase, method, path) {
            ("Lobby", "POST", "/lol-lobby/v2/lobby/matchmaking/search") => {
                self.phase = "Matchmaking"
            }
            ("Matchmaking", "DELETE", "/lol-lobby/v2/lobby/matchmaking/search") => {
                self.phase = "Lobby"
            }
            ("ReadyCheck", "POST", "/lol-matchmaking/v1/ready-check/accept") => {
                self.response = "Accepted"
            }
            ("ReadyCheck", "POST", "/lol-matchmaking/v1/ready-check/decline") => {
                self.response = "Declined"
            }
            ("ChampSelect", "PATCH", "/lol-champ-select/v1/session/actions/17")
                if self.choosing =>
            {
                let payload: Value = serde_json::from_slice(body).unwrap();
                let Some(id) = payload["championId"]
                    .as_u64()
                    .filter(|id| [22, 103].contains(id))
                else {
                    return 409;
                };
                self.intent = id as u32;
                if payload["completed"] == true {
                    self.choosing = false;
                    self.champion = id as u32;
                    self.bench = if id == 22 { 103 } else { 22 };
                }
            }
            ("ChampSelect", "POST", target)
                if target == format!("/lol-champ-select/v1/session/bench/swap/{}", self.bench) =>
            {
                std::mem::swap(&mut self.champion, &mut self.bench);
                self.selected_skin = 0;
            }
            ("ChampSelect", "POST", "/lol-champ-select/v1/session/my-selection/reroll")
                if !self.choosing && self.rerolls_remaining > 0 =>
            {
                self.bench = self.champion;
                self.champion = if self.champion == 55 { 22 } else { 55 };
                self.selected_skin = 0;
                self.rerolls_remaining -= 1;
            }
            ("ChampSelect", "PATCH", "/lol-champ-select/v1/session/my-selection")
                if self.champion == 103
                    && serde_json::from_slice::<Value>(body)
                        .is_ok_and(|body| body == json!({"selectedSkinId":OWNED_SKIN_ID})) =>
            {
                self.selected_skin = OWNED_SKIN_ID;
            }
            ("ChampSelect", "POST", LEAVE_TARGET)
                if serde_json::from_slice::<Value>(body).is_ok_and(|body| {
                    body == json!({"data":["", "teambuilder-draft", "quitV2", ""]})
                }) => {}
            _ => return 404,
        }
        204
    }
}
