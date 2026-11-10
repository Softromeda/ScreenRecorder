// Softromeda Screen Recorder
// Copyright (C) 2026 Softromeda <https://softromeda.com>
// Originally created by Softromeda - https://softromeda.com/screen-recorder
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU General Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version, with the additional terms listed in
// the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

//! User-interface translations.
//!
//! Each language has a plain-text table in `locales/<code>.tsv`, compiled into the program: one
//! line per string, the English text and its translation separated by a TAB. Lines starting with
//! `#` are comments. The English text is the lookup key, so a missing translation falls back to
//! English. `{}` (or `{0}`, `{1}` when a translation needs a different order) mark values inserted
//! at run time. UI code calls [`tr`] for fixed strings and [`tf`] for templates; the active
//! language is a process-wide setting changed with [`set_language`].

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

/// A user-interface language. Serialised by variant name in the settings file, so variants must
/// not be renamed. Variants are listed alphabetically; their order has no effect on behaviour.
///
/// # Adding a language
///
/// 1. Add a variant here, at its alphabetical place.
/// 2. Add its ISO 639-1 code to [`Language::code`] and its native name to
///    [`Language::native_name`].
/// 3. Add an `include_str!` of its table to [`Language::table_source`].
/// 4. Append it to the end of [`Language::ALL`] and insert it at its alphabetical place in
///    [`Language::MENU`], then bump the array lengths (and the length in `tables`).
/// 5. Create `locales/<code>.tsv` with a translation for every key; the
///    `all_strings_are_translated` test lists any that are missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    /// Arabic (`ar`).
    Arabic,
    /// Bengali (`bn`).
    Bengali,
    /// Simplified Chinese (`zh`).
    Chinese,
    /// English (`en`); the source language, needs no table.
    English,
    /// French (`fr`).
    French,
    /// Hindi (`hi`).
    Hindi,
    /// Indonesian (`id`).
    Indonesian,
    /// Korean (`ko`).
    Korean,
    /// Portuguese (`pt`).
    Portuguese,
    /// Russian (`ru`).
    Russian,
    /// Spanish (`es`).
    Spanish,
    /// Urdu (`ur`).
    Urdu,
    /// Uzbek in Latin script (`uz`).
    Uzbek,
}

impl Language {
    /// Every supported language, in storage order. The menu uses [`Language::MENU`] instead.
    ///
    /// The index into this array is what [`set_language`] stores and what [`tables`] is indexed
    /// by. English must stay first because index 0 is the untranslated fast path in [`tr`]. The
    /// order is otherwise arbitrary; new languages are simply appended. It is kept separate from
    /// [`Language::MENU`] so that the display order can change without touching this indexing.
    pub const ALL: [Language; 13] = [
        Language::English,
        Language::Uzbek,
        Language::Russian,
        Language::Spanish,
        Language::French,
        Language::Portuguese,
        Language::Korean,
        Language::Arabic,
        Language::Chinese,
        Language::Hindi,
        Language::Bengali,
        Language::Indonesian,
        Language::Urdu,
    ];

    /// Every supported language in the order shown in the language menu: alphabetical by native
    /// name, Latin-script languages first, then other scripts in Unicode order.
    ///
    /// Must contain exactly the languages of [`Language::ALL`] (checked by a unit test).
    pub const MENU: [Language; 13] = [
        Language::Indonesian,
        Language::English,
        Language::Spanish,
        Language::French,
        Language::Uzbek,
        Language::Portuguese,
        Language::Russian,
        Language::Arabic,
        Language::Urdu,
        Language::Hindi,
        Language::Bengali,
        Language::Chinese,
        Language::Korean,
    ];

    /// Returns the ISO 639-1 code, which is also the locale file name (`locales/<code>.tsv`).
    pub fn code(self) -> &'static str {
        match self {
            Language::English => "en",
            Language::Uzbek => "uz",
            Language::Chinese => "zh",
            Language::Hindi => "hi",
            Language::Spanish => "es",
            Language::Arabic => "ar",
            Language::French => "fr",
            Language::Bengali => "bn",
            Language::Portuguese => "pt",
            Language::Russian => "ru",
            Language::Indonesian => "id",
            Language::Urdu => "ur",
            Language::Korean => "ko",
        }
    }

    /// Returns the language's name written in that language, as shown in the language menu.
    pub fn native_name(self) -> &'static str {
        match self {
            Language::English => "English",
            Language::Uzbek => "Oʻzbekcha",
            Language::Chinese => "简体中文",
            Language::Hindi => "हिन्दी",
            Language::Spanish => "Español",
            Language::Arabic => "العربية",
            Language::French => "Français",
            Language::Bengali => "বাংলা",
            Language::Portuguese => "Português",
            Language::Russian => "Русский",
            Language::Indonesian => "Bahasa Indonesia",
            Language::Urdu => "اردو",
            Language::Korean => "한국어",
        }
    }

    /// Matches a locale name such as `zh-CN`, `pt_BR` or `uz-Latn-UZ` by its language part.
    ///
    /// Returns `None` for languages without a translation.
    pub fn from_locale(locale: &str) -> Option<Self> {
        let code = locale
            .split(['-', '_'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        Self::ALL.into_iter().find(|l| l.code() == code)
    }

    /// Returns the embedded translation table text; empty for English, which needs none.
    fn table_source(self) -> &'static str {
        match self {
            Language::English => "",
            Language::Uzbek => include_str!("../locales/uz.tsv"),
            Language::Chinese => include_str!("../locales/zh.tsv"),
            Language::Hindi => include_str!("../locales/hi.tsv"),
            Language::Spanish => include_str!("../locales/es.tsv"),
            Language::Arabic => include_str!("../locales/ar.tsv"),
            Language::French => include_str!("../locales/fr.tsv"),
            Language::Bengali => include_str!("../locales/bn.tsv"),
            Language::Portuguese => include_str!("../locales/pt.tsv"),
            Language::Russian => include_str!("../locales/ru.tsv"),
            Language::Indonesian => include_str!("../locales/id.tsv"),
            Language::Urdu => include_str!("../locales/ur.tsv"),
            Language::Korean => include_str!("../locales/ko.tsv"),
        }
    }
}

/// Translation table: English text to translated text.
type Table = HashMap<&'static str, &'static str>;

/// Parses a `.tsv` translation table.
///
/// Blank lines, `#` comments, lines without a TAB and entries with an empty translation are
/// skipped, so an untranslated entry falls back to English.
fn parse(source: &'static str) -> Table {
    source
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('\t'))
        .filter(|(_, v)| !v.trim().is_empty())
        .collect()
}

/// Returns the parsed tables of all languages, indexed like [`Language::ALL`]; parsed on first use.
fn tables() -> &'static [Table; 13] {
    static TABLES: OnceLock<[Table; 13]> = OnceLock::new();
    TABLES.get_or_init(|| Language::ALL.map(|l| parse(l.table_source())))
}

/// Index into [`Language::ALL`] of the active language. An atomic so that [`tr`] can be called
/// from any thread without locking.
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Makes `lang` the active language for all subsequent [`tr`] and [`tf`] calls.
pub fn set_language(lang: Language) {
    let idx = Language::ALL.iter().position(|l| *l == lang).unwrap_or(0);
    CURRENT.store(idx as u8, Ordering::Relaxed);
}

/// Returns the active language.
pub fn language() -> Language {
    Language::ALL[CURRENT.load(Ordering::Relaxed) as usize % Language::ALL.len()]
}

/// Translates a fixed UI string into the active language.
///
/// `english` is both the lookup key and the fallback when no translation exists. The string
/// must be a literal so that the `all_strings_are_translated` test can find it.
pub fn tr(english: &'static str) -> &'static str {
    let idx = CURRENT.load(Ordering::Relaxed) as usize;
    if idx == 0 {
        return english;
    }
    tables()[idx].get(english).copied().unwrap_or(english)
}

/// Translates a template and fills in its `{}` or `{0}`, `{1}`, ... placeholders with `args`.
pub fn tf(english: &'static str, args: &[&dyn Display]) -> String {
    fill(tr(english), args)
}

/// Substitutes `args` into `template`.
///
/// If the template uses any numbered placeholder (`{0}`, `{1}`, ...), every `{i}` is replaced by
/// `args[i]`, which lets translations reorder values. Otherwise each `{}` is replaced in turn;
/// surplus arguments are ignored.
fn fill(template: &str, args: &[&dyn Display]) -> String {
    let mut out = template.to_string();
    if (0..args.len()).any(|i| template.contains(&format!("{{{i}}}"))) {
        for (i, a) in args.iter().enumerate() {
            out = out.replace(&format!("{{{i}}}"), &a.to_string());
        }
    } else {
        for a in args {
            if let Some(pos) = out.find("{}") {
                out.replace_range(pos..pos + 2, &a.to_string());
            }
        }
    }
    out
}

/// Returns the menu label for a language choice; `None` means "follow Windows" and also names
/// the detected system language.
fn choice_name(choice: Option<Language>) -> String {
    match choice {
        None => format!(
            "{} ({})",
            tr("Same as Windows"),
            system_language().native_name()
        ),
        Some(l) => l.native_name().to_string(),
    }
}

/// Shows the languages as selectable entries inside a menu and closes the menu on a choice.
///
/// `choice` is the user's setting: `None` follows the Windows display language. The list scrolls
/// when the window is too short to show every language.
pub fn language_menu(ui: &mut egui::Ui, choice: &mut Option<Language>) {
    let room = ui.ctx().content_rect().bottom() - ui.next_widget_position().y - 16.0;
    egui::ScrollArea::vertical()
        .max_height(room.max(120.0))
        .show(ui, |ui| {
            for c in std::iter::once(None).chain(Language::MENU.into_iter().map(Some)) {
                if ui.selectable_label(*choice == c, choice_name(c)).clicked() {
                    *choice = c;
                    ui.close();
                }
            }
        });
}

/// Shows a drop-down list of languages for the Options window; `None` follows Windows.
pub fn language_combo(ui: &mut egui::Ui, choice: &mut Option<Language>) {
    egui::ComboBox::from_id_salt("language")
        .width(220.0)
        .selected_text(choice_name(*choice))
        .show_ui(ui, |ui| {
            for c in std::iter::once(None).chain(Language::MENU.into_iter().map(Some)) {
                ui.selectable_value(choice, c, choice_name(c));
            }
        });
}

/// Returns the Windows display language if it is supported, otherwise English.
///
/// Detected once and cached for the lifetime of the process.
pub fn system_language() -> Language {
    static SYSTEM: OnceLock<Language> = OnceLock::new();
    *SYSTEM.get_or_init(detect_system_language)
}

/// Reads the user's default locale name from Windows and maps it to a [`Language`].
fn detect_system_language() -> Language {
    #[cfg(windows)]
    {
        // LOCALE_NAME_MAX_LENGTH is 85 UTF-16 units; the returned length includes the
        // terminating NUL, so 0 (failure) and 1 (empty name) are both rejected.
        let mut buf = [0u16; 85];
        // SAFETY: the buffer is a valid, writable slice whose length is passed by the binding.
        let n = unsafe { windows::Win32::Globalization::GetUserDefaultLocaleName(&mut buf) };
        if n > 1 {
            let name = String::from_utf16_lossy(&buf[..(n - 1) as usize]);
            if let Some(l) = Language::from_locale(&name) {
                return l;
            }
        }
    }
    Language::English
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locale names with region and script suffixes map to the right language; unsupported
    /// languages map to `None`.
    #[test]
    fn locale_matching() {
        assert_eq!(Language::from_locale("zh-CN"), Some(Language::Chinese));
        assert_eq!(Language::from_locale("pt_BR"), Some(Language::Portuguese));
        assert_eq!(Language::from_locale("uz-Latn-UZ"), Some(Language::Uzbek));
        assert_eq!(Language::from_locale("ko-KR"), Some(Language::Korean));
        assert_eq!(Language::from_locale("ja-JP"), None);
    }

    /// The menu lists every language exactly once.
    #[test]
    fn menu_has_every_language() {
        for lang in Language::ALL {
            let n = Language::MENU.iter().filter(|l| **l == lang).count();
            assert_eq!(n, 1, "{lang:?} appears {n} times in Language::MENU");
        }
    }

    /// Positional `{}` and numbered `{0}`/`{1}` placeholders are filled in correctly.
    #[test]
    fn placeholders() {
        assert_eq!(fill("Saved {}", &[&"a.mp4"]), "Saved a.mp4");
        assert_eq!(fill("{1} / {0}", &[&1, &2]), "2 / 1");
        assert_eq!(fill("{}×{} area", &[&640, &480]), "640×480 area");
    }

    /// Every string passed to `tr(...)` / `tf(...)` in the UI sources is translated in every
    /// language with the same number of placeholders, and no table has unused entries.
    #[test]
    fn all_strings_are_translated() {
        let sources = [
            include_str!("app.rs"),
            include_str!("editor/mod.rs"),
            include_str!("editor/project.rs"),
            include_str!("tools.rs"),
            include_str!("selector.rs"),
            include_str!("settings.rs"),
            include_str!("theme.rs"),
            include_str!("i18n.rs"),
            include_str!("editor/export.rs"),
            include_str!("editor/render.rs"),
            include_str!("overlay.rs"),
            include_str!("library.rs"),
            include_str!("tray.rs"),
        ];
        // Collect the string literal following each `tr(` / `tf(`; literals with escapes are
        // skipped because their source text differs from the runtime key.
        let mut keys = std::collections::BTreeSet::new();
        for src in sources {
            for marker in ["tr", "tf"].map(|m| format!("{m}(")) {
                for part in src.split(marker.as_str()).skip(1) {
                    // rustfmt may move the literal to the next line.
                    let Some(part) = part.trim_start().strip_prefix('"') else {
                        continue;
                    };
                    if let Some(end) = part.find('"') {
                        let key = &part[..end];
                        if !key.contains('\\') {
                            keys.insert(key.to_string());
                        }
                    }
                }
            }
        }
        // Guard against the scan silently finding nothing.
        assert!(keys.len() > 100, "found only {} keys", keys.len());
        let placeholders = |s: &str| s.matches("{").count();
        // Check every translated language (English is skipped) in both directions.
        let mut problems = vec![];
        for lang in Language::ALL.into_iter().skip(1) {
            let table = parse(lang.table_source());
            for k in &keys {
                match table.get(k.as_str()) {
                    None => problems.push(format!("{}: missing \"{k}\"", lang.code())),
                    Some(v) if placeholders(v) != placeholders(k) => {
                        problems.push(format!("{}: placeholders differ in \"{k}\"", lang.code()))
                    }
                    _ => {}
                }
            }
            for k in table.keys() {
                if !keys.contains(*k) {
                    problems.push(format!("{}: unused \"{k}\"", lang.code()));
                }
            }
        }
        assert!(
            problems.is_empty(),
            "{} problems:\n{}",
            problems.len(),
            problems.join("\n")
        );
    }
}
