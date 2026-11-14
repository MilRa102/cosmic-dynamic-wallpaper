//! Monitors page — which pack each *physical monitor* shows. Monitors are identified
//! by `wallpaper_ipc::monitor_key` (EDID make/model/serial, make/model without a
//! serial, the output name only without EDID data), so a monitor keeps its pack when
//! moved to another port. A pack is
//! never tied to a monitor: any pack can be picked for any number of monitors, and a
//! monitor with no explicit choice (or never seen before) shows the default pack.
//!
//! Monitors that have a stored setting but aren't connected right now are listed as
//! disabled: their setting is kept and restored on reconnect until the user forgets
//! it. Writes the same `wallpaper_ipc::RendererConfig` the daemon and `wallpaperctl`
//! read.

use cosmic::widget;
use cosmic::Element;
use pack_loader::PackSource;
use wallpaper_ipc::{ConnectedMonitor, RendererConfig};

use crate::i18n::{tr, trf};
use crate::pack_display;

/// Sets the default pack (`None` = no default: monitors without their own pack stay
/// empty).
pub fn set_default_pack(config: &mut RendererConfig, source: Option<PackSource>) {
    config.same_pack_everywhere = source;
}

/// Sets `key`'s own pack (`None` = follow the default pack). Creates the monitor's
/// entry if the daemon hasn't registered it yet; `monitor` refreshes its label.
pub fn set_monitor_pack(config: &mut RendererConfig, key: &str, monitor: Option<&ConnectedMonitor>, source: Option<PackSource>) {
    let entry = config.monitors.entry(key.to_string()).or_default();
    entry.pack = source;
    if let Some(monitor) = monitor {
        entry.label.clone_from(&monitor.label);
        entry.last_connector.clone_from(&monitor.connector);
    }
}

/// For a daemon too old to know monitors (no `ListMonitors`): the per-connector
/// override it does read — which the new daemon also inherits when it first
/// registers the monitor, so the choice survives the upgrade.
pub fn set_legacy_override(config: &mut RendererConfig, connector: &str, source: Option<PackSource>) {
    match source {
        Some(source) => {
            config.overrides.insert(connector.to_string(), source);
        }
        None => {
            config.overrides.remove(connector);
        }
    }
}

/// Drops a disconnected monitor's stored setting. Its legacy per-connector override,
/// if any, goes too — otherwise it would be re-inherited on the next connect.
pub fn forget_monitor(config: &mut RendererConfig, key: &str) {
    if let Some(entry) = config.monitors.remove(key) {
        config.overrides.remove(&entry.last_connector);
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    /// Index into the default-pack dropdown: 0 = none, `n` = `available_packs[n - 1]`.
    DefaultPackSelected(usize),
    /// `(monitor_key, index)`: 0 = the default pack, `n` = `available_packs[n - 1]`.
    MonitorPackSelected(String, usize),
    ForgetMonitor(String),
}

pub struct State {
    /// Monitors the daemon drives right now; `None` when it isn't reachable.
    pub connected: Option<Vec<ConnectedMonitor>>,
    /// The daemon doesn't support `ListMonitors`: monitors are its connectors and
    /// choices go to the legacy per-connector overrides.
    pub legacy_daemon: bool,
    pub available_packs: Vec<PackSource>,
    /// Same order/index as `available_packs`: each pack's display-name override.
    pub available_pack_display_names: Vec<Option<String>>,
    pub current_config: RendererConfig,
}

impl State {
    /// The pack chosen by dropdown `index` where 0 means "none/default".
    pub fn pack_at(&self, index: usize) -> Option<PackSource> {
        index.checked_sub(1).and_then(|i| self.available_packs.get(i).cloned())
    }

    pub fn connected_monitor(&self, key: &str) -> Option<&ConnectedMonitor> {
        self.connected.as_ref()?.iter().find(|m| m.key == key)
    }

    fn selected_index(&self, source: Option<&PackSource>) -> usize {
        source.and_then(|s| self.available_packs.iter().position(|p| p == s)).map_or(0, |i| i + 1)
    }
}

pub fn view(state: &State) -> Element<'_, Message> {
    let mut column = widget::column::with_capacity(3).spacing(cosmic::theme::spacing().space_m);

    if state.available_packs.is_empty() {
        let section = widget::settings::section()
            .title(tr("Monitors"))
            .add(widget::text::body(tr("No packs registered yet — add one on the Packs page first.")));
        return widget::scrollable(column.push(section)).into();
    }

    let pack_labels: Vec<String> = state
        .available_packs
        .iter()
        .zip(state.available_pack_display_names.iter())
        .map(|(s, override_name)| {
            pack_display::resolve_pack_display_name(s, override_name.as_deref()).unwrap_or_else(|| tr("(unnamed pack)"))
        })
        .collect();
    let with_first = |first: String| -> Vec<String> { std::iter::once(first).chain(pack_labels.iter().cloned()).collect() };

    let default_choices = with_first(tr("None"));
    let default_dropdown =
        widget::dropdown(default_choices, Some(state.selected_index(state.current_config.same_pack_everywhere.as_ref())), Message::DefaultPackSelected);
    column = column.push(
        widget::settings::section().title(tr("Default pack")).add(
            widget::settings::item::builder(tr("Default pack"))
                .description(tr("Shown on new monitors and on monitors set to “Default pack”."))
                .control(default_dropdown),
        ),
    );

    let monitor_choices = with_first(tr("Default pack"));
    let monitor_dropdown = |key: &str| {
        let current = if state.legacy_daemon {
            state.connected_monitor(key).and_then(|m| state.current_config.overrides.get(&m.connector))
        } else {
            state.current_config.monitors.get(key).and_then(|m| m.pack.as_ref())
        };
        let selected = state.selected_index(current);
        let key = key.to_string();
        widget::dropdown(monitor_choices.clone(), Some(selected), move |index| Message::MonitorPackSelected(key.clone(), index))
    };

    let mut connected_section = widget::settings::section().title(tr("Connected monitors"));
    match &state.connected {
        None => {
            connected_section =
                connected_section.add(widget::text::body(tr("wallpaperd is not running — connected monitors can't be detected.")));
        }
        Some(monitors) if monitors.is_empty() => {
            connected_section = connected_section.add(widget::text::body(tr("No monitors detected.")));
        }
        Some(monitors) => {
            for monitor in monitors {
                connected_section = connected_section.add(
                    widget::settings::item::builder(monitor.label.clone())
                        .description(monitor.connector.clone())
                        .control(monitor_dropdown(&monitor.key)),
                );
            }
        }
    }
    column = column.push(connected_section);

    let connected = state.connected.as_deref().unwrap_or_default();
    let disabled = state.current_config.disconnected_monitors(connected);
    if state.connected.is_some() && !state.legacy_daemon && !disabled.is_empty() {
        let mut section = widget::settings::section().title(tr("Disconnected monitors"));
        for key in disabled {
            let entry = &state.current_config.monitors[&key];
            let name = if entry.label.is_empty() { key.clone() } else { entry.label.clone() };
            let description = if entry.last_connector.is_empty() {
                tr("Not connected")
            } else {
                trf("Not connected · last seen on {connector}", &[("connector", entry.last_connector.as_str())])
            };
            let controls = widget::row::with_capacity(2)
                .spacing(cosmic::theme::spacing().space_xs)
                .align_y(cosmic::iced::Alignment::Center)
                .push(monitor_dropdown(&key))
                .push(widget::button::destructive(tr("Forget")).on_press(Message::ForgetMonitor(key.clone())));
            section = section.add(widget::settings::item::builder(name).description(description).control(controls));
        }
        column = column.push(section);
    }

    widget::scrollable(column).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(path: &str) -> PackSource {
        PackSource::StaticFile(path.into())
    }

    fn monitor(key: &str, connector: &str) -> ConnectedMonitor {
        ConnectedMonitor { key: key.into(), connector: connector.into(), label: "Dell U2720Q".into() }
    }

    #[test]
    fn choosing_a_pack_creates_the_monitor_entry_and_default_clears_only_the_pack() {
        let mut config = RendererConfig::default();
        set_monitor_pack(&mut config, "edid-x", Some(&monitor("edid-x", "DP-3")), Some(source("/a.jpg")));
        assert_eq!(config.monitors["edid-x"].pack, Some(source("/a.jpg")));
        assert_eq!(config.monitors["edid-x"].last_connector, "DP-3");

        set_monitor_pack(&mut config, "edid-x", None, None);
        assert_eq!(config.monitors["edid-x"].pack, None);
        assert_eq!(config.monitors["edid-x"].label, "Dell U2720Q", "entry kept, so it stays known");
    }

    #[test]
    fn forgetting_drops_the_entry_and_its_legacy_override() {
        let mut config = RendererConfig::default();
        config.overrides.insert("DP-3".into(), source("/legacy.jpg"));
        config.overrides.insert("DP-4".into(), source("/other.jpg"));
        set_monitor_pack(&mut config, "edid-x", Some(&monitor("edid-x", "DP-3")), Some(source("/a.jpg")));

        forget_monitor(&mut config, "edid-x");
        assert!(config.monitors.is_empty());
        assert!(!config.overrides.contains_key("DP-3"));
        assert!(config.overrides.contains_key("DP-4"));
    }

    #[test]
    fn legacy_override_is_inherited_when_the_new_daemon_registers_the_monitor() {
        let mut config = RendererConfig::default();
        set_legacy_override(&mut config, "DP-3", Some(source("/a.jpg")));
        config.register_monitors(&[monitor("edid-x", "DP-3")]);
        assert_eq!(config.monitors["edid-x"].pack, Some(source("/a.jpg")));

        set_legacy_override(&mut config, "DP-3", None);
        assert!(config.overrides.is_empty());
    }

    #[test]
    fn dropdown_index_zero_means_none_or_default() {
        let state = State {
            connected: Some(Vec::new()),
            legacy_daemon: false,
            available_packs: vec![source("/a.jpg"), source("/b.jpg")],
            available_pack_display_names: vec![None, None],
            current_config: RendererConfig::default(),
        };
        assert_eq!(state.pack_at(0), None);
        assert_eq!(state.pack_at(2), Some(source("/b.jpg")));
        assert_eq!(state.pack_at(3), None);
        assert_eq!(state.selected_index(Some(&source("/b.jpg"))), 2);
        assert_eq!(state.selected_index(None), 0);
    }
}
