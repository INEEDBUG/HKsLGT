//! Automatic display of the bundled auxiliary panel during a local ARAM draft.

use super::{publish_window_property, show_companion, AUX_WINDOW};
use crate::{
    aram_automation::SelectionScope, client_discovery::LocalClient, client_runtime::ClientRuntime,
};
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Manager};

const RETRY_DELAY: Duration = Duration::from_secs(2);

struct AuxState {
    enabled: bool,
    auto_show: bool,
    shown: Option<SelectionScope>,
    attempted: Option<(SelectionScope, Instant)>,
    automated_client: Option<Arc<LocalClient>>,
}

pub(crate) struct AuxWindow(Mutex<AuxState>);

impl AuxWindow {
    pub(crate) fn new(enabled: bool, auto_show: bool) -> Self {
        Self(Mutex::new(AuxState {
            enabled,
            auto_show,
            shown: None,
            attempted: None,
            automated_client: None,
        }))
    }

    pub(crate) fn auto_enabled(&self) -> bool {
        self.0
            .lock()
            .is_ok_and(|state| state.enabled && state.auto_show)
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

    pub(super) fn mark_manual_show(&self, scope: Option<SelectionScope>) {
        if let Ok(mut state) = self.0.lock() {
            if let Some(scope) = scope {
                state.shown = Some(scope);
            }
            state.automated_client = None;
        }
    }

    pub(crate) fn reconcile(&self, app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
        let client = app.state::<ClientRuntime>();
        let existing = app.get_webview_window("aux");
        let ended = client.selection_ended();
        let hide = self
            .0
            .lock()
            .map_err(|_| "小型辅助面板状态不可用。")?
            .automated_client
            .as_ref()
            .is_some_and(|owner| ended || !owner.alive());
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

        let Some((scope, owner)) = client.aux_draft() else {
            // A missing selection sample is inconclusive. Keep the current draft latch.
            return Ok(());
        };
        {
            let mut state = self.0.lock().map_err(|_| "小型辅助面板状态不可用。")?;
            if !state.enabled
                || !state.auto_show
                || state.shown == Some(scope)
                || state
                    .attempted
                    .is_some_and(|(draft, at)| draft == scope && at.elapsed() < RETRY_DELAY)
            {
                return Ok(());
            }
            state.attempted = Some((scope, Instant::now()));
        }
        let already_visible = existing.is_some_and(|window| window.is_visible().unwrap_or(false));
        if !already_visible {
            show_companion(app, AUX_WINDOW, true)?;
        }
        let mut state = self.0.lock().map_err(|_| "小型辅助面板状态不可用。")?;
        state.shown = Some(scope);
        if !already_visible {
            state.automated_client = Some(owner);
        }
        Ok(())
    }
}
