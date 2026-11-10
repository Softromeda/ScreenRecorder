// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! System-wide hotkeys that work while other applications have the keyboard focus.
//!
//! [`Hotkeys`] wraps the `global_hotkey` crate: the application registers the key combinations
//! from the settings (for example `Ctrl+F11`) with [`Hotkeys::register`] and calls
//! [`Hotkeys::poll`] every frame to receive the [`HotkeyAction`]s that were triggered. [`parse`]
//! turns the user-friendly text stored in the settings into a [`HotKey`].

use std::str::FromStr;

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

/// What a global hotkey does when pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    /// Start a recording, or pause / resume the current one.
    StartPause,
    /// Stop the current recording and save it.
    Stop,
    /// Discard the current recording.
    Cancel,
    /// Turn drawing on the screen on or off.
    Draw,
    /// Drop a marker in the recording (shown on the editor timeline).
    Marker,
}

/// The set of currently registered global hotkeys.
pub struct Hotkeys {
    /// The OS-level hotkey manager; `None` if it could not be created on this system.
    manager: Option<GlobalHotKeyManager>,
    /// Hotkeys registered by the last [`register`](Self::register) call and their actions.
    registered: Vec<(HotKey, HotkeyAction)>,
    /// Human-readable errors from the last registration (invalid text, key already in use, …),
    /// shown in the options dialog.
    pub errors: Vec<String>,
}

impl Hotkeys {
    /// Creates the hotkey manager. If the system refuses, a warning is logged and hotkeys are
    /// disabled; [`register`](Self::register) then reports an error instead of failing.
    pub fn new() -> Self {
        let manager = match GlobalHotKeyManager::new() {
            Ok(m) => Some(m),
            Err(e) => {
                log::warn!("global hotkeys unavailable: {e}");
                None
            }
        };
        Self {
            manager,
            registered: vec![],
            errors: vec![],
        }
    }

    /// Replaces all registered hotkeys with `keys`, given in textual form such as `"Ctrl+F11"`.
    ///
    /// Empty entries are skipped. Entries that cannot be parsed or registered (for example
    /// because another program already owns the combination) are recorded in
    /// [`errors`](Self::errors); the remaining hotkeys are still registered.
    pub fn register(&mut self, keys: &[(&str, HotkeyAction)]) {
        self.errors.clear();
        let Some(manager) = &self.manager else {
            self.errors
                .push("Global hotkeys could not be registered on this system.".into());
            return;
        };
        // Release the previous set first so a combination can move to another action.
        for (hk, _) in self.registered.drain(..) {
            let _ = manager.unregister(hk);
        }
        for &(text, action) in keys {
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            match parse(text) {
                Ok(hk) => match manager.register(hk) {
                    Ok(()) => self.registered.push((hk, action)),
                    Err(e) => self.errors.push(format!("{text}: {e}")),
                },
                Err(e) => self.errors.push(format!("{text}: {e}")),
            }
        }
    }

    /// Drains pending hotkey events and returns the actions of the keys pressed since the last
    /// call, in order. Key releases and events for unknown hotkeys are ignored.
    pub fn poll(&self) -> Vec<HotkeyAction> {
        let mut out = vec![];
        while let Ok(ev) = GlobalHotKeyEvent::receiver().try_recv() {
            if ev.state() != HotKeyState::Pressed {
                continue;
            }
            if let Some((_, a)) = self.registered.iter().find(|(hk, _)| hk.id() == ev.id()) {
                out.push(*a);
            }
        }
        out
    }
}

/// Parses a hotkey written in a friendly form such as `"Ctrl+Shift+F12"` or `"Cmd+Alt+R"`.
///
/// Modifier names are case-insensitive and accept common aliases (`Ctrl`/`Control`,
/// `Win`/`Cmd`/`Super`/`Meta`, `Alt`/`Option`). Single letters and digits are translated to the
/// key codes `global_hotkey` expects (`KeyR`, `Digit5`); other parts (`F12`, `Space`, …) are
/// passed through unchanged. Returns a readable error if the combination is not recognised.
pub fn parse(text: &str) -> Result<HotKey, String> {
    let normalized: Vec<String> = text
        .split('+')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "control".to_string(),
            "cmd" | "command" | "win" | "super" | "meta" => "super".to_string(),
            "alt" | "option" => "alt".to_string(),
            "shift" => "shift".to_string(),
            other if other.len() == 1 && other.chars().all(|c| c.is_ascii_alphabetic()) => {
                format!("Key{}", other.to_ascii_uppercase())
            }
            other if other.len() == 1 && other.chars().all(|c| c.is_ascii_digit()) => {
                format!("Digit{other}")
            }
            _ => p.to_string(),
        })
        .collect();
    HotKey::from_str(&normalized.join("+")).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Friendly modifier aliases, function keys and digits parse; unknown key names are rejected.
    #[test]
    fn parses_friendly_names() {
        assert!(parse("Ctrl+F11").is_ok());
        assert!(parse("Ctrl+Shift+F12").is_ok());
        assert!(parse("Cmd+Alt+R").is_ok());
        assert!(parse("Ctrl+5").is_ok());
        assert!(parse("Ctrl+Nope").is_err());
    }
}
