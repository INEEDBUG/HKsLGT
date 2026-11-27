//! Enter the bundled player tab while its real same-shard HTTPS request is held open.
use super::{
    bootstrap, check_renderer, mini_client_fixture::MiniClient, set_preference, ClientRuntime,
    DesktopProfile, AUX_WINDOW,
};
use crate::{client_discovery::LocalClient, client_runtime::reconcile_companions};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetWindowThreadProcessId, SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_BLOCK,
    WM_NULL,
};

const PLAYER: &str = "11111111-2222-4333-8444-555555555555";
const GAMES: &str =
    "/lol-match-history/v1/products/lol/11111111-2222-4333-8444-555555555555/matches";
const BUDGET: Duration = Duration::from_millis(1500);
const FIRST_GAME: u64 = 8_050_001;
const PAGE_SIZE: u64 = 20;
const NEW_GAME: u64 = FIRST_GAME + PAGE_SIZE;
const KIWI_GAME: u64 = NEW_GAME - 1;
const BITMAP_PROBE_GAME: u64 = KIWI_GAME - 1;
const ASSETS: &str = "/lol-game-data/assets/";
const PROFILE: &str = "v1/profile-icons/29.jpg";
const TEAMMATE_PROFILE: &str = "v1/profile-icons/0.jpg";
const ITEMS: [(u32, &str); 7] = [
    (
        3006,
        "ASSETS/Items/Icons2D/3006_Class_T2_BerserkersGreaves.png",
    ),
    (
        6672,
        "ASSETS/Items/Icons2D/6672_Marksman_T4_BehemothSlayer.png",
    ),
    (3085, "ASSETS/Items/Icons2D/3085_Marksman_T3_Runaans.png"),
    (
        3031,
        "ASSETS/Items/Icons2D/3031_Marksman_T3_InfinityEdge.png",
    ),
    (
        3036,
        "ASSETS/Items/Icons2D/3036_Marksman_T3_DominikRegards.png",
    ),
    (
        3072,
        "ASSETS/Items/Icons2D/3072_Fighter_T3_Bloodthirster.png",
    ),
    (2052, "ASSETS/Items/Icons2D/2052_Poro_Snack.png"),
];
const FLASH: &str = "DATA/Spells/Icons2D/Summoner_flash.png";
const MARK: &str = "DATA/Spells/Icons2D/Summoner_Mark.png";
const PERK: &str = "v1/perk-images/Styles/Precision/PressTheAttack/PressTheAttack.png";
const PERK_STYLE: &str = "v1/perk-images/Styles/7200_Domination.png";
const BITMAP_PROBE: &str = "v1/perk-images/Styles/7202_Sorcery.png";
const AUGMENT: &str = "ASSETS/Maps/Particles/Kiwi/MagicMissile_small.png";
const LATE_CATALOGS: [&str; 3] = ["augments", "items", "spells"];
const INITIAL_CATALOG_ATTEMPTS: usize = 3;
const QUEUE_BLOCKERS: [&str; 4] = [
    "v1/champion-icons/266.png",
    "v1/champion-icons/222.png",
    "v1/champion-icons/67.png",
    "v1/champion-icons/238.png",
];
const QUEUED_IMAGE: &str = "v1/champion-icons/412.png";
const QUEUE_HOLD: Duration = Duration::from_millis(3200);
const CDN_ONLY: &str = "HKSLGT_HISTORY_CDN_ONLY";
const CDN_MISSING_ITEM: u32 = 6672;
const CDN_LOCAL_MISSING: [&str; 6] = [PROFILE, ITEMS[0].1, FLASH, PERK, PERK_STYLE, AUGMENT];
const CDN_IMAGES: [(&str, u16); 8] = [
    (
        "https://game.gtimg.cn/images/lol/act/img/item/3006.png",
        200,
    ),
    (
        "https://game.gtimg.cn/images/lol/act/img/item/6672.png",
        200,
    ),
    (
        "https://game.gtimg.cn/images/lol/act/img/spell/SummonerFlash.png",
        200,
    ),
    (
        "https://game.gtimg.cn/images/lol/act/img/rune/8005.png",
        200,
    ),
    (
        "https://game.gtimg.cn/images/lol/act/img/rune/7200_Domination.png",
        200,
    ),
    (
        "https://game.gtimg.cn/images/lol/act/img/rune/magicmissile_small.png",
        200,
    ),
    (
        "https://game.gtimg.cn/images/lol/act/img/profileicon/29.png",
        404,
    ),
    (
        "https://ddragon.leagueoflegends.com/cdn/16.23.1/img/profileicon/29.png",
        200,
    ),
];

fn expected_art() -> impl Iterator<Item = &'static str> {
    ITEMS.into_iter().map(|(_, path)| path).chain([
        PROFILE,
        TEAMMATE_PROFILE,
        FLASH,
        MARK,
        PERK,
        PERK_STYLE,
        BITMAP_PROBE,
        AUGMENT,
    ])
}

fn asset_path(path: &str) -> String {
    format!("{ASSETS}{path}")
}

// LCU list responses contain the requesting participant. The game endpoint supplies both teams.
fn game(id: u64, full: bool) -> Value {
    let mut participants = Vec::new();
    let mut identities = Vec::new();
    let hour = id - FIRST_GAME;
    let own_kills = if id == NEW_GAME { 13 } else { 12 };
    for slot in 1..=if full { 10 } else { 1 } {
        let champion = [22, 103, 55, 86, 111, 1, 45, 99, 75, 21][slot - 1];
        let player = if slot == 1 {
            PLAYER.to_owned()
        } else {
            format!("10000000-0000-4000-8000-{slot:012}")
        };
        let name = if slot == 1 {
            "History Fixture".to_owned()
        } else {
            format!("Fixture player {slot}")
        };
        identities.push(json!({"participantId":slot,"player":{"puuid":player,"currentPlatformId":"HN1","platformId":"HN1",
            "accountId":41+slot,"summonerId":41+slot,"gameName":name,"summonerName":name,"tagLine":"MOCK",
            "profileIcon":if slot == 2 {0} else {29}}}));
        let mut stats = json!({"participantId":slot,"win":slot<=5,"kills":if slot==1 {own_kills} else {6},
            "deaths":3,"assists":19,"champLevel":18,"goldEarned":18900,"totalDamageDealtToChampions":34567,
            "totalDamageTaken":19500,"totalMinionsKilled":41,"neutralMinionsKilled":0,"magicDamageDealtToChampions":1500,
            "physicalDamageDealtToChampions":32567,"trueDamageDealtToChampions":500,"totalDamageDealt":66000,
            "physicalDamageTaken":1000,"magicalDamageTaken":12000,"trueDamageTaken":500,"totalHeal":0,
            "totalUnitsHealed":0,"visionScore":0,"perkPrimaryStyle":8000,
            "perkSubStyle":if id == BITMAP_PROBE_GAME && slot == 1 {8200} else {8100}});
        let fields = stats.as_object_mut().unwrap();
        for (index, (item, _)) in ITEMS.into_iter().enumerate() {
            fields.insert(
                format!("item{index}"),
                json!(if id == NEW_GAME && index == 0 {
                    0
                } else {
                    item
                }),
            );
        }
        for (index, perk) in [8005, 8009, 9104, 8014, 8139, 8106].into_iter().enumerate() {
            fields.insert(format!("perk{index}"), json!(perk));
        }
        if id == KIWI_GAME {
            fields.insert("playerAugment1".into(), json!(1133));
        }
        participants.push(
            json!({"participantId":slot,"teamId":if slot<=5 {100} else {200},
            "championId":champion,"spell1Id":4,"spell2Id":32,
            "timeline":{"participantId":slot,"lane":"NONE","role":"NONE"},"stats":stats}),
        );
    }
    json!({"gameId":id,"gameCreation":1_795_138_400_000_u64 + hour*3_600_000,
        "gameCreationDate":format!("2026-11-20T{:02}:33:20.000Z", 1+hour),"gameDuration":1380,
        "gameMode":if id == KIWI_GAME {"KIWI"} else {"ARAM"},"gameType":"MATCHED_GAME",
        "mapId":12,"queueId":if id == KIWI_GAME {2400} else {450},"platformId":"HN1","gameVersion":"16.22.1","seasonId":0,"endOfGameResult":"GameComplete",
        "gameModeMutators":[],"participantIdentities":identities,"participants":participants,
        "teams":[{"teamId":100,"win":"Win","bans":[]},{"teamId":200,"win":"Fail","bans":[]}]})
}

fn timeline() -> Value {
    let frames: Vec<_> = (0..24).map(|minute| {
        let participants: serde_json::Map<_,_> = (1..=10).map(|id| (id.to_string(),json!({
            "participantId":id,"level":1+(minute*2/3),"currentGold":300,"totalGold":500+minute*500,
            "xp":minute*500,"minionsKilled":minute*2,"jungleMinionsKilled":0,"position":{"x":500+id*10,"y":750+id*10}
        }))).collect();
        json!({"timestamp":minute*60000,"participantFrames":participants,
            "events":if minute==10 {json!([{"type":"CHAMPION_KILL","timestamp":600000,"killerId":1,"victimId":6,"assistingParticipantIds":[2]}])} else {json!([])}})
    }).collect();
    json!({"frameInterval":60000,"frames":frames})
}

#[derive(Default)]
struct HistoryRequests {
    hold: bool,
    fail: bool,
    pages: usize,
    failed_pages: usize,
    new_game_reads: usize,
    champion_images: usize,
    history_art: BTreeSet<&'static str>,
    cdn_case: bool,
    local_cdn_failures: BTreeSet<&'static str>,
    bitmap_probe_responses: Vec<u16>,
    artwork_queue: Option<ArtworkQueue>,
    catalog_reads: BTreeMap<&'static str, usize>,
    recover_catalogs: bool,
    ranked: usize,
    profiles: usize,
    details: usize,
    timelines: usize,
    in_flight: usize,
    peak: usize,
    hold_details: bool,
    timed_out: bool,
}

#[derive(Default)]
struct ArtworkQueue {
    blockers: BTreeSet<&'static str>,
    first_read: Option<Instant>,
    released: bool,
    queued_reads: usize,
    timed_out: bool,
}

#[derive(Default)]
pub(super) struct HistoryFixture {
    active: AtomicBool,
    requests: Mutex<HistoryRequests>,
    release: Condvar,
}

impl HistoryFixture {
    pub(super) fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn begin(&self) {
        *self.requests.lock().unwrap() = HistoryRequests {
            hold: true,
            hold_details: true,
            ..Default::default()
        };
        self.active.store(true, Ordering::Release);
    }

    fn begin_cdn(&self) {
        *self.requests.lock().unwrap() = HistoryRequests {
            cdn_case: true,
            ..Default::default()
        };
        self.active.store(true, Ordering::Release);
    }

    fn release_page(&self) {
        self.requests.lock().unwrap().hold = false;
        self.release.notify_all();
    }

    fn end(&self) {
        self.active.store(false, Ordering::Release);
        let mut state = self.requests.lock().unwrap();
        state.hold_details = false;
        state.recover_catalogs = true;
        drop(state);
        self.release_page();
    }

    fn release_catalogs(&self) {
        self.requests.lock().unwrap().recover_catalogs = true;
    }

    pub(super) fn hold_queue_artwork(&self, path: &str) -> bool {
        let Some(relative) = path.strip_prefix(ASSETS) else {
            return false;
        };
        let blocker = QUEUE_BLOCKERS.into_iter().find(|&image| image == relative);
        if blocker.is_none() && relative != QUEUED_IMAGE {
            return false;
        }
        let mut requests = self.requests.lock().unwrap();
        let Some(queue) = requests.artwork_queue.as_mut() else {
            return false;
        };
        let Some(blocker) = blocker else {
            queue.queued_reads += 1;
            return true;
        };
        queue.first_read.get_or_insert_with(Instant::now);
        queue.blockers.insert(blocker);
        let (mut requests, elapsed) = self
            .release
            .wait_timeout_while(requests, Duration::from_secs(4), |state| {
                state
                    .artwork_queue
                    .as_ref()
                    .is_some_and(|queue| !queue.released)
            })
            .unwrap();
        if elapsed.timed_out() {
            requests.artwork_queue.as_mut().unwrap().timed_out = true;
        }
        true
    }

    pub(super) fn art(&self, path: &str) -> Option<(u16, &'static str)> {
        if !self.active() {
            return None;
        }
        let relative = path.strip_prefix(ASSETS)?;
        let mut requests = self.requests.lock().unwrap();
        if requests.cdn_case {
            if let Some(asset) = CDN_LOCAL_MISSING
                .into_iter()
                .find(|&asset| asset == relative)
            {
                requests.local_cdn_failures.insert(asset);
                return Some((404, "application/json"));
            }
            let known_portrait = relative
                .strip_prefix("v1/champion-icons/")
                .and_then(|file| file.strip_suffix(".png"))
                .and_then(|id| id.parse::<u32>().ok())
                .is_some_and(|id| [1, 21, 22, 45, 55, 75, 86, 99, 103, 111].contains(&id));
            if (relative.ends_with(".png") || relative.ends_with(".jpg"))
                && !known_portrait
                && !expected_art().any(|asset| asset == relative && asset != ITEMS[1].1)
            {
                return Some((404, "application/json"));
            }
        }
        let asset = expected_art().find(|asset| asset.eq_ignore_ascii_case(relative))?;
        if asset == BITMAP_PROBE && requests.bitmap_probe_responses.is_empty() {
            requests.bitmap_probe_responses.push(503);
            println!("HKSLGT_HISTORY_BITMAP_503: {asset}");
            return Some((503, "application/json"));
        }
        if asset == BITMAP_PROBE {
            requests.bitmap_probe_responses.push(200);
        }
        requests.history_art.insert(asset);
        let mime = if asset.ends_with(".jpg") {
            "image/jpeg"
        } else {
            "image/png"
        };
        Some((200, mime))
    }

    pub(super) fn get(&self, path: &str) -> Option<(u16, Value)> {
        if !self.active() {
            return None;
        }
        let mut requests = self.requests.lock().unwrap();
        let catalog = match path {
            "/lol-game-data/assets/v1/champion-summary.json" => Some("champions"),
            "/lol-game-data/assets/v1/perks.json" => Some("perks"),
            "/lol-game-data/assets/v1/perkstyles.json" => Some("perkstyles"),
            "/lol-game-data/assets/v1/items.json" => Some("items"),
            "/lol-game-data/assets/v1/summoner-spells.json" => Some("spells"),
            "/lol-game-data/assets/v1/cherry-augments.json" => Some("augments"),
            _ => None,
        };
        if let Some(catalog) = catalog {
            let attempts = requests.catalog_reads.entry(catalog).or_default();
            *attempts += 1;
            let attempt = *attempts;
            if !requests.cdn_case
                && LATE_CATALOGS.contains(&catalog)
                && (attempt <= INITIAL_CATALOG_ATTEMPTS || !requests.recover_catalogs)
            {
                println!("HKSLGT_HISTORY_CATALOG_503: {catalog} attempt={attempt}");
                return Some((503, json!({"message":"Fixture catalog still loading"})));
            }
        }
        let details = path
            .strip_prefix("/lol-match-history/v1/games/")
            .map(|id| (id, false))
            .or_else(|| {
                path.strip_prefix("/lol-match-history/v1/game-timelines/")
                    .map(|id| (id, true))
            });
        if let Some((id, is_timeline)) = details {
            let id: u64 = id.parse().ok()?;
            if !(FIRST_GAME..=NEW_GAME).contains(&id) {
                return None;
            }
            if is_timeline {
                requests.timelines += 1;
            } else {
                requests.details += 1;
                if id == NEW_GAME {
                    requests.new_game_reads += 1;
                    println!(
                        "HKSLGT_HISTORY_NEW_GAME: renderer requested fresh game {id} after retry"
                    );
                }
            }
            requests.in_flight += 1;
            requests.peak = requests.peak.max(requests.in_flight);
            let (mut state, elapsed) = self
                .release
                .wait_timeout_while(requests, Duration::from_secs(7), |r| r.hold_details)
                .unwrap();
            if elapsed.timed_out() {
                state.timed_out = true;
            }
            drop(state);
            thread::sleep(Duration::from_millis(75));
            let data = if is_timeline {
                timeline()
            } else {
                game(id, true)
            };
            self.requests.lock().unwrap().in_flight -= 1;
            return Some((200, data));
        }
        match path.split('?').next()? {
            asset if asset.starts_with("/lol-game-data/assets/v1/champion-icons/") => {
                if requests.pages > 0 {
                    requests.champion_images += 1;
                }
                None // MiniClient serves the actual image response.
            }
            "/lol-chat/v1/me" => Some((200, json!({"puuid":PLAYER,"platformId":"HN1"}))),
            "/lol-summoner/v1/current-summoner" => {
                requests.profiles += 1;
                Some((
                    200,
                    json!({"puuid":PLAYER,"accountId":42,"summonerId":42,"gameName":"History Fixture","displayName":"History Fixture",
                    "internalName":"History Fixture","tagLine":"MOCK","profileIconId":29,"summonerLevel":45,
                    "xpSinceLastLevel":30,"xpUntilNextLevel":70,"privacy":"PUBLIC"}),
                ))
            }
            "/lol-game-data/assets/v1/items.json" => Some((200, json!(ITEMS.into_iter().filter(|(id, _)| !requests.cdn_case || *id != CDN_MISSING_ITEM).map(|(id,path)| json!({
                "id":id,"name":format!("Fixture item {id}"),"iconPath":asset_path(path),
                "description":"<mainText>Fixture item</mainText>","from":[],"to":[],"price":500,"priceTotal":2800
            })).collect::<Vec<_>>()))),
            "/lol-game-data/assets/v1/summoner-spells.json" => Some((200, json!([
                {"id":4,"name":"Flash","iconPath":asset_path(FLASH),"description":"Blink briefly.","summonerLevel":7,"cooldown":300,"gameModes":["ARAM","KIWI"]},
                {"id":32,"name":"Mark","iconPath":asset_path(MARK),"description":"Mark an opponent.","summonerLevel":1,"cooldown":80,"gameModes":["ARAM","KIWI"]}
            ]))),
            "/lol-game-data/assets/v1/perks.json" => Some((200, json!([
                {"id":8005,"name":"Press the Attack","iconPath":asset_path(PERK),"longDesc":"Repeated attacks apply damage.","endOfGameStatDescs":[]}
            ]))),
            "/lol-game-data/assets/v1/perkstyles.json" => Some((200, json!({"schemaVersion":2,"styles":[
                {"id":8100,"name":"Domination","iconPath":asset_path(PERK_STYLE),"tooltip":"Burst damage","assetMap":{}},
                {"id":8200,"name":"Sorcery","iconPath":asset_path(BITMAP_PROBE),"tooltip":"Spell damage","assetMap":{}}
            ]}))),
            "/lol-game-data/assets/v1/cherry-augments.json" => Some((200, json!([
                {"id":1133,"augmentNameId":"ARAM_MagicMissile","nameTRA":"Magic Missile","simpleNameTRA":"",
                    "augmentSmallIconPath":asset_path(AUGMENT),"rarity":"kGold"}
            ]))),
            "/lol-ranked/v1/ranked-stats/11111111-2222-4333-8444-555555555555" => {
                requests.ranked += 1;
                Some((200, json!({"queues":[],"queueMap":{}})))
            }
            GAMES => {
                requests.pages += 1;
                if requests.cdn_case {
                    let games = [NEW_GAME, KIWI_GAME].map(|id| game(id, false));
                    return Some((200, json!({"accountId":42,"platformId":"HN1","games":{
                        "gameIndexBegin":0,"gameIndexEnd":1,"gameCount":2,"games":games
                    }})));
                }
                let (mut state, elapsed) = self
                    .release
                    .wait_timeout_while(requests, Duration::from_secs(7), |r| r.hold)
                    .unwrap();
                if elapsed.timed_out() {
                    state.timed_out = true;
                }
                if state.fail {
                    state.failed_pages += 1;
                    println!(
                        "HKSLGT_HISTORY_HTTP: page={} returned HTTP 503",
                        state.pages
                    );
                    return Some((503, json!({"message":"Synthetic history unavailable"})));
                }
                let oldest = FIRST_GAME + u64::from(state.failed_pages > 0);
                let newest = oldest + PAGE_SIZE - 1;
                println!("HKSLGT_HISTORY_HTTP: page={} returned HTTP 200, newestGame={newest} oldestGame={oldest}", state.pages);
                drop(state);
                let games: Vec<_> = (oldest..=newest).rev().map(|id| game(id, false)).collect();
                Some((
                    200,
                    json!({"accountId":42,"platformId":"HN1","games":{
                        "gameIndexBegin":0,"gameIndexEnd":19,"gameCount":PAGE_SIZE,"games":games
                    }}),
                ))
            }
            _ => None,
        }
    }
}

fn child_window_count(parent: *mut std::ffi::c_void) -> usize {
    unsafe extern "system" fn count(_: *mut std::ffi::c_void, data: isize) -> i32 {
        // EnumChildWindows invokes this synchronously on the caller's thread.
        unsafe { *(data as *mut usize) += 1 };
        1
    }
    let mut children = 0_usize;
    unsafe { EnumChildWindows(parent, Some(count), (&mut children as *mut usize) as isize) };
    children
}

struct StopWatchdog(Arc<AtomicBool>);

impl Drop for StopWatchdog {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn watchdog(
    hwnd: usize,
    stop: Arc<AtomicBool>,
    stage: Arc<Mutex<&'static str>>,
    entered: Instant,
) -> thread::JoinHandle<(usize, Duration)> {
    thread::spawn(move || {
        let hwnd = hwnd as *mut std::ffi::c_void;
        let mut owner = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut owner) };
        assert_eq!(
            owner,
            std::process::id(),
            "main HWND must belong to this fixture"
        );
        let mut checks = 0;
        let mut worst = Duration::ZERO;
        let mut max_children = child_window_count(hwnd);
        let mut last_stage = None;
        let mut stage_checks = 0;
        let mut stage_worst = Duration::ZERO;
        while !stop.load(Ordering::Acquire) {
            let start = Instant::now();
            let section = *stage.lock().unwrap();
            let children = child_window_count(hwnd);
            max_children = max_children.max(children);
            if last_stage != Some(section) {
                if let Some(previous) = last_stage {
                    println!("HKSLGT_HISTORY_NATIVE_ACK: stage={previous}, pings={stage_checks}, worst={stage_worst:?}");
                }
                stage_checks = 0;
                stage_worst = Duration::ZERO;
                println!("HKSLGT_HISTORY_CHILD_HWND: stage={section}, elapsed={:?}, children={children}, maxChildren={max_children}", start.duration_since(entered));
                last_stage = Some(section);
            }
            let mut reply = 0;
            let delivered = unsafe {
                SendMessageTimeoutW(
                    hwnd,
                    WM_NULL,
                    0,
                    0,
                    SMTO_ABORTIFHUNG | SMTO_BLOCK,
                    BUDGET.as_millis() as u32,
                    &mut reply,
                )
            };
            if delivered == 0 {
                let at_timeout = child_window_count(hwnd);
                if stop.load(Ordering::Acquire) {
                    let mut current_owner = 0;
                    unsafe { GetWindowThreadProcessId(hwnd, &mut current_owner) };
                    if current_owner != owner {
                        // The coordinator already unwound and its owned HWND has been destroyed.
                        break;
                    }
                }
                panic!("HKSLGT_HISTORY_NATIVE_HUNG: main HWND failed WM_NULL in {BUDGET:?}; sent at {:?} during {section}, timeout during {}, childrenBefore={children}, childrenAtTimeout={at_timeout}, maxChildren={}",
                    start.duration_since(entered), *stage.lock().unwrap(), max_children.max(at_timeout));
            }
            let elapsed = start.elapsed();
            worst = worst.max(elapsed);
            stage_worst = stage_worst.max(elapsed);
            stage_checks += 1;
            checks += 1;
            thread::sleep(Duration::from_millis(50));
        }
        if let Some(previous) = last_stage {
            println!("HKSLGT_HISTORY_NATIVE_ACK: stage={previous}, pings={stage_checks}, worst={stage_worst:?}");
        }
        println!("HKSLGT_HISTORY_CHILD_HWND: final maxChildren={max_children}");
        (checks, worst)
    })
}

// Register the WebView evaluation off the test's async coordinator. If Windows stops
// dispatching, eval_with_callback itself can block before registering its callback.
async fn page_bool(window: &WebviewWindow, script: &'static str, label: &'static str) -> bool {
    page_bool_within(window, script, label, BUDGET).await
}

async fn page_bool_within(
    window: &WebviewWindow,
    script: &'static str,
    label: &'static str,
    budget: Duration,
) -> bool {
    tokio::time::timeout(budget, page_result(window, script))
        .await
        .unwrap_or_else(|_| panic!("HKSLGT_HISTORY_RENDERER_HUNG: {label} exceeded {budget:?}"))
}

async fn page_result<T: serde::de::DeserializeOwned>(
    window: &WebviewWindow,
    script: &'static str,
) -> T {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let window = window.clone();
    thread::spawn(move || {
        let tx = Mutex::new(Some(tx));
        let _ = window.eval_with_callback(script, move |raw| {
            if let Some(tx) = tx.lock().unwrap().take() {
                let _ = tx.send(String::from(raw));
            }
        });
    });
    let raw = rx.await.expect("main WebView2 evaluation callback closed");
    serde_json::from_str(&raw).expect("main WebView2 returned the expected JSON type")
}

const COLLAPSED: &str =
    "document.querySelector('.app-sidebar')?.classList.contains('collapsed') === true";
const PRESS_SIDEBAR: &str = "(() => { const el = document.querySelector('.app-sidebar__expand-line'); if (!el) return false; el.dispatchEvent(new MouseEvent('mousedown',{bubbles:true,button:0})); return true })()";

const ARM_ROOM_CLICK: &str = r#"(() => {
    const sidebar = document.querySelector('.app-sidebar');
    const control = sidebar?.querySelector('.app-sidebar__expand-line');
    if (!control || !location.hash.includes('player-tabs') || window.__hkslgtRoomClick) return false;
    const before = sidebar.classList.contains('collapsed');
    const armed = performance.now();
    const pulse = window.__hkslgtRoomClick = {
        before, armed, due: armed + 100, armedWall: Date.now(),
        fired: null, firedWall: null, changed: null, changedWall: null, after: null, error: null
    };
    const observer = new MutationObserver(() => {
        const collapsed = sidebar.classList.contains('collapsed');
        if (pulse.fired !== null && collapsed !== before) {
            pulse.changed = performance.now();
            pulse.changedWall = Date.now();
            pulse.after = collapsed;
            observer.disconnect();
        }
    });
    observer.observe(sidebar, {attributes: true, attributeFilter: ['class']});
    setTimeout(() => {
        if (!control.isConnected || !location.hash.includes('player-tabs') || sidebar.classList.contains('collapsed') !== before) {
            pulse.error = 'the real history sidebar changed before its scheduled input';
            observer.disconnect();
            return;
        }
        pulse.fired = performance.now();
        pulse.firedWall = Date.now();
        control.dispatchEvent(new MouseEvent('mousedown', {bubbles:true, button:0}));
    }, 100);
    return true;
})()"#;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoomClick {
    before: bool,
    armed: f64,
    due: f64,
    armed_wall: f64,
    fired: Option<f64>,
    fired_wall: Option<f64>,
    changed: Option<f64>,
    changed_wall: Option<f64>,
    sampled: f64,
    after: Option<bool>,
    error: Option<String>,
}

fn wall_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn measured_room_click(
    main: &WebviewWindow,
    before: bool,
    open_wall: u64,
    cold: bool,
) -> u64 {
    let read_started = Instant::now();
    let pulse = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let pulse: RoomClick = page_result(main, "({...window.__hkslgtRoomClick, sampled:performance.now()})").await;
            if let Some(error) = &pulse.error {
                panic!("HKSLGT_HISTORY_ROOM_INPUT: {error}");
            }
            if pulse.changed.is_some() {
                break pulse;
            }
            assert!(pulse.sampled - pulse.due <= BUDGET.as_secs_f64() * 1000.0,
                "HKSLGT_HISTORY_ROOM_INPUT_HUNG: browser readback succeeded; timer or Vue change still pending after {:.2}ms (fired={:?})",
                pulse.sampled - pulse.due, pulse.fired);
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }).await.expect("HKSLGT_HISTORY_ROOM_INPUT_COLLECTION: callback did not return a finished browser measurement within 8s");
    let fired = pulse
        .fired
        .expect("browser timer dispatched the real input");
    // WebView2 serializes the integer returned by Date.now() as a JSON double.
    let fired_wall = pulse.fired_wall.unwrap() as u64;
    let changed = pulse.changed.unwrap();
    assert_eq!(
        pulse.before, before,
        "sidebar changed while arming the timer"
    );
    assert_eq!(
        pulse.after,
        Some(!before),
        "Vue must toggle the original sidebar class"
    );
    assert!(
        pulse.armed_wall <= open_wall as f64 && open_wall <= fired_wall,
        "test timer outside the real open: armed={} open={open_wall} fired={fired_wall}",
        pulse.armed_wall
    );
    assert!(
        pulse.due >= pulse.armed && changed >= fired,
        "invalid browser monotonic clock samples"
    );
    let late = (fired - pulse.due).max(0.0);
    let feedback = changed - fired;
    let elapsed = late + feedback;
    println!("HKSLGT_HISTORY_ROOM_INPUT: newWindow={cold} plannedMs={:.2} firedMs={fired:.2} changedMs={changed:.2} lateMs={late:.2} vueFeedbackMs={feedback:.2} actualMs={elapsed:.2} firedAfterOpenMs={} collectionWait={:?} resultAgeAtCollectionMs={}",
        pulse.due, fired_wall - open_wall, read_started.elapsed(), wall_millis().saturating_sub(pulse.changed_wall.unwrap() as u64));
    assert!(elapsed <= BUDGET.as_secs_f64() * 1000.0,
        "HKSLGT_HISTORY_ROOM_INPUT_HUNG: the scheduled sidebar input and Vue class change took {elapsed:.2}ms");
    fired_wall
}

// PlayerTabHeader drives NButton.loading from all eight profile requests. Naive UI 2.44.1
// ignores clicks whilst loading but sets the DOM disabled property only for its disabled prop.
const CLICK_READY_REFRESH: &str = r#"(() => {
    const buttons = document.querySelectorAll('.mb-6.h-28 > .ml-8 > button.n-button:last-child');
    const refresh = Array.from(buttons).find(button => button.getClientRects().length > 0);
    if (!refresh || refresh.disabled || refresh.classList.contains('n-button--loading')) return false;
    refresh.click();
    return true;
})()"#;
const LOADING: &str =
    "location.hash.includes('player-tabs') && document.querySelector('.n-spin') != null";
const CARDS: &str = r#"document.querySelectorAll('[class*="contain-intrinsic-size:116px"]').length === 20 && [...document.querySelectorAll('[class*="contain-intrinsic-size:116px"]')].some(card => /12\s*\/\s*3\s*\/\s*19/.test(card.innerText))"#;
const RECOVERED: &str = r#"(() => { const cards = document.querySelectorAll('[class*="contain-intrinsic-size:116px"]'); return cards.length === 20 && /13\s*\/\s*3\s*\/\s*19/.test(cards[0].innerText); })()"#;

const DECODED_ART: &str = r#"(() => {
    const images = [...document.querySelectorAll('.mb-6.h-28 img, [class*="contain-intrinsic-size:116px"] img, img[class~="size-4.5"]')];
    return images.flatMap(img => {
        if (!img.complete || img.naturalWidth < 2 || img.naturalHeight < 2 || !img.getClientRects().length) return [];
        const url = new URL(img.currentSrc);
        const prefix = '/lol-game-data/assets/';
        return url.host === 'league-art.localhost' && url.pathname.startsWith(prefix)
            ? [decodeURIComponent(url.pathname.slice(prefix.length)).toLowerCase()] : [];
    });
})()"#;

const CATALOGS_PENDING: &str = r#"(() => {
    const cards = [...document.querySelectorAll('[class*="contain-intrinsic-size:116px"]')];
    const icons = cards.flatMap(card => [...card.querySelectorAll('img')]);
    return cards.length === 20 &&
        icons.some(img => img.naturalWidth > 1 && img.currentSrc.endsWith('/PressTheAttack.png')) &&
        icons.some(img => img.naturalWidth > 1 && img.currentSrc.includes('/v1/champion-icons/')) &&
        !icons.some(img => /\/Icons2D\/|MagicMissile_small\.png/i.test(img.currentSrc));
})()"#;

const ORIGINAL_MATCH_WITH_ART: &str = r#"(() => {
    const card = document.querySelector('[class*="contain-intrinsic-size:116px"]');
    return /12\s*\/\s*3\s*\/\s*19/.test(card?.innerText ?? '') &&
        [...card.querySelectorAll('img')].some(img =>
            img.currentSrc.endsWith('/MagicMissile_small.png') && img.naturalWidth > 1);
})()"#;

const TEAMMATE_AND_EMPTY_SLOT: &str = r#"(() => {
    const teammate = [...document.querySelectorAll('img[class~="size-4.5"]')].find(img =>
        img.currentSrc.endsWith('/v1/profile-icons/0.jpg') && img.naturalWidth > 1 &&
        img.closest('.flex.items-center')?.innerText.includes('Fixture player 2'));
    const newest = document.querySelector('[class*="contain-intrinsic-size:116px"]');
    return !!teammate && /13\s*\/\s*3\s*\/\s*19/.test(newest?.innerText ?? '') &&
        newest.querySelectorAll('img.item, img.trinket').length === 6 &&
        newest.querySelectorAll('.item.empty').length === 1;
})()"#;

const HISTORY_LAYOUT: &str = r#"(() => {
    const header = document.querySelector('.mb-6.h-28');
    return [
        window.innerWidth,
        document.querySelector('.app-frame__right-content')?.clientWidth ?? 0,
        header?.parentElement?.classList.contains('w-266') ?? false
    ];
})()"#;

async fn show_recent_teammates(main: &WebviewWindow) {
    let (initial_viewport, initial_content, initially_wide): (f64, f64, bool) =
        page_result(main, HISTORY_LAYOUT).await;
    if !initially_wide {
        // Stock PlayerTab mounts recent teammates only in the wide layout. Change the
        // native WebView zoom so a small runner desktop gets that layout too.
        let zoom = (initial_viewport / 1360.0).clamp(0.25, 1.0);
        main.set_zoom(zoom)
            .expect("widen the real history viewport on a small desktop");
    }
    check_renderer(
        main,
        r#"document.querySelector('.app-frame__right-content').clientWidth >= 1064 &&
            [...document.querySelectorAll('.w-266 .w-75 .flex.items-center')].some(
                row => row.innerText.includes('Fixture player 2'))"#,
        "stock wide history layout with the fixture's recent teammate row",
        || {},
    )
    .await;
    let (viewport, content, wide): (f64, f64, bool) = page_result(main, HISTORY_LAYOUT).await;
    println!(
        "HKSLGT_HISTORY_RECENT_LAYOUT: viewport {initial_viewport} -> {viewport}, content {initial_content} -> {content}, wide {initially_wide} -> {wide}"
    );
}

async fn check_history_art(main: &WebviewWindow, history: &HistoryFixture) {
    let started = Instant::now();
    loop {
        let decoded: BTreeSet<String> =
            tokio::time::timeout(BUDGET, page_result(main, DECODED_ART))
                .await
                .expect("read genuinely decoded history images from WebView2");
        let fetched = history.requests.lock().unwrap().history_art.clone();
        let missing: Vec<_> = expected_art()
            .filter(|asset| {
                !decoded.contains(&asset.to_ascii_lowercase()) || !fetched.contains(asset)
            })
            .collect();
        if missing.is_empty() {
            println!("HKSLGT_HISTORY_ART_PASS: {} profile, item, spell, rune and KIWI augment icons fetched over TLS and decoded in real WebView2", fetched.len());
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "HKSLGT_HISTORY_ART_MISSING: {missing:?}; TLS assets={fetched:?}; decoded={decoded:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn bitmap_retry_before_refresh(
    main: &WebviewWindow,
    history: &HistoryFixture,
) -> (Vec<u16>, bool) {
    let decoded = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let decoded: BTreeSet<String> = page_result(main, DECODED_ART).await;
            if decoded.contains(&BITMAP_PROBE.to_ascii_lowercase()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .is_ok();
    let responses = history
        .requests
        .lock()
        .unwrap()
        .bitmap_probe_responses
        .clone();
    println!("HKSLGT_HISTORY_BITMAP_BEFORE_REFRESH: {BITMAP_PROBE} responses={responses:?}; decoded={decoded}");
    (responses, decoded)
}

async fn check_artwork_queue(main: &WebviewWindow, history: &HistoryFixture) {
    history.requests.lock().unwrap().artwork_queue = Some(ArtworkQueue::default());
    let paths = QUEUE_BLOCKERS.map(|path| format!("http://league-art.localhost{ASSETS}{path}"));
    let original_document: f64 = page_result(main, "performance.timeOrigin").await;
    // Exercise the real WebView URI handler using fresh image URLs. Existing match
    // cards may already have decoded and cached their own artwork.
    main.eval(format!(
        "window.__hkslgtQueue = {{ blockers: {}.map(src => {{ const img = new Image(); img.src = src; return img }}) }}",
        json!(paths)
    ))
    .expect("request the four real image subresources");
    let first_read = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            {
                let state = history.requests.lock().unwrap();
                let queue = state.artwork_queue.as_ref().unwrap();
                if queue.blockers.len() == QUEUE_BLOCKERS.len() {
                    break queue.first_read.unwrap();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("four distinct HTTPS image requests occupy the four native permits");
    let queued_url = format!("http://league-art.localhost{ASSETS}{QUEUED_IMAGE}");
    main.eval(format!(
        "window.__hkslgtQueue.last = new Image(); window.__hkslgtQueue.last.src = {}",
        json!(queued_url)
    ))
    .expect("queue the fifth image in the same WebView");
    assert!(
        page_bool(
            main,
            "Boolean(window.__hkslgtQueue?.last?.src)",
            "observe the queued image in the same browser document"
        )
        .await
    );
    let queued_at = Instant::now();
    tokio::time::sleep(QUEUE_HOLD).await;
    let (released_at, reads_before_release) = {
        let mut state = history.requests.lock().unwrap();
        let queue = state.artwork_queue.as_mut().unwrap();
        queue.released = true;
        (Instant::now(), queue.queued_reads)
    };
    history.release.notify_all();
    let held_while_waiting = released_at.duration_since(queued_at);
    let oldest_request = released_at.duration_since(first_read);
    assert!(
        held_while_waiting >= QUEUE_HOLD && oldest_request < Duration::from_millis(3900),
        "four valid requests held {held_while_waiting:?} after the fifth was issued, oldest request {oldest_request:?} (LCU deadline 4s)"
    );
    assert_eq!(
        reads_before_release, 0,
        "the fifth request must wait for a permit"
    );
    let (blocker_widths, queued_width, source): (Vec<u32>, u32, String) =
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let result: (bool, Vec<u32>, u32, String) = page_result(
                    main,
                    r#"(() => {
                        const {blockers, last} = window.__hkslgtQueue;
                        return [last.complete && blockers.every(i => i.complete),
                            blockers.map(i => i.naturalWidth), last.naturalWidth, last.currentSrc];
                    })()"#,
                )
                .await;
                if result.0 {
                    break (result.1, result.2, result.3);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("all five WebView artwork requests settled after releasing the first four");
    let queue = history
        .requests
        .lock()
        .unwrap()
        .artwork_queue
        .take()
        .unwrap();
    println!(
        "HKSLGT_HISTORY_ART_QUEUE: occupied={} held={held_while_waiting:?} oldest={oldest_request:?} fifthTlsReads={} blockerWidths={blocker_widths:?} fifthWidth={queued_width}",
        queue.blockers.len(), queue.queued_reads
    );
    assert!(
        !queue.timed_out
            && blocker_widths.len() == 4
            && blocker_widths.iter().all(|&width| width > 1)
    );
    assert_eq!(
        page_result::<f64>(main, "performance.timeOrigin").await,
        original_document
    );
    assert!(
        queue.queued_reads == 1 && queued_width > 1 && source == queued_url,
        "the same fifth URI needs one authenticated HTTPS request and decoded pixels after waiting"
    );
}

async fn click_ready_refresh(main: &WebviewWindow, label: &'static str) {
    let started = Instant::now();
    loop {
        if page_bool(main, CLICK_READY_REFRESH, label).await {
            println!(
                "HKSLGT_HISTORY_REFRESH_CLICK: {label} available after {:?}",
                started.elapsed()
            );
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{label}: the stock refresh stayed unavailable"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn click_sidebar(
    main: &WebviewWindow,
    expected: bool,
    budget: Duration,
    label: &'static str,
) -> Duration {
    let started = Instant::now();
    assert!(
        page_bool_within(main, PRESS_SIDEBAR, label, budget).await,
        "stock sidebar control missing"
    );
    let dispatched = started.elapsed();
    assert_eq!(
        page_bool_within(
            main,
            COLLAPSED,
            label,
            budget.saturating_sub(started.elapsed())
        )
        .await,
        expected,
        "sidebar did not handle its input"
    );
    let elapsed = started.elapsed();
    if elapsed > BUDGET {
        println!("HKSLGT_HISTORY_SLOW_INPUT: {label}, click dispatch={dispatched:?}, following Vue readback={:?}", elapsed - dispatched);
    }
    elapsed
}

async fn input_round_trip(main: &WebviewWindow, label: &'static str) {
    let started = Instant::now();
    println!("HKSLGT_HISTORY_UI_INPUT: {label}");
    let previous = page_bool(main, COLLAPSED, "read sidebar").await;
    for expected in [!previous, previous] {
        click_sidebar(main, expected, BUDGET, label).await;
    }
    println!("HKSLGT_HISTORY_UI_ACK: {label} in {:?}", started.elapsed());
}

#[derive(Default)]
struct CdnResponses {
    by_url: BTreeMap<&'static str, u16>,
    had_credentials_or_referrer: bool,
    error: Option<String>,
}

async fn intercept_cdn_images(main: &WebviewWindow) -> Arc<Mutex<CdnResponses>> {
    use base64::Engine;
    use webview2_com::{
        CoTaskMemPWSTR,
        Microsoft::Web::WebView2::Win32::{
            ICoreWebView2_22, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE,
            COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
        },
        WebResourceRequestedEventHandler,
    };
    use windows::{
        core::{w, Error, Interface, PCWSTR, PWSTR},
        Win32::{Foundation::E_OUTOFMEMORY, UI::Shell::SHCreateMemStream},
    };

    let decoded_png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAMAAAACCAIAAAASFvFNAAAAFUlEQVR4nGO4pMEvt8BNK+8EA5wFAEKIBtsqErd8AAAAAElFTkSuQmCC")
        .expect("valid, generated 3x2 artwork fixture");
    let responses = Arc::new(Mutex::new(CdnResponses::default()));
    let capture = Arc::clone(&responses);
    let registration = Arc::new(Mutex::new("dispatch to the WebView2 UI thread"));
    let step = Arc::clone(&registration);
    let (send, receive) = tokio::sync::oneshot::channel();
    main.with_webview(move |webview| {
        // WebView2 requires that the filters and handler be installed on its own UI thread.
        let result = (|| -> windows::core::Result<()> {
            *step.lock().unwrap() = "get CoreWebView2";
            let controller = webview.controller();
            let environment = webview.environment();
            let core = unsafe { controller.CoreWebView2()? };
            let modern_filters = core.cast::<ICoreWebView2_22>().ok();
            for (url, _) in CDN_IMAGES {
                *step.lock().unwrap() = url;
                let wide: Vec<_> = url.encode_utf16().chain([0]).collect();
                unsafe {
                    if let Some(modern) = &modern_filters {
                        modern.AddWebResourceRequestedFilterWithRequestSourceKinds(
                            PCWSTR(wide.as_ptr()),
                            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE,
                            COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
                        )?;
                    } else {
                        core.AddWebResourceRequestedFilter(
                            PCWSTR(wide.as_ptr()),
                            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE,
                        )?;
                    }
                }
            }
            *step.lock().unwrap() = "register WebResourceRequested handler";
            let handler = WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                // These generated COM interfaces are used only here on the WebView2 callback thread.
                let result = (|| unsafe {
                    let request = args.Request()?;
                    let mut uri = PWSTR::null();
                    request.Uri(&mut uri)?;
                    let uri = CoTaskMemPWSTR::from(uri).to_string();
                    let Some((url, status)) = CDN_IMAGES.into_iter().find(|&(url, _)| uri == url)
                    else {
                        return Ok(());
                    };
                    let mut method = PWSTR::null();
                    request.Method(&mut method)?;
                    if CoTaskMemPWSTR::from(method).to_string() != "GET" {
                        return Ok(());
                    }
                    let headers = request.Headers()?;
                    let mut authorization = Default::default();
                    let mut referer = Default::default();
                    headers.Contains(w!("Authorization"), &mut authorization)?;
                    headers.Contains(w!("Referer"), &mut referer)?;
                    let png = (status == 200).then_some(decoded_png.as_slice());
                    let body = SHCreateMemStream(png).ok_or_else(|| Error::from(E_OUTOFMEMORY))?;
                    let reason = if status == 200 {
                        w!("OK")
                    } else {
                        w!("Not Found")
                    };
                    let response = environment.CreateWebResourceResponse(
                        &body,
                        i32::from(status),
                        reason,
                        w!("Content-Type: image/png\r\nCache-Control: no-store"),
                    )?;
                    args.SetResponse(&response)?;
                    let mut responses = capture.lock().unwrap();
                    responses.by_url.insert(url, status);
                    responses.had_credentials_or_referrer |=
                        authorization.as_bool() || referer.as_bool();
                    Ok(())
                })();
                if let Err(error) = &result {
                    capture.lock().unwrap().error = Some(format!("{error}"));
                }
                result
            }));
            let mut token = 0;
            unsafe { core.add_WebResourceRequested(&handler, &mut token)? };
            Ok(())
        })();
        let _ = send.send(result.map_err(|error| format!("{}: {error}", *step.lock().unwrap())));
    })
    .expect("schedule the offline response filter on the real WebView2");
    match tokio::time::timeout(Duration::from_secs(8), receive).await {
        Ok(Ok(Ok(()))) => println!("HKSLGT_HISTORY_CDN_FILTER_READY"),
        Ok(Ok(Err(error))) => panic!("WebView2 CDN filter setup rejected: {error}"),
        Ok(Err(error)) => panic!(
            "WebView2 CDN filter callback dropped at {}: {error}",
            *registration.lock().unwrap()
        ),
        Err(_) => panic!(
            "WebView2 CDN filter setup did not finish at {}",
            *registration.lock().unwrap()
        ),
    }
    responses
}

async fn check_persistent_artwork(app: &AppHandle) {
    let profile = app.state::<DesktopProfile>();
    bootstrap::backend_pref("self-update-main", "autoCheckUpdates")
        .unwrap()
        .write(&profile.database.lock().unwrap(), &json!(false))
        .unwrap();
    let runtime = app.state::<ClientRuntime>();
    runtime.select(app, None).unwrap();
    let server = app.state::<MiniClient>();
    server.scene.lock().unwrap().phase = "None";
    server.history.begin_cdn();
    let client = Arc::new(LocalClient::fixture_at(server.port).expect("isolated HN1 HTTPS peer"));
    let main = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("main-window.html".into()))
        .data_directory(profile.directory.join("PersistentArtWebView2"))
        .inner_size(1000.0, 760.0)
        .decorations(false)
        .on_navigation(super::super::bundled_navigation)
        .build()
        .expect("open the genuine bundled main window");
    // Wry queues creation for this worker thread; a builder handle alone does
    // not prove that the UI thread has registered and navigated its WebView2.
    check_renderer(
        &main,
        "!!document.querySelector('.menu-item[data-key=player-tabs]')",
        "bundled main page mounted before installing WebView2 image filters",
        || {},
    )
    .await;
    // Set the normal wide layout before navigation, so the actual cards never
    // need to remount in the change between local artwork and public fallback.
    main.set_zoom(0.72)
        .expect("widen this isolated history viewport");
    check_renderer(
        &main,
        "document.querySelector('.app-frame__right-content')?.clientWidth >= 1064 && !!document.querySelector('.menu-item[data-key=player-tabs]')",
        "genuine main navigation and wide history viewport",
        || {},
    )
    .await;
    let intercepted = intercept_cdn_images(&main).await;
    let document_origin: f64 = page_result(&main, "performance.timeOrigin").await;
    main.eval(
        r#"(() => {
        const observeCards = new MutationObserver(() => {
            const cards = [...document.querySelectorAll('[class*="contain-intrinsic-size:116px"]')];
            if (cards.length === 2) {
                window.__hkslgtCdnCards = cards;
                observeCards.disconnect();
            }
        });
        observeCards.observe(document.querySelector('#app'), {subtree: true, childList: true});
    })()"#,
    )
    .unwrap();
    runtime.sample_fixture(app, &client).await;
    assert!(page_bool(&main, "(() => { document.querySelector('.menu-item[data-key=player-tabs]').click(); return true })()", "open real ARAM and KIWI cards").await);
    check_renderer(
        &main,
        "location.hash.includes('player-tabs') && window.__hkslgtCdnCards?.length === 2",
        "the two original ARAM and KIWI card elements",
        || {},
    )
    .await;

    let started = Instant::now();
    loop {
        if let Some(error) = &intercepted.lock().unwrap().error {
            panic!("WebView2 CDN image response failed: {error}");
        }
        let decoded: BTreeSet<String> = tokio::time::timeout(BUDGET, page_result(
            &main,
            r#"Array.from(document.querySelectorAll('.mb-6.h-28 img, [class*="contain-intrinsic-size:116px"] img'))
                .filter(img => img.naturalWidth === 3 && img.naturalHeight === 2 && img.getClientRects().length)
                .map(img => img.currentSrc)"#,
        )).await.expect("read the actual fallback image dimensions");
        let missing: Vec<_> = CDN_IMAGES
            .into_iter()
            .filter(|&(url, status)| status == 200 && !decoded.contains(url))
            .map(|(url, _)| url)
            .collect();
        if missing.is_empty() {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(8),
            "HKSLGT_HISTORY_CDN_MISSING: {missing:?}; intercepted={:?}; persistent local 404={:?}; decoded={decoded:?}",
            intercepted.lock().unwrap().by_url, server.history.requests.lock().unwrap().local_cdn_failures);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    check_renderer(
        &main,
        TEAMMATE_AND_EMPTY_SLOT,
        "decoded local teammate avatar and correctly empty zero equipment slot",
        || {},
    )
    .await;
    check_renderer(&main, r#"(() => {
        const header = document.querySelector('.mb-6.h-28');
        const cards = [...document.querySelectorAll('[class*="contain-intrinsic-size:116px"]')];
        return header.innerText.includes('History Fixture') &&
            header.querySelector('img[src^="https://ddragon.leagueoflegends.com/"][src$="/profileicon/29.png"]')?.naturalWidth === 3 &&
            cards.every((card, i) => card === window.__hkslgtCdnCards[i]) &&
            cards.some(card => [...card.querySelectorAll('img')].some(img => img.currentSrc.includes('http://league-art.localhost/lol-game-data/assets/v1/champion-icons/') && img.naturalWidth > 1)) &&
            cards[1].querySelector('img.augment.gold[src$="/rune/magicmissile_small.png"]')?.naturalWidth === 3 &&
            /13\s*\/\s*3\s*\/\s*19/.test(cards[0].innerText);
    })()"#, "the same player and two unchanged cards, with local champion and rendered KIWI fallback", || {}).await;
    assert_eq!(
        page_result::<f64>(&main, "performance.timeOrigin").await,
        document_origin
    );
    let (_, game_data) = runtime
        .fields("league-client-main", "gameData")
        .unwrap()
        .unwrap();
    assert!(game_data["items"]
        .get(CDN_MISSING_ITEM.to_string())
        .is_none());
    assert!(Arc::ptr_eq(&runtime.selected_client().unwrap(), &client));
    assert_eq!(
        server.history.requests.lock().unwrap().local_cdn_failures,
        BTreeSet::from(CDN_LOCAL_MISSING)
    );
    let hits = intercepted.lock().unwrap();
    assert_eq!(hits.by_url, BTreeMap::from(CDN_IMAGES));
    assert!(
        !hits.had_credentials_or_referrer,
        "public artwork requests carry no native credentials or referrer"
    );
    println!("HKSLGT_HISTORY_CDN_PASS: six permanent local 404s and one missing item catalog ID recovered from exact offline-intercepted CDN URLs; local controls and the original cards stayed intact");
    drop(hits);
    server.history.end();
    main.destroy()
        .expect("close the isolated CDN regression window");
}

pub(super) async fn check(app: &AppHandle) {
    if std::env::var_os(CDN_ONLY).is_some() {
        check_persistent_artwork(app).await;
        return;
    }
    let profile = app.state::<DesktopProfile>();
    let (aux_enabled, auto_update) = {
        let db = profile.database.lock().unwrap();
        let update = bootstrap::backend_pref("self-update-main", "autoCheckUpdates").unwrap();
        let auto_update = update.read(&db).unwrap();
        update.write(&db, &json!(false)).unwrap();
        let enabled = bootstrap::backend_pref(AUX_WINDOW, "enabled")
            .unwrap()
            .read(&db)
            .unwrap()
            .as_bool()
            .unwrap();
        (enabled, auto_update)
    };
    set_preference(app, AUX_WINDOW, "enabled", false);
    let runtime = app.state::<ClientRuntime>();
    // The bundled suite reuses its PID after the Mini scene. Begin this scene with a fresh catalog.
    runtime
        .select(app, None)
        .expect("disconnect the preceding test scene");
    let server = app.state::<MiniClient>();
    let previous_phase = {
        let mut scene = server.scene.lock().unwrap();
        std::mem::replace(&mut scene.phase, "None")
    };
    server.history.begin();
    let client =
        Arc::new(LocalClient::fixture_at(server.port).expect("owned HN1 fixture listener"));
    let main = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("main-window.html".into()))
        .data_directory(
            profile
                .webview_directory()
                .expect("shared fixture WebView2 directory"),
        )
        .inner_size(1000.0, 760.0)
        .decorations(false)
        .on_navigation(super::super::bundled_navigation)
        .build()
        .expect("create bundled main window for history acceptance");
    check_renderer(
        &main,
        "document.querySelector('.menu-item[data-key=toolkit]') != null",
        "bundled main navigation",
        || {},
    )
    .await;
    assert!(page_bool(&main, "(() => { document.querySelector('.menu-item[data-key=toolkit]').click(); return true })()", "open toolkit before history").await);
    check_renderer(
        &main,
        "location.hash.includes('toolkit')",
        "actual toolkit route before history",
        || {},
    )
    .await;
    runtime.sample_fixture(app, &client).await;
    reconcile_companions(app);
    super::assert_hidden(
        app,
        "aux",
        "aux must be hidden before opening player history",
    );
    assert_eq!(
        server.history.requests.lock().unwrap().pages,
        0,
        "history must start on actual navigation"
    );
    input_round_trip(&main, "toolkit before navigation").await;
    let started = Instant::now();
    let stop = StopWatchdog(Arc::new(AtomicBool::new(false)));
    let stage = Arc::new(Mutex::new("history navigation; aux disabled"));
    println!(
        "HKSLGT_HISTORY_CHILD_HWND: before navigation children={}",
        child_window_count(main.hwnd().unwrap().0)
    );
    let ping = watchdog(
        main.hwnd().unwrap().0 as usize,
        Arc::clone(&stop.0),
        Arc::clone(&stage),
        started,
    );
    println!(
        "HKSLGT_HISTORY_ENTER: responsive main in toolkit, clicking the bundled player-tabs menu"
    );
    assert!(page_bool(&main, "(() => { document.querySelector('.menu-item[data-key=player-tabs]').click(); return true })()", "enter player-tabs").await);
    tokio::time::timeout(Duration::from_secs(6), async {
        while server.history.requests.lock().unwrap().pages == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("no HN1 history request arrived after clicking the player tab");
    *stage.lock().unwrap() = "history read outstanding; aux disabled";
    println!(
        "HKSLGT_HISTORY_PENDING: native HN1 page read entered at {:?}; aux disabled",
        started.elapsed()
    );
    assert!(
        page_bool(&main, LOADING, "loading with aux still disabled").await,
        "real history route must still be loading"
    );
    input_round_trip(&main, "held first HN1 page with aux disabled").await;
    server.history.release_page();
    tokio::time::timeout(Duration::from_secs(6), async {
        while server.history.requests.lock().unwrap().details == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the production renderer did not request any HN1 game data");
    *stage.lock().unwrap() = "first full page game details and image requests; aux disabled";
    println!("HKSLGT_HISTORY_DETAILS_PENDING: at {:?}, game details for the 20-row page are pending with aux disabled", started.elapsed());
    input_round_trip(&main, "full game data pending with aux disabled").await;
    let before_open = page_bool(&main, COLLAPSED, "sidebar before the room opens").await;
    let cold = app.get_webview_window("aux").is_none();
    println!(
        "HKSLGT_HISTORY_AUX_REQUESTED: at {:?} after the standalone page heartbeat; newWindow={cold}, shared WebView2 directory",
        started.elapsed(),
    );
    *stage.lock().unwrap() = "room sampling while history read outstanding";
    set_preference(app, AUX_WINDOW, "enabled", true);
    server.scene.lock().unwrap().phase = "Lobby";
    let (opening, open_started) = tokio::sync::oneshot::channel();
    let popup = tauri::async_runtime::spawn({
        let app = app.clone();
        let main = main.clone();
        let client = Arc::clone(&client);
        let stage = Arc::clone(&stage);
        async move {
            app.state::<ClientRuntime>()
                .sample_fixture(&app, &client)
                .await;
            assert!(
                page_bool(
                    &main,
                    ARM_ROOM_CLICK,
                    "arm stock history sidebar before room open"
                )
                .await,
                "could not arm the one-shot input in the real main window"
            );
            *stage.lock().unwrap() = "room window opening while history read outstanding";
            println!(
                "HKSLGT_HISTORY_AUX_OPEN: at {:?}, real Lobby sample ready; requesting HWND",
                started.elapsed()
            );
            let _ = opening.send(wall_millis());
            reconcile_companions(&app);
            let open_returned = wall_millis();
            println!(
                "HKSLGT_HISTORY_AUX_OPEN_RETURN: native room open returned at {:?}",
                started.elapsed()
            );
            open_returned
        }
    });
    let open_wall = tokio::time::timeout(Duration::from_secs(3), open_started)
        .await
        .expect("the concurrent Lobby sample did not arm and request the real mini window")
        .unwrap();
    // Turn the pending HTTPS requests loose during the room open, independently of its
    // first input acknowledgement and within the real League transport's 4-second timeout.
    let history = Arc::clone(&server.history);
    let release = tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        history.requests.lock().unwrap().hold_details = false;
        history.release.notify_all();
    });
    let fired_wall = measured_room_click(&main, before_open, open_wall, cold).await;
    let following =
        click_sidebar(&main, before_open, BUDGET, "next input during room opening").await;
    println!("HKSLGT_HISTORY_AUX_INPUT: newWindow={cold}, nextAck={following:?}");
    release.await.unwrap();
    *stage.lock().unwrap() = "first full page rendering alongside room";
    check_renderer(
        &main,
        CARDS,
        "20 match cards with the fixture player's KDA on first entry",
        || {},
    )
    .await;
    println!(
        "HKSLGT_HISTORY_FIRST_PAGE: 20 match cards rendered at {:?}",
        started.elapsed()
    );
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let exhausted = {
                let state = server.history.requests.lock().unwrap();
                LATE_CATALOGS.iter().all(|key| {
                    state.catalog_reads.get(key).copied().unwrap_or(0) >= INITIAL_CATALOG_ATTEMPTS
                })
            };
            if exhausted {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("three real HTTPS attempts reached each unavailable catalog");
    check_renderer(
        &main,
        CATALOGS_PENDING,
        "visible champion and rune art while item, spell and augment catalogs remain unavailable",
        || {},
    )
    .await;
    let original_document: (f64, String) =
        page_result(&main, "[performance.timeOrigin, location.hash]").await;
    println!("HKSLGT_HISTORY_CATALOG_PENDING: all startup attempts exhausted; champion and rune art decoded, item, spell and augment art absent");
    tokio::time::timeout(Duration::from_secs(12), async {
        while server.history.requests.lock().unwrap().timelines < PAGE_SIZE as usize {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("full HN1 timelines not requested by the bundled history page");
    let returned_wall = tokio::time::timeout(Duration::from_secs(12), popup)
        .await
        .expect("aux opening remained pending")
        .unwrap();
    let aux = super::visible_window(app, "aux").expect("real concurrently opened mini window");
    check_renderer(
        &aux,
        "document.querySelector('.aux-activity[data-phase=Lobby]') != null",
        "visible aux lounge while main is on history",
        || {},
    )
    .await;
    if cold {
        // WebView construction returns before the new document finishes loading. Read the
        // WebView's own recorded load time instead of timing a later automation callback.
        let loaded_wall: f64 = tokio::time::timeout(BUDGET, page_result(&aux,
            "performance.timeOrigin + performance.getEntriesByType('navigation')[0].loadEventEnd"))
            .await.expect("could not read the already-mounted cold room's navigation timing");
        println!("HKSLGT_HISTORY_COLD_OVERLAP: nativeReturnAfterOpenMs={} newDocumentLoadedAfterOpenMs={:.1} inputAfterOpenMs={}",
            returned_wall - open_wall, loaded_wall - open_wall as f64, fired_wall - open_wall);
    }
    input_round_trip(&main, "full history rendered alongside visible room").await;

    // The second ordinary match has a unique rune style, so another card cannot
    // hide a lost image request. Observe recovery before zoom or any UI refresh.
    let bitmap = bitmap_retry_before_refresh(&main, &server.history).await;
    *stage.lock().unwrap() = "artwork queued behind four native reads";
    check_artwork_queue(&main, &server.history).await;
    *stage.lock().unwrap() = "catalog recovery after stock refresh";
    server.history.release_catalogs();
    click_ready_refresh(&main, "refresh the same page after its catalogs recover").await;
    tokio::time::timeout(Duration::from_secs(6), async {
        while server.history.requests.lock().unwrap().pages < 2 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the stock refresh re-requested the same HN1 page");
    show_recent_teammates(&main).await;
    check_history_art(&main, &server.history).await;
    check_renderer(
        &main,
        ORIGINAL_MATCH_WITH_ART,
        "the original KIWI match now has its decoded augment",
        || {},
    )
    .await;
    assert_eq!(
        page_result::<(f64, String)>(&main, "[performance.timeOrigin, location.hash]").await,
        original_document
    );
    assert!(Arc::ptr_eq(
        &runtime.selected_client().expect("same connected fixture"),
        &client
    ));
    let (_, owner) = runtime
        .fields("league-client-main", "summoner")
        .unwrap()
        .unwrap();
    assert_eq!(owner["me"]["puuid"], PLAYER);
    assert_eq!(
        bitmap,
        (vec![503, 200], true),
        "the first bitmap error must recover on the same URI before refresh, through the native bridge"
    );
    {
        let state = server.history.requests.lock().unwrap();
        assert_eq!(
            state.catalog_reads,
            BTreeMap::from([
                ("augments", 4),
                ("champions", 1),
                ("items", 4),
                ("perks", 1),
                ("perkstyles", 1),
                ("spells", 4)
            ])
        );
    }
    println!("HKSLGT_HISTORY_CATALOG_RECOVERED: the same match and connection recovered three exhausted catalogs after refresh; healthy catalogs stayed at one read");

    *stage.lock().unwrap() = "failed refresh and retry";
    server.history.requests.lock().unwrap().fail = true;
    click_ready_refresh(&main, "refresh from player header").await;
    tokio::time::timeout(Duration::from_secs(6), async {
        while server.history.requests.lock().unwrap().failed_pages == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the stock refresh did not receive the required HTTP 503");
    check_renderer(
        &main,
        "/Failed to Load Match History|加载战绩失败/i.test(document.body.innerText)",
        "explicit history error after the simulated HTTP 503",
        || {},
    )
    .await;
    let failed_page = server.history.requests.lock().unwrap().pages;
    server.history.requests.lock().unwrap().fail = false;
    click_ready_refresh(&main, "retry from player header").await;
    tokio::time::timeout(Duration::from_secs(6), async {
        while server.history.requests.lock().unwrap().pages <= failed_page {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("retry did not reach the HN1 history service");
    tokio::time::timeout(Duration::from_secs(8), async {
        while server.history.requests.lock().unwrap().new_game_reads == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the real renderer did not fetch the new HN1 game ID after recovery");
    check_renderer(
        &main,
        RECOVERED,
        "the new 13/3/19 game visible at the top of the recovered 20-match page",
        || {},
    )
    .await;
    input_round_trip(&main, "history after HTTP 503 retry").await;
    check_renderer(
        &main,
        TEAMMATE_AND_EMPTY_SLOT,
        "decoded recently played teammate avatar and one intentionally empty item slot",
        || {},
    )
    .await;
    drop(stop);
    let (pings, worst) = ping
        .join()
        .expect("independent Windows main-thread heartbeat");
    let (pages, failed, ranked, profiles, details, timelines, images, peak, timeout) = {
        let state = server.history.requests.lock().unwrap();
        (
            state.pages,
            state.failed_pages,
            state.ranked,
            state.profiles,
            state.details,
            state.timelines,
            state.champion_images,
            state.peak,
            state.timed_out,
        )
    };
    assert!(pings >= 3 && pages >= 3 && failed > 0 && details >= PAGE_SIZE as usize && ranked > 0 && profiles > 0 && !timeout,
        "missing live request/heartbeat coverage: pings={pings}, pages={pages}, ranked={ranked}, profiles={profiles}, gateTimedOut={timeout}");
    println!("HKSLGT_HISTORY_RESPONSIVE: nativePings={pings} worstNative={worst:?} profileReads={profiles} rankReads={ranked} historyReads={pages} detailReads={details} timelineReads={timelines} championImages={images} peakGameReads={peak}; stock sidebar handled input during loading, room opened, HTTP 503 was visible and retry rendered a new game among 20 matches");
    server.scene.lock().unwrap().phase = "None";
    runtime.sample_fixture(app, &client).await;
    reconcile_companions(app);
    set_preference(app, AUX_WINDOW, "enabled", aux_enabled);
    server.scene.lock().unwrap().phase = previous_phase;
    server.history.end();
    main.destroy()
        .expect("destroy only the owned history main window");
    bootstrap::backend_pref("self-update-main", "autoCheckUpdates")
        .unwrap()
        .write(&profile.database.lock().unwrap(), &auto_update)
        .unwrap();
}
