//! Automatic display of the bundled auxiliary panel in a League room or selection.

use super::{publish_window_property, show_companion, AUX_WINDOW};
use crate::{
    client_discovery::{ClientInstance, LocalClient},
    client_runtime::ClientRuntime,
};
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Manager};

const RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuxStage {
    Room,
    Selection,
}

pub(crate) enum AuxObservation {
    Unconfirmed,
    Inactive,
    Active {
        stage: AuxStage,
        client: Arc<LocalClient>,
        game_id: Option<u64>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ActiveStage {
    stage: AuxStage,
    client: ClientInstance,
    game_id: Option<u64>,
}

struct AuxState {
    enabled: bool,
    auto_show: bool,
    active: Option<ActiveStage>,
    shown: bool,
    attempted: Option<Instant>,
    automated_client: Option<Arc<LocalClient>>,
}

impl AuxState {
    fn observe(&mut self, observation: &AuxObservation) {
        match observation {
            AuxObservation::Unconfirmed => {}
            AuxObservation::Inactive => {
                self.active = None;
                self.shown = false;
                self.attempted = None;
            }
            AuxObservation::Active {
                stage,
                client,
                game_id,
            } => {
                self.observe_active(*stage, client.instance, *game_id);
            }
        }
    }

    fn observe_active(&mut self, stage: AuxStage, client: ClientInstance, game_id: Option<u64>) {
        let game_id = game_id.filter(|&id| stage == AuxStage::Selection && id != 0);
        if let Some(previous) = self.active.as_mut() {
            if previous.stage == stage
                && previous.client == client
                && !matches!((previous.game_id, game_id), (Some(old), Some(new)) if old != new)
            {
                // A draft can acquire an ID after the panel has already been dismissed.
                previous.game_id = game_id.or(previous.game_id);
                return;
            }
        }
        self.active = Some(ActiveStage {
            stage,
            client,
            game_id,
        });
        self.shown = false;
        self.attempted = None;
    }

    fn begin_show(&mut self, now: Instant) -> bool {
        if self.active.is_none()
            || !self.enabled
            || !self.auto_show
            || self.shown
            || self
                .attempted
                .is_some_and(|at| now.duration_since(at) < RETRY_DELAY)
        {
            return false;
        }
        self.attempted = Some(now);
        true
    }
}

pub(crate) struct AuxWindow(Mutex<AuxState>);

impl AuxWindow {
    pub(crate) fn new(enabled: bool, auto_show: bool) -> Self {
        Self(Mutex::new(AuxState {
            enabled,
            auto_show,
            active: None,
            shown: false,
            attempted: None,
            automated_client: None,
        }))
    }

    pub(crate) fn enabled(&self) -> bool {
        self.0.lock().is_ok_and(|state| state.enabled)
    }

    pub(crate) fn update_setting(&self, key: &str, value: bool) -> Result<(), &'static str> {
        let mut state = self.0.lock().map_err(|_| "小型辅助面板状态不可用。")?;
        match key {
            "enabled" => state.enabled = value,
            "autoShow" => state.auto_show = value,
            _ => return Err("未知的小型辅助面板设置。"),
        }
        Ok(())
    }

    pub(super) fn mark_manual_show(&self, observation: AuxObservation) {
        if let Ok(mut state) = self.0.lock() {
            state.observe(&observation);
            state.shown = state.active.is_some();
            state.automated_client = None;
        }
    }

    pub(crate) fn reconcile(&self, app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
        let observation = app.state::<ClientRuntime>().aux_observation();
        let existing = app.get_webview_window("aux");
        let hide = {
            let mut state = self.0.lock().map_err(|_| "小型辅助面板状态不可用。")?;
            state.observe(&observation);
            state.automated_client.as_ref().is_some_and(|owner| {
                !owner.alive()
                    || match &observation {
                        AuxObservation::Inactive => true,
                        AuxObservation::Unconfirmed => false,
                        AuxObservation::Active { client, .. } => client.instance != owner.instance,
                    }
            })
        };
        if hide {
            if let Some(window) = existing.as_ref() {
                window.hide()?;
                publish_window_property(window, "show", json!(false))?;
            }
            self.0
                .lock()
                .map_err(|_| "小型辅助面板状态不可用。")?
                .automated_client = None;
        }

        let AuxObservation::Active { client: owner, .. } = observation else {
            return Ok(());
        };
        let active = {
            let mut state = self.0.lock().map_err(|_| "小型辅助面板状态不可用。")?;
            if !state.begin_show(Instant::now()) {
                return Ok(());
            }
            state.active
        };
        let already_visible = existing.is_some_and(|window| window.is_visible().unwrap_or(false));
        let result = if already_visible {
            Ok(())
        } else {
            show_companion(app, AUX_WINDOW, true)
        };
        if result.is_ok()
            || app
                .get_webview_window("aux")
                .is_some_and(|window| window.is_visible().unwrap_or(false))
        {
            let mut state = self.0.lock().map_err(|_| "小型辅助面板状态不可用。")?;
            if state.active == active && !state.shown {
                state.shown = true;
                if !already_visible {
                    state.automated_client = Some(owner);
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT: ClientInstance = ClientInstance {
        pid: 10,
        created_at: 20,
    };

    #[test]
    fn dismissal_in_the_room_does_not_consume_the_selection_popup() {
        let window = AuxWindow::new(true, true);
        let mut state = window.0.lock().unwrap();
        let now = Instant::now();

        state.observe_active(AuxStage::Room, CLIENT, None);
        assert!(state.begin_show(now));
        state.shown = true;
        state.observe_active(AuxStage::Room, CLIENT, Some(0));
        assert!(!state.begin_show(now + RETRY_DELAY));

        state.observe_active(AuxStage::Selection, CLIENT, None);
        assert!(state.begin_show(now));
        state.shown = true;
        state.observe_active(AuxStage::Selection, CLIENT, Some(125));
        assert!(!state.begin_show(now + RETRY_DELAY));
    }

    #[test]
    fn inconclusive_draft_id_and_refresh_keep_a_dismissal_until_a_different_draft() {
        let window = AuxWindow::new(true, true);
        let mut state = window.0.lock().unwrap();
        let now = Instant::now();

        state.observe_active(AuxStage::Selection, CLIENT, Some(125));
        assert!(state.begin_show(now));
        state.shown = true;
        state.observe(&AuxObservation::Unconfirmed);
        state.observe_active(AuxStage::Selection, CLIENT, None);
        state.observe_active(AuxStage::Selection, CLIENT, Some(125));
        assert!(!state.begin_show(now + RETRY_DELAY));

        state.observe_active(AuxStage::Selection, CLIENT, Some(126));
        assert!(state.begin_show(now));
        assert!(!state.begin_show(now));
        assert!(state.begin_show(now + RETRY_DELAY));

        state.observe(&AuxObservation::Inactive);
        assert!(!state.begin_show(now + RETRY_DELAY));
        state.observe_active(AuxStage::Room, CLIENT, None);
        assert!(state.begin_show(now));
    }
}
