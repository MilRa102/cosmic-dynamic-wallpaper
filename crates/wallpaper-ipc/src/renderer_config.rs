//! [`OutputId`], [`OutputAssignment`], and [`RendererConfig`] — spec 3's per-output pack
//! assignment `cosmic-config` schema (data-model.md `OutputAssignmentRequest`,
//! contracts/renderer-config-schema.md), extracted here (spec 7 research.md R2) as the
//! single source of truth `crates/renderer`, `crates/wallpaperctl`, and
//! `crates/wallpaper-settings` all depend on, instead of each independently defining
//! (and risking drifting) their own copy — the exact bug class this project already hit
//! once (see this module's own regression test).

use std::collections::HashMap;
use std::fmt;

use cosmic_config::cosmic_config_derive::CosmicConfigEntry;
use cosmic_config::{Config, CosmicConfigEntry};
use serde::{Deserialize, Serialize};

use pack_loader::PackSource;

/// `cosmic-config` application id for [`RendererConfig`] — shared by every reader/writer.
pub const RENDERER_CONFIG_ID: &str = "com.system76.CosmicDynamicWallpaper.Renderer";

/// The pre-rename application id (spec 009-project-rename, FR-004a) —
/// [`RendererConfig::migrate_from_old_app_id`] reads this once so an existing
/// installation's settings survive the rename, never written to again afterward.
const OLD_RENDERER_CONFIG_ID: &str = "com.system76.CosmicWallpaper.Renderer";

/// A stable identifier for a physical Wayland output, derived from `xdg-output`'s
/// reported connector name (e.g. `"eDP-1"`, `"DP-3"`) — data-model.md `OutputId`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OutputId(String);

/// The longest identifier [`OutputId::validated`] accepts (spec 011 US4/US5, FR-017/
/// FR-019) — generous headroom over any real connector name (`"eDP-1"`, `"DP-3"`, ...
/// are a handful of bytes), while still bounding the two untrusted boundaries that
/// construct an `OutputId` from external input: the D-Bus `output_id` argument and
/// `wallpaperctl assign --output`.
pub const MAX_OUTPUT_ID_BYTES: usize = 256;

/// Why [`OutputId::validated`] rejected a candidate identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputIdError {
    /// The identifier was empty.
    Empty,
    /// The identifier exceeded [`MAX_OUTPUT_ID_BYTES`].
    TooLong {
        /// The rejected identifier's actual length, in bytes.
        len: usize,
    },
    /// The identifier contained a byte outside the charset every real Wayland
    /// connector name uses (spec 011 US5 FR-019 — the audit's own reproduction,
    /// `--output "DP-3;rm -rf /"`, is only caught by this check; it's well within the
    /// length limit and non-empty).
    InvalidCharacters,
}

impl fmt::Display for OutputIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutputIdError::Empty => write!(f, "output id must not be empty"),
            OutputIdError::TooLong { len } => {
                write!(f, "output id is {len} bytes, longer than the {MAX_OUTPUT_ID_BYTES}-byte limit")
            }
            OutputIdError::InvalidCharacters => {
                write!(f, "output id must contain only ASCII letters, digits, '-', and '_' (real connector names, e.g. \"eDP-1\", \"DP-3\", never do otherwise)")
            }
        }
    }
}

impl std::error::Error for OutputIdError {}

impl OutputId {
    /// Wrap a connector-name string as an opaque [`OutputId`] with no validation —
    /// for trusted internal construction from a real Wayland connector name (spec 3's
    /// own compositor-reported strings), never from CLI/D-Bus input directly.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Construct an [`OutputId`] from untrusted input (a D-Bus method argument, or
    /// `wallpaperctl assign --output`'s value — spec 011 US4/US5, FR-017/FR-019,
    /// research.md R13), rejecting an empty, overlong, or oddly-shaped identifier
    /// rather than silently accepting a value that can never match a real output.
    /// Every real Wayland connector name (`"eDP-1"`, `"DP-3"`, `"HDMI-A-1"`,
    /// `"Virtual-1"`) is ASCII letters/digits/`-`/`_` only — the character check below
    /// is generous within that shape, not a hand-picked allow-list of known prefixes.
    pub fn validated(id: impl Into<String>) -> Result<Self, OutputIdError> {
        let id = id.into();
        if id.is_empty() {
            return Err(OutputIdError::Empty);
        }
        if id.len() > MAX_OUTPUT_ID_BYTES {
            return Err(OutputIdError::TooLong { len: id.len() });
        }
        if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(OutputIdError::InvalidCharacters);
        }
        Ok(Self(id))
    }

    /// Borrow the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OutputId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The longest monitor key [`monitor_key`] produces.
pub const MAX_MONITOR_KEY_BYTES: usize = 200;

/// A stable identifier for a *physical monitor*, independent of the port it's plugged
/// into, from the most specific EDID data available:
/// 1. `edid-<make>-<model>-<serial>` — a real serial number (via `wlr-output-management`);
/// 2. `model-<make>-<model>` — no usable serial, but the make/model are known;
/// 3. `output-<connector>` — the output name, only when the EDID reports neither.
///
/// Every non `[A-Za-z0-9_-]` byte becomes `_`, so a key is always a valid
/// [`OutputId::validated`] string as well (CLI/D-Bus safe).
pub fn monitor_key(make: &str, model: &str, serial: Option<&str>, connector: &str) -> String {
    let sanitize = |s: &str| -> String {
        s.trim().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
    };
    let known = |s: &str| {
        let s = s.trim();
        !s.is_empty() && !s.eq_ignore_ascii_case("unknown")
    };
    let serial = serial.map(str::trim).filter(|s| is_real_serial(s));
    let mut key = match serial {
        Some(serial) => format!("edid-{}-{}-{}", sanitize(make), sanitize(model), sanitize(serial)),
        None if known(make) || known(model) => format!("model-{}-{}", sanitize(make), sanitize(model)),
        None => format!("output-{}", sanitize(connector)),
    };
    key.truncate(MAX_MONITOR_KEY_BYTES);
    key
}

/// Rejects the placeholder serials many panels report instead of a real one —
/// empty, "Unknown", a repeated character ("0000", "1111"), or 0x01010101.
fn is_real_serial(serial: &str) -> bool {
    let mut chars = serial.chars();
    let Some(first) = chars.next() else { return false };
    !serial.eq_ignore_ascii_case("unknown") && serial != "16843009" && !chars.all(|c| c == first)
}

/// Makes every key in `monitors` unique: monitors whose EDID data can't tell them
/// apart (identical serial-less models, or the same placeholder serial) fall back to
/// their output name, so they can still be configured separately.
pub fn dedupe_monitor_keys(monitors: &mut [ConnectedMonitor]) {
    let keys: Vec<String> = monitors.iter().map(|m| m.key.clone()).collect();
    for monitor in monitors.iter_mut() {
        if keys.iter().filter(|k| **k == monitor.key).count() > 1 {
            monitor.key = monitor_key("", "", None, &monitor.connector);
        }
    }
}

/// The persisted per-monitor setting — survives the monitor being unplugged (it's
/// then just "disabled": known, not connected) until the user forgets it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    /// The pack shown on this monitor; `None` = the default pack
    /// (`RendererConfig.same_pack_everywhere`).
    pub pack: Option<PackSource>,
    /// Human-readable name ("Dell U2720Q"), refreshed by the daemon on connect.
    pub label: String,
    /// The connector it was last seen on ("DP-3") — display only, never used to match.
    pub last_connector: String,
}

/// A monitor the daemon currently drives, as it reports it (`ListMonitors`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectedMonitor {
    /// [`monitor_key`].
    pub key: String,
    /// The connector it's on right now.
    pub connector: String,
    /// Human-readable name.
    pub label: String,
}

/// What one output should display — exactly one of, per output (data-model.md
/// `OutputAssignment`, FR-005, FR-006).
#[derive(Debug, Clone, PartialEq)]
pub enum OutputAssignment {
    /// An explicit per-output override — always takes precedence over the toggle.
    Explicit(PackSource),
    /// No override; follows `RendererConfig.same_pack_everywhere` if it's `Some`.
    FollowsToggle,
    /// No override, and the toggle is off (or has no pack chosen) — a well-defined
    /// empty state, not an error (FR-009).
    Unassigned,
}

/// The "same pack on all outputs" toggle, per-output overrides, and the crossfade
/// duration — this project's own `cosmic-config` schema (data-model.md `RendererConfig`,
/// FR-005–FR-007; spec 7 adds `crossfade_duration_secs`, contracts/gui-application.md).
///
/// **`overrides` is keyed by plain `String`, not [`OutputId`]** — found to matter for
/// real, not just in principle, while manually testing this crate against a live
/// `wallpaperctl`-written config (2026-08-13): RON deserializes a `HashMap<OutputId,
/// _>` key expecting `OutputId`'s newtype-struct textual form, not a bare string — the
/// two are *not* wire-compatible despite `OutputId` being "just a string wrapper", so a
/// mismatch here silently produces an empty `overrides` map (RON parse error swallowed
/// by `load`'s `unwrap_or_else` fallback to `Default`) rather than the assignment that
/// was actually on disk. `String` guarantees byte-for-byte compatibility.
#[derive(Debug, Clone, PartialEq, CosmicConfigEntry)]
#[version = 1]
pub struct RendererConfig {
    /// `None` = the toggle is off.
    pub same_pack_everywhere: Option<PackSource>,
    /// Explicit per-output overrides, keyed by output identifier (as a plain string —
    /// see this struct's doc comment for why not `OutputId`).
    pub overrides: HashMap<String, PackSource>,
    /// Crossfade transition duration in seconds (spec 7 FR-006). Defaults to `45` —
    /// spec 3's pre-spec-7 `CROSSFADE_DURATION` constant value, so upgrading changes
    /// nothing until a user visits the GUI's Crossfade page or runs a future CLI
    /// equivalent.
    pub crossfade_duration_secs: u32,
    /// Per-monitor settings keyed by [`monitor_key`] — the stable, port-independent
    /// successor to `overrides`. An entry here is authoritative for its monitor
    /// (`pack: None` = default pack); `overrides` only applies to monitors with no
    /// entry yet. Absent on disk for configs written before this field existed,
    /// which `cosmic-config`'s per-key fallback turns into an empty map.
    pub monitors: HashMap<String, MonitorConfig>,
}

impl Default for RendererConfig {
    fn default() -> Self {
        Self { same_pack_everywhere: None, overrides: HashMap::new(), crossfade_duration_secs: 45, monitors: HashMap::new() }
    }
}

impl RendererConfig {
    /// Open the real, user-global renderer config.
    pub fn open() -> Result<Config, cosmic_config::Error> {
        Config::new(RENDERER_CONFIG_ID, Self::VERSION)
    }

    /// Open a renderer config rooted at a custom path — test-only.
    #[doc(hidden)]
    pub fn open_at(custom_path: &std::path::Path) -> Result<Config, cosmic_config::Error> {
        Config::with_custom_path(RENDERER_CONFIG_ID, Self::VERSION, custom_path.to_path_buf())
    }

    /// Read the current entry, falling back to the default (empty overrides, toggle
    /// off, 45s crossfade) if nothing has been written yet.
    pub fn load(config: &Config) -> Self {
        Self::load_reporting_corruption(config).0
    }

    /// [`Self::load`], but also reports whether the fallback-to-default path was
    /// taken because of a genuine read/parse error, not just "never configured"
    /// (spec 011 US6 FR-023, research.md R18) — see
    /// [`crate::LocationConfigEntry::load_reporting_corruption`]'s doc comment for the
    /// full rationale; this is the identical fix applied to the renderer config.
    pub fn load_reporting_corruption(config: &Config) -> (Self, bool) {
        match Self::get_entry(config) {
            Ok(entry) => (entry, false),
            Err((errors, default)) => {
                let corrupted = errors.iter().any(cosmic_config::Error::is_err);
                if corrupted {
                    tracing::warn!(?errors, "renderer config could not be fully read — falling back to defaults for the affected field(s)");
                }
                (default, corrupted)
            }
        }
    }

    /// Persist this entry. On Unix, best-effort pre-creates the target directory at
    /// `0700` before writing and tightens it again afterward — same fix as
    /// `LocationConfigEntry::save` (see its doc comment for the full rationale,
    /// including spec 011 adversarial re-review finding 2), applied here since
    /// per-output assignments can also reveal locally-sensitive filesystem paths.
    pub fn save(&self, config: &Config) -> Result<(), cosmic_config::Error> {
        #[cfg(unix)]
        crate::ensure_config_dir_permissions_before_write(RENDERER_CONFIG_ID, Self::VERSION);
        self.write_entry(config)?;
        #[cfg(unix)]
        crate::tighten_config_dir_permissions(RENDERER_CONFIG_ID, Self::VERSION);
        Ok(())
    }

    /// Load `new_config`, migrating forward from the pre-rename application id
    /// (spec 009-project-rename, FR-004a/contracts/config-migration.md) if nothing has
    /// been written under the new id yet. Use this in place of a bare [`Self::load`] at
    /// every startup call site — see `contracts/config-migration.md` for the full
    /// behavior contract (idempotent, never mutates the old store, a fresh install with
    /// no old store at all is a silent no-op, not an error).
    pub fn migrate_from_old_app_id(new_config: &Config) -> Self {
        Self::migrate_core(new_config, Config::new(OLD_RENDERER_CONFIG_ID, Self::VERSION))
    }

    /// The pure migration core, taking both handles already open — split out from
    /// [`Self::migrate_from_old_app_id`] purely so tests can construct the old handle
    /// via [`Self::open_at`]'s custom-path equivalent instead of the real,
    /// well-known system path `Config::new` always resolves to.
    fn migrate_core(new_config: &Config, old_config: Result<Config, cosmic_config::Error>) -> Self {
        let current = Self::load(new_config);
        if current != Self::default() {
            return current; // already migrated, or configured fresh under the new id
        }
        let Ok(old_config) = old_config else {
            return current; // no old store at all — fresh install, not an error
        };
        let old = Self::load(&old_config);
        if old == Self::default() {
            return current; // old install existed but was never actually configured
        }
        // Best-effort: a write failure here just means the next call retries the same
        // migration (the old store is never mutated, so nothing is lost either way).
        let _ = old.save(new_config);
        old
    }
}

impl RendererConfig {
    /// Records newly connected monitors (and refreshes the label/connector of known
    /// ones) — a reconnect finds its old entry and therefore its old pack. A new
    /// monitor inherits a legacy `overrides` entry for its connector, if any, so
    /// per-port setups keep working after upgrading. Returns whether anything changed.
    pub fn register_monitors(&mut self, connected: &[ConnectedMonitor]) -> bool {
        let mut changed = false;
        for monitor in connected {
            match self.monitors.get_mut(&monitor.key) {
                Some(entry) => {
                    if entry.label != monitor.label || entry.last_connector != monitor.connector {
                        entry.label.clone_from(&monitor.label);
                        entry.last_connector.clone_from(&monitor.connector);
                        changed = true;
                    }
                }
                None => {
                    let pack = self.overrides.get(&monitor.connector).cloned();
                    self.monitors.insert(
                        monitor.key.clone(),
                        MonitorConfig { pack, label: monitor.label.clone(), last_connector: monitor.connector.clone() },
                    );
                    changed = true;
                }
            }
        }
        changed
    }

    /// Writes only the `monitors` key, after re-reading it from disk — so the daemon
    /// registering a monitor never clobbers anything the GUI/CLI wrote meanwhile.
    pub fn save_registered_monitors(config: &Config, connected: &[ConnectedMonitor]) -> Result<bool, cosmic_config::Error> {
        let mut current = Self::load(config);
        if !current.register_monitors(connected) {
            return Ok(false);
        }
        #[cfg(unix)]
        crate::ensure_config_dir_permissions_before_write(RENDERER_CONFIG_ID, Self::VERSION);
        cosmic_config::ConfigSet::set(config, "monitors", &current.monitors)?;
        #[cfg(unix)]
        crate::tighten_config_dir_permissions(RENDERER_CONFIG_ID, Self::VERSION);
        Ok(true)
    }

    /// The monitor keys that have a stored setting but aren't in `connected` —
    /// the "disabled" monitors, sorted for stable display.
    pub fn disconnected_monitors(&self, connected: &[ConnectedMonitor]) -> Vec<String> {
        let mut keys: Vec<String> =
            self.monitors.keys().filter(|key| !connected.iter().any(|m| &m.key == *key)).cloned().collect();
        keys.sort();
        keys
    }
}

/// Resolve the assignment for `output`, which is the physical monitor `monitor` (its
/// [`monitor_key`], when known). Order: the monitor's own entry (`pack: None` falls
/// through to the default pack) > a legacy per-connector override > the default pack.
pub fn resolve_monitor_assignment(output: &OutputId, monitor: Option<&str>, config: &RendererConfig) -> OutputAssignment {
    if let Some(entry) = monitor.and_then(|key| config.monitors.get(key)) {
        return match &entry.pack {
            Some(source) => OutputAssignment::Explicit(source.clone()),
            None if config.same_pack_everywhere.is_some() => OutputAssignment::FollowsToggle,
            None => OutputAssignment::Unassigned,
        };
    }
    resolve_assignment(output, config)
}

/// Resolve `output`'s [`OutputAssignment`] from the current [`RendererConfig`]
/// (data-model.md's Resolution rule, FR-005–FR-007): an `overrides` entry always wins;
/// else `FollowsToggle` if the toggle is set; else `Unassigned`.
pub fn resolve_assignment(output: &OutputId, config: &RendererConfig) -> OutputAssignment {
    if let Some(source) = config.overrides.get(output.as_str()) {
        OutputAssignment::Explicit(source.clone())
    } else if config.same_pack_everywhere.is_some() {
        OutputAssignment::FollowsToggle
    } else {
        OutputAssignment::Unassigned
    }
}

/// The actual [`PackSource`] an [`OutputAssignment`] currently points at, re-derived
/// from the *current* config each time (not cached in the assignment itself) — so a
/// change to the toggle's chosen pack is picked up by every `FollowsToggle` output
/// without each one needing its own copy.
pub fn effective_pack<'a>(assignment: &'a OutputAssignment, config: &'a RendererConfig) -> Option<&'a PackSource> {
    match assignment {
        OutputAssignment::Explicit(source) => Some(source),
        OutputAssignment::FollowsToggle => config.same_pack_everywhere.as_ref(),
        OutputAssignment::Unassigned => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(path: &str) -> PackSource {
        PackSource::StaticFile(path.into())
    }

    #[test]
    fn output_id_validated_rejects_empty() {
        assert_eq!(OutputId::validated(""), Err(OutputIdError::Empty));
    }

    #[test]
    fn output_id_validated_rejects_oversized() {
        let too_long = "x".repeat(MAX_OUTPUT_ID_BYTES + 1);
        assert_eq!(OutputId::validated(too_long.clone()), Err(OutputIdError::TooLong { len: too_long.len() }));
    }

    #[test]
    fn output_id_validated_accepts_real_connector_names() {
        assert_eq!(OutputId::validated("DP-3").unwrap(), OutputId::new("DP-3"));
        assert_eq!(OutputId::validated("eDP-1").unwrap(), OutputId::new("eDP-1"));
        assert_eq!(OutputId::validated("HDMI-A-1").unwrap(), OutputId::new("HDMI-A-1"));
        assert_eq!(OutputId::validated("Virtual-1").unwrap(), OutputId::new("Virtual-1"));
        // Exactly at the limit is still accepted (only *over* the limit is rejected).
        let at_limit = "x".repeat(MAX_OUTPUT_ID_BYTES);
        assert!(OutputId::validated(at_limit).is_ok());
    }

    /// Spec 011 US5 FR-019 (research.md R13/R15) — the audit's own reproduction:
    /// `--output "DP-3;rm -rf /"` is non-empty and well within the length limit, so
    /// only a character-class check catches it.
    #[test]
    fn output_id_validated_rejects_shell_metacharacters() {
        assert_eq!(OutputId::validated("DP-3;rm -rf /"), Err(OutputIdError::InvalidCharacters));
        assert_eq!(OutputId::validated("DP-3\n"), Err(OutputIdError::InvalidCharacters));
        assert_eq!(OutputId::validated("../etc/passwd"), Err(OutputIdError::InvalidCharacters));
    }

    #[test]
    fn explicit_override_always_wins() {
        let mut config = RendererConfig { same_pack_everywhere: Some(source("/toggle.jpg")), ..RendererConfig::default() };
        config.overrides.insert("DP-3".to_string(), source("/override.jpg"));

        let assignment = resolve_assignment(&OutputId::new("DP-3"), &config);
        assert_eq!(assignment, OutputAssignment::Explicit(source("/override.jpg")));
        assert_eq!(effective_pack(&assignment, &config), Some(&source("/override.jpg")));
    }

    #[test]
    fn no_override_follows_toggle_when_set() {
        let config = RendererConfig { same_pack_everywhere: Some(source("/toggle.jpg")), ..RendererConfig::default() };
        let assignment = resolve_assignment(&OutputId::new("eDP-1"), &config);
        assert_eq!(assignment, OutputAssignment::FollowsToggle);
        assert_eq!(effective_pack(&assignment, &config), Some(&source("/toggle.jpg")));
    }

    #[test]
    fn no_override_and_toggle_off_is_unassigned() {
        let config = RendererConfig::default();
        let assignment = resolve_assignment(&OutputId::new("eDP-1"), &config);
        assert_eq!(assignment, OutputAssignment::Unassigned);
        assert_eq!(effective_pack(&assignment, &config), None);
    }

    fn connected(key: &str, connector: &str) -> ConnectedMonitor {
        ConnectedMonitor { key: key.into(), connector: connector.into(), label: format!("Monitor {key}") }
    }

    #[test]
    fn monitor_key_prefers_the_serial_and_ignores_the_port() {
        let a = monitor_key("Dell Inc.", "U2720Q", Some("ABC123"), "DP-3");
        assert_eq!(a, "edid-Dell_Inc_-U2720Q-ABC123");
        assert_eq!(a, monitor_key("Dell Inc.", "U2720Q", Some("ABC123"), "HDMI-A-1"));
        assert!(OutputId::validated(a).is_ok());
    }

    #[test]
    fn monitor_key_without_a_usable_serial_uses_make_and_model_not_the_port() {
        for serial in [None, Some("  "), Some("0000"), Some("Unknown"), Some("1111"), Some("16843009"), Some("7")] {
            assert_eq!(monitor_key("BOE", "0x0BCA", serial, "eDP-1"), "model-BOE-0x0BCA");
            assert_eq!(monitor_key("BOE", "0x0BCA", serial, "HDMI-A-1"), "model-BOE-0x0BCA");
        }
        assert!(OutputId::validated(monitor_key("Dell Inc.", "U2720Q", None, "DP-3")).is_ok());
    }

    #[test]
    fn monitor_key_falls_back_to_the_output_name_without_any_edid_data() {
        assert_eq!(monitor_key("", "", None, "Virtual-1"), "output-Virtual-1");
        assert_eq!(monitor_key("Unknown", " ", Some("0"), "Virtual-1"), "output-Virtual-1");
    }

    #[test]
    fn identical_serial_less_monitors_fall_back_to_their_output_names() {
        let mut monitors = vec![
            connected(&monitor_key("Dell", "P2419H", None, "DP-1"), "DP-1"),
            connected(&monitor_key("Dell", "P2419H", None, "DP-2"), "DP-2"),
            connected(&monitor_key("BOE", "0x0BCA", None, "eDP-1"), "eDP-1"),
        ];
        dedupe_monitor_keys(&mut monitors);
        let keys: Vec<&str> = monitors.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, ["output-DP-1", "output-DP-2", "model-BOE-0x0BCA"]);
    }

    #[test]
    fn identical_monitors_sharing_a_serial_fall_back_to_their_connectors() {
        let mut monitors = vec![connected("edid-a-b-123", "DP-1"), connected("edid-a-b-123", "DP-2"), connected("edid-c", "DP-3")];
        dedupe_monitor_keys(&mut monitors);
        let keys: Vec<&str> = monitors.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, ["output-DP-1", "output-DP-2", "edid-c"]);
    }

    #[test]
    fn a_monitor_entry_wins_over_a_legacy_override_and_none_means_default() {
        let mut config = RendererConfig { same_pack_everywhere: Some(source("/default.jpg")), ..RendererConfig::default() };
        config.overrides.insert("DP-3".into(), source("/legacy.jpg"));
        let out = OutputId::new("DP-3");
        assert_eq!(resolve_monitor_assignment(&out, Some("edid-x"), &config), OutputAssignment::Explicit(source("/legacy.jpg")));

        config.monitors.insert("edid-x".into(), MonitorConfig { pack: Some(source("/mine.jpg")), ..MonitorConfig::default() });
        assert_eq!(resolve_monitor_assignment(&out, Some("edid-x"), &config), OutputAssignment::Explicit(source("/mine.jpg")));

        config.monitors.insert("edid-x".into(), MonitorConfig::default());
        assert_eq!(resolve_monitor_assignment(&out, Some("edid-x"), &config), OutputAssignment::FollowsToggle);
    }

    #[test]
    fn registering_inherits_the_legacy_override_and_survives_reconnects() {
        let mut config = RendererConfig::default();
        config.overrides.insert("DP-3".into(), source("/legacy.jpg"));
        assert!(config.register_monitors(&[connected("edid-x", "DP-3")]));
        assert_eq!(config.monitors["edid-x"].pack, Some(source("/legacy.jpg")));

        config.monitors.get_mut("edid-x").unwrap().pack = Some(source("/chosen.jpg"));
        // Unplugged: still known, reported as disconnected.
        assert_eq!(config.disconnected_monitors(&[]), vec!["edid-x".to_string()]);
        // Replugged into another port: same entry, pack kept, connector refreshed.
        assert!(config.register_monitors(&[connected("edid-x", "HDMI-A-1")]));
        assert_eq!(config.monitors["edid-x"].pack, Some(source("/chosen.jpg")));
        assert_eq!(config.monitors["edid-x"].last_connector, "HDMI-A-1");
        assert!(!config.register_monitors(&[connected("edid-x", "HDMI-A-1")]), "no-op when nothing changed");
    }

    #[test]
    fn saving_registered_monitors_keeps_other_fields_written_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let config = RendererConfig::open_at(dir.path()).unwrap();
        let mut gui = RendererConfig::load(&config);
        gui.crossfade_duration_secs = 7;
        gui.save(&config).unwrap();

        assert!(RendererConfig::save_registered_monitors(&config, &[connected("edid-x", "DP-3")]).unwrap());
        let loaded = RendererConfig::load(&config);
        assert_eq!(loaded.crossfade_duration_secs, 7);
        assert!(loaded.monitors.contains_key("edid-x"));
        assert!(!RendererConfig::save_registered_monitors(&config, &[connected("edid-x", "DP-3")]).unwrap());
    }

    #[test]
    fn overridden_output_is_unaffected_by_toggle_changes() {
        let mut config = RendererConfig { same_pack_everywhere: Some(source("/a.jpg")), ..RendererConfig::default() };
        config.overrides.insert("DP-3".to_string(), source("/override.jpg"));
        let assignment = resolve_assignment(&OutputId::new("DP-3"), &config);

        config.same_pack_everywhere = Some(source("/b.jpg")); // toggle's pack changes
        assert_eq!(effective_pack(&assignment, &config), Some(&source("/override.jpg")));
    }

    #[test]
    fn two_outputs_resolve_independently() {
        let mut config = RendererConfig::default();
        config.overrides.insert("DP-3".to_string(), source("/a.jpg"));

        let dp3 = resolve_assignment(&OutputId::new("DP-3"), &config);
        let edp1 = resolve_assignment(&OutputId::new("eDP-1"), &config);
        assert_eq!(dp3, OutputAssignment::Explicit(source("/a.jpg")));
        assert_eq!(edp1, OutputAssignment::Unassigned);
    }

    /// Regression test for a real bug found manually testing this crate against a live
    /// `wallpaperctl`-written config (see this module's doc comment): `overrides` must
    /// parse a plain-string-keyed RON map.
    #[test]
    fn overrides_parses_the_exact_shape_wallpaperctl_writes() {
        let dir = tempfile::tempdir().unwrap();
        let config = RendererConfig::open_at(dir.path()).unwrap();

        let overrides_path = dir.path().join("cosmic").join(RENDERER_CONFIG_ID).join("v1").join("overrides");
        std::fs::create_dir_all(overrides_path.parent().unwrap()).unwrap();
        std::fs::write(&overrides_path, r#"{"eDP-1": Directory("/home/user/pack")}"#).unwrap();

        let loaded = RendererConfig::load(&config);
        assert_eq!(loaded.overrides.get("eDP-1"), Some(&PackSource::Directory("/home/user/pack".into())));
    }

    #[test]
    fn renderer_config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let config = RendererConfig::open_at(dir.path()).unwrap();

        let mut state = RendererConfig::load(&config);
        assert_eq!(state, RendererConfig::default());
        assert_eq!(state.crossfade_duration_secs, 45);

        state.overrides.insert("DP-3".to_string(), PackSource::StaticFile("/x.jpg".into()));
        state.crossfade_duration_secs = 30;
        state.save(&config).unwrap();

        let reloaded = RendererConfig::load(&config);
        assert_eq!(reloaded.overrides.len(), 1);
        assert_eq!(reloaded.crossfade_duration_secs, 30);
    }

    /// Spec 011 US6 FR-023 (research.md R18) — same fix as
    /// `location_config::tests::corrupted_file_surfaces_warning_not_silent_default`,
    /// applied to `RendererConfig`.
    #[test]
    fn corrupted_file_surfaces_warning_not_silent_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = RendererConfig::open_at(dir.path()).unwrap();
        RendererConfig::default().save(&config).unwrap();

        let key_path =
            dir.path().join("cosmic").join(RENDERER_CONFIG_ID).join(format!("v{}", RendererConfig::VERSION)).join("crossfade_duration_secs");
        assert!(key_path.exists(), "expected cosmic-config's own on-disk layout at {}", key_path.display());
        std::fs::write(&key_path, b"not valid RON {{{").unwrap();

        let (loaded, corrupted) = RendererConfig::load_reporting_corruption(&config);
        assert!(corrupted);
        assert_eq!(loaded.crossfade_duration_secs, RendererConfig::default().crossfade_duration_secs);
    }

    #[test]
    fn never_configured_is_not_reported_as_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let config = RendererConfig::open_at(dir.path()).unwrap();

        let (loaded, corrupted) = RendererConfig::load_reporting_corruption(&config);
        assert!(!corrupted);
        assert_eq!(loaded, RendererConfig::default());
    }

    /// Spec 011 US7 FR-030 (research.md R25) — same fix and same rationale as
    /// `location_config::tests::save_tightens_permissions`, applied to `RendererConfig`.
    #[cfg(unix)]
    #[test]
    fn save_tightens_permissions() {
        use std::os::unix::fs::PermissionsExt;

        crate::test_support::with_scratch_xdg_config_home(|| {
            let config = RendererConfig::open().unwrap();
            RendererConfig::default().save(&config).unwrap();

            let dir = dirs::config_dir().unwrap().join("cosmic").join(RENDERER_CONFIG_ID).join(format!("v{}", RendererConfig::VERSION));
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "expected the config directory to be tightened to 0700, got {mode:o}");
        });
    }

    /// T018: closes research.md R2's own real-bug precedent (see this module's doc
    /// comment) — a `RendererConfig` value written through a handle simulating
    /// `wallpaperctl`'s write path round-trips byte-for-byte when read back through a
    /// second, independently-opened handle simulating `wallpaperd`'s load path. Now
    /// structurally guaranteed (both "simulated" call sites are the exact same type
    /// from the exact same crate), not just guarded by a per-field regression test.
    #[test]
    fn a_value_written_by_a_simulated_wallpaperctl_round_trips_through_a_simulated_wallpaperd() {
        let dir = tempfile::tempdir().unwrap();

        let wallpaperctl_handle = RendererConfig::open_at(dir.path()).unwrap();
        let mut written = RendererConfig::load(&wallpaperctl_handle);
        written.overrides.insert("DP-3".to_string(), source("/a.jpg"));
        written.same_pack_everywhere = Some(source("/b.jpg"));
        written.crossfade_duration_secs = 20;
        written.save(&wallpaperctl_handle).unwrap();

        let wallpaperd_handle = RendererConfig::open_at(dir.path()).unwrap();
        let read = RendererConfig::load(&wallpaperd_handle);
        assert_eq!(read, written);
    }

    /// New field, safe default, carried automatically by `cosmic-config`'s per-key
    /// fallback (spec 6 research.md R7's verified mechanism) — a pre-spec-7 entry with
    /// no `crossfade_duration_secs` key on disk still loads with the correct default.
    #[test]
    fn missing_crossfade_duration_key_defaults_to_45() {
        let dir = tempfile::tempdir().unwrap();
        let config = RendererConfig::open_at(dir.path()).unwrap();

        let overrides_path = dir.path().join("cosmic").join(RENDERER_CONFIG_ID).join("v1").join("overrides");
        std::fs::create_dir_all(overrides_path.parent().unwrap()).unwrap();
        std::fs::write(&overrides_path, r#"{}"#).unwrap();

        assert_eq!(RendererConfig::load(&config).crossfade_duration_secs, 45);
    }

    /// contracts/config-migration.md's 4 required test cases, applied to
    /// `RendererConfig`.
    mod migration {
        use super::*;

        fn old_config(root: &std::path::Path) -> Config {
            Config::with_custom_path(OLD_RENDERER_CONFIG_ID, RendererConfig::VERSION, root.to_path_buf()).unwrap()
        }

        fn new_config(root: &std::path::Path) -> Config {
            Config::with_custom_path(RENDERER_CONFIG_ID, RendererConfig::VERSION, root.to_path_buf()).unwrap()
        }

        #[test]
        fn migrates_real_content_and_is_idempotent_on_a_second_call() {
            let dir = tempfile::tempdir().unwrap();
            let old = old_config(dir.path());
            let mut written = RendererConfig::load(&old);
            written.same_pack_everywhere = Some(source("/old.jpg"));
            written.save(&old).unwrap();

            let new = new_config(dir.path());
            let migrated = RendererConfig::migrate_core(&new, Ok(old_config(dir.path())));
            assert_eq!(migrated.same_pack_everywhere, Some(source("/old.jpg")));
            assert_eq!(RendererConfig::load(&new).same_pack_everywhere, Some(source("/old.jpg")));

            // Idempotent: a second call sees the new store already populated.
            let migrated_again = RendererConfig::migrate_core(&new, Ok(old_config(dir.path())));
            assert_eq!(migrated_again, migrated);
        }

        /// `cosmic_config::Config::new`/`with_custom_path` create their directory on
        /// open rather than failing when it doesn't exist yet, so opening the old
        /// store essentially always succeeds even on a genuinely fresh install — the
        /// real "nothing to migrate" signal is `load()` returning `Default`, covered
        /// by the sibling test below. This test instead covers the `Err` branch itself
        /// (a real open failure, e.g. an invalid application id) to prove it degrades
        /// to the same no-op rather than panicking.
        #[test]
        fn a_failed_old_config_open_is_a_silent_no_op() {
            let dir = tempfile::tempdir().unwrap();
            let new = new_config(dir.path());
            let invalid_open = Config::with_custom_path("../invalid", RendererConfig::VERSION, dir.path().to_path_buf());
            assert!(invalid_open.is_err(), "fixture assumption: this name is rejected by cosmic-config");

            let result = RendererConfig::migrate_core(&new, invalid_open);
            assert_eq!(result, RendererConfig::default());
        }

        #[test]
        fn old_store_exists_but_was_never_configured_stays_default() {
            let dir = tempfile::tempdir().unwrap();
            let _ = old_config(dir.path()); // opened, but nothing ever written to it
            let new = new_config(dir.path());

            let result = RendererConfig::migrate_core(&new, Ok(old_config(dir.path())));
            assert_eq!(result, RendererConfig::default());
            assert_eq!(RendererConfig::load(&new), RendererConfig::default());
        }

        #[test]
        fn new_store_already_populated_never_consults_the_old_one() {
            let dir = tempfile::tempdir().unwrap();
            let old = old_config(dir.path());
            let mut old_entry = RendererConfig::load(&old);
            old_entry.same_pack_everywhere = Some(source("/old.jpg"));
            old_entry.save(&old).unwrap();

            let new = new_config(dir.path());
            let mut new_entry = RendererConfig::load(&new);
            new_entry.same_pack_everywhere = Some(source("/already-configured.jpg"));
            new_entry.save(&new).unwrap();

            let result = RendererConfig::migrate_core(&new, Ok(old_config(dir.path())));
            assert_eq!(result.same_pack_everywhere, Some(source("/already-configured.jpg")));
        }
    }
}
