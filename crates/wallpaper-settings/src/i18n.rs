//! Minimal localization layer: English source strings are the lookup keys, and the
//! catalog for the user's locale (taken from `LC_ALL`/`LC_MESSAGES`/`LANGUAGE`/`LANG`)
//! supplies the translation. Anything missing falls back to the English text.
//! Unit tests always run in English so assertions stay locale-independent.

use std::collections::HashMap;
use std::sync::OnceLock;

mod ru;

type Catalog = HashMap<&'static str, &'static str>;

fn detect_language() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANGUAGE", "LANG"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .find(|v| !v.is_empty() && v != "C" && v != "POSIX")
        .map(|v| v.to_ascii_lowercase())
}

fn catalog() -> Option<&'static Catalog> {
    static CATALOG: OnceLock<Option<Catalog>> = OnceLock::new();
    CATALOG
        .get_or_init(|| {
            if cfg!(test) {
                return None;
            }
            let lang = detect_language()?;
            lang.starts_with("ru").then(|| ru::MESSAGES.iter().copied().collect())
        })
        .as_ref()
}

/// Translates a UI string; returns the English text when no translation exists.
pub fn tr(message: &str) -> String {
    catalog().and_then(|c| c.get(message)).copied().unwrap_or(message).to_string()
}

/// Like [`tr`], substituting each `{name}` placeholder with its value.
pub fn trf(message: &str, args: &[(&str, &str)]) -> String {
    let mut text = tr(message);
    for (name, value) in args {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_placeholders() {
        assert_eq!(trf("Remove {name}?", &[("name", "x")]), "Remove x?");
    }

    #[test]
    fn russian_catalog_keeps_placeholders() {
        let mut seen = std::collections::HashSet::new();
        for (en, ru) in ru::MESSAGES {
            assert!(seen.insert(*en), "duplicate key: {en}");
            for part in en.split('{').skip(1) {
                let name = part.split('}').next().unwrap_or_default();
                assert!(ru.contains(&format!("{{{name}}}")), "{ru:?} lost {{{name}}}");
            }
        }
    }
}
