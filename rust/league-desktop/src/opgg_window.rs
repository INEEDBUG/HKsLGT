//! Local OP.GG companion for a verified draft. The guide opens in the user's browser.

use crate::{
    aram_automation::SelectionScope,
    client_discovery::ClientInstance,
    client_runtime::ClientRuntime,
    desktop::{close_opgg_companion, show_opgg_companion},
    lcu::GameflowSession,
};
use serde_json::{json, Value};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

pub(crate) const NAMESPACE: &str = "opgg-guide-main";
const LABEL: &str = "opgg";

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpggMode {
    Aram,
    Kiwi,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpggTarget {
    pub(crate) scope: SelectionScope,
    pub(crate) champion_id: Option<u32>,
    mode: OpggMode,
}

impl OpggTarget {
    pub(crate) fn from_sample(
        instance: ClientInstance,
        flow: &GameflowSession,
        select: &Value,
    ) -> Option<Self> {
        let scope = SelectionScope::from_sample(instance, flow, select)?;
        let mode = if flow.is_hextech_aram() {
            OpggMode::Kiwi
        } else {
            OpggMode::Aram
        };
        let cell = select["localPlayerCellId"].as_i64()?;
        let player = select["myTeam"]
            .as_array()?
            .iter()
            .find(|entry| entry["cellId"].as_i64() == Some(cell))?;
        let champion_id = player["championId"]
            .as_u64()
            .and_then(|id| u32::try_from(id).ok())
            .filter(|&id| id > 0);
        Some(Self {
            scope,
            champion_id,
            mode,
        })
    }

    pub(crate) fn view(self) -> Value {
        json!({
            "gameMode": match self.mode { OpggMode::Aram => "ARAM", OpggMode::Kiwi => "KIWI" },
            "championId": self.champion_id,
        })
    }

    pub(crate) fn guide_url(self, alias: Option<&str>) -> String {
        let mode_page = match self.mode {
            OpggMode::Aram => "https://op.gg/lol/modes/aram",
            OpggMode::Kiwi => "https://op.gg/lol/modes/aram-mayhem",
        };
        // Aliases come from the verified local catalog. Accept one URL path
        // segment and use the official mode page if the name is unavailable.
        let champion = alias.filter(|alias| {
            self.champion_id.is_some()
                && (1..=32).contains(&alias.len())
                && alias.bytes().all(|byte| byte.is_ascii_alphanumeric())
                && alias.as_bytes()[0].is_ascii_alphabetic()
        });
        match champion {
            Some(alias) => format!("{mode_page}/{}/build", alias.to_ascii_lowercase()),
            None => mode_page.to_owned(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct OpggPreferences {
    pub(crate) enabled: bool,
    pub(crate) auto_show: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum OpggDecision {
    Close,
    Show,
    Ignore,
}

const OPEN_RETRY_DELAY: Duration = Duration::from_secs(2);

struct OpggState {
    preferences: OpggPreferences,
    announced: Option<SelectionScope>,
    retry_after: Option<(SelectionScope, Instant)>,
}

impl OpggState {
    fn new(preferences: OpggPreferences) -> Self {
        Self {
            preferences,
            announced: None,
            retry_after: None,
        }
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.preferences.enabled = enabled;
        if !enabled {
            self.announced = None;
            self.retry_after = None;
        }
    }

    fn decision(&mut self, target: Option<OpggTarget>) -> OpggDecision {
        self.decision_at(target, Instant::now())
    }

    fn decision_at(&mut self, target: Option<OpggTarget>, now: Instant) -> OpggDecision {
        let Some(target) = target.filter(|_| self.preferences.enabled) else {
            return OpggDecision::Close;
        };
        if self.preferences.auto_show && self.announced != Some(target.scope) {
            if self
                .retry_after
                .is_some_and(|(scope, retry_at)| scope == target.scope && now < retry_at)
            {
                return OpggDecision::Ignore;
            }
            self.retry_after = Some((target.scope, now + OPEN_RETRY_DELAY));
            OpggDecision::Show
        } else {
            OpggDecision::Ignore
        }
    }

    fn mark_shown(&mut self, scope: SelectionScope) {
        self.announced = Some(scope);
        self.retry_after = None;
    }

    fn record_open_error(&mut self, scope: SelectionScope, visible: bool, now: Instant) {
        if visible {
            self.mark_shown(scope);
        } else {
            if self.announced == Some(scope) {
                self.announced = None;
            }
            self.retry_after = Some((scope, now + OPEN_RETRY_DELAY));
        }
    }
}

pub(crate) struct OpggWindow(Mutex<OpggState>);

impl OpggWindow {
    pub(crate) fn new(preferences: OpggPreferences) -> Self {
        Self(Mutex::new(OpggState::new(preferences)))
    }

    pub(crate) fn enabled(&self) -> bool {
        self.0.lock().is_ok_and(|state| state.preferences.enabled)
    }

    pub(crate) fn update_setting(&self, key: &str, value: bool) -> Result<(), &'static str> {
        let mut state = self.0.lock().map_err(|_| "OP.GG 设置暂不可用。")?;
        match key {
            "enabled" => state.set_enabled(value),
            "autoShow" => state.preferences.auto_show = value,
            _ => return Err("未知的 OP.GG 设置。"),
        }
        Ok(())
    }

    pub(crate) fn target(&self, app: &AppHandle) -> Option<OpggTarget> {
        if self.enabled() {
            app.state::<ClientRuntime>().opgg_target()
        } else {
            None
        }
    }

    pub(crate) fn mark_shown(&self, scope: SelectionScope) {
        if let Ok(mut state) = self.0.lock() {
            state.mark_shown(scope);
        }
    }

    pub(crate) fn reconcile(&self, app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
        let target = app.state::<ClientRuntime>().opgg_target();
        let decision = self
            .0
            .lock()
            .map_err(|_| "OP.GG 设置暂不可用。")?
            .decision(target);
        match decision {
            OpggDecision::Close => close_opgg_companion(app)?,
            OpggDecision::Show => {
                let scope = target
                    .expect("Show decision requires an eligible target")
                    .scope;
                if app
                    .get_webview_window(LABEL)
                    .is_none_or(|window| !window.is_visible().unwrap_or(false))
                {
                    if let Err(error) = show_opgg_companion(app) {
                        let visible = app
                            .get_webview_window(LABEL)
                            .is_some_and(|window| window.is_visible().unwrap_or(false));
                        if let Ok(mut state) = self.0.lock() {
                            state.record_open_error(scope, visible, Instant::now());
                        }
                        return Err(error);
                    }
                }
                self.mark_shown(scope);
            }
            OpggDecision::Ignore => {}
        }
        Ok(())
    }

    pub(crate) fn open_guide(&self, app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
        let runtime = app.state::<ClientRuntime>();
        let (target, url) = runtime
            .opgg_guide()
            .ok_or("当前没有可用的大乱斗选人会话。")?;
        if self.target(app) != Some(target) {
            return Err("大乱斗选人已结束或 OP.GG 已关闭。".into());
        }
        app.opener().open_url(url, None::<&str>)?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "opgg_window_tests.rs"]
mod tests;

#[cfg(test)]
mod retry_tests {
    use super::*;
    use serde_json::json;
    use std::time::Instant;

    #[test]
    fn automatic_decision_does_not_consume_scope_before_window_shows() {
        let instance = ClientInstance {
            pid: 14,
            created_at: 1000,
        };
        let flow: GameflowSession = serde_json::from_value(json!({
            "phase": "ChampSelect",
            "gameData": {
                "gameId": 45,
                "isCustomGame": false,
                "queue": {"gameMode": "ARAM", "type": "ARAM"}
            }
        }))
        .unwrap();
        let select = json!({
            "gameId": 45,
            "isCustomGame": false,
            "isSpectating": false,
            "localPlayerCellId": 1,
            "myTeam": [{"cellId": 1, "championId": 22}]
        });
        let target = OpggTarget::from_sample(instance, &flow, &select).unwrap();
        let mut state = OpggState::new(OpggPreferences {
            enabled: true,
            auto_show: true,
        });

        assert_eq!(state.decision(Some(target)), OpggDecision::Show);
        assert!(
            state.announced.is_none(),
            "the native window has not shown yet"
        );
        let scope = target.scope;
        let now = Instant::now();
        state.record_open_error(scope, false, now);
        assert_eq!(state.decision_at(Some(target), now), OpggDecision::Ignore);
        assert_eq!(
            state.decision_at(Some(target), now + OPEN_RETRY_DELAY),
            OpggDecision::Show
        );
        state.mark_shown(scope);
        state.record_open_error(scope, true, now + OPEN_RETRY_DELAY);
        assert!(state.announced.is_some(), "a visible window remains shown");
        assert_eq!(
            state.decision_at(Some(target), now + OPEN_RETRY_DELAY + OPEN_RETRY_DELAY),
            OpggDecision::Ignore
        );
    }
}
