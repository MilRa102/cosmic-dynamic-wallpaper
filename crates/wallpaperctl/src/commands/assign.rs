//! `wallpaperctl assign --output <id> <pack>` / `assign --same-everywhere <pack>`
//! (FR-006, FR-007). `<id>` is either a monitor key (`edid-…`/`model-…`/`output-…`, see
//! `wallpaper_ipc::monitor_key`) or a connector name (`DP-3`).

use std::path::Path;

use cosmic_config::Config;
use pack_loader::Registry;

use crate::config::RendererConfig;
use wallpaper_ipc::DbusClient;
use crate::error::CliError;
use crate::output::{self, Ack};
use crate::pack_ref::find_registered;

/// What an assignment targets — exactly one of a specific output or the "same pack
/// everywhere" toggle (data-model.md `OutputAssignmentRequest.target`).
pub enum AssignTarget {
    Output(String),
    SameEverywhere,
}

/// `registry`/`renderer_config` are already-open handles (see `register.rs`'s doc
/// comment for why) — `main.rs` passes the real ones, tests pass tempdir-backed ones.
pub fn run(
    target: AssignTarget,
    pack_path: &Path,
    registry: &Registry,
    renderer_config: &Config,
    json: bool,
) -> Result<String, CliError> {
    // FR-007: checked against the local registry — never requires a daemon.
    let source = find_registered(registry, pack_path)
        .ok_or_else(|| CliError::PackNotFound { source: pack_path.to_path_buf() })?;

    let connected = DbusClient::connect().and_then(|client| client.list_monitors()).ok();
    let mut state = RendererConfig::load(renderer_config);
    let target_desc = match &target {
        AssignTarget::Output(id) => {
            assign_output(&mut state, id, &source, connected.as_deref());
            format!("output {id:?}")
        }
        AssignTarget::SameEverywhere => {
            state.same_pack_everywhere = Some(source.clone());
            "all outputs (same-everywhere)".to_string()
        }
    };
    state.save(renderer_config)?;

    // FR-007: a non-fatal warning only, and only if the daemon happens to be
    // reachable — assigning to a not-yet-connected output name is a legitimate
    // "configure ahead of time" case, never a failure.
    if let AssignTarget::Output(id) = &target {
        if let Ok(client) = DbusClient::connect() {
            if let Ok(entries) = client.query_all() {
                if !entries.iter().any(|e| &e.output == id) && !connected.iter().flatten().any(|m| &m.key == id) {
                    eprintln!(
                        "warning: output {id:?} is not currently connected — the assignment is saved and will apply once it connects"
                    );
                }
            }
        }
    }

    Ok(output::render(json, &Ack::ok(), || format!("assigned to {target_desc}")))
}

/// `id` naming a known monitor key sets that monitor's pack. A connector name writes
/// the legacy per-connector override (applies to monitors seen there for the first
/// time) and also updates the monitor on that connector — the connected one when the
/// daemon tells us, else every stored monitor last seen there — since a stored
/// monitor entry takes precedence over the override.
fn assign_output(state: &mut RendererConfig, id: &str, source: &pack_loader::PackSource, connected: Option<&[wallpaper_ipc::ConnectedMonitor]>) {
    if let Some(monitor) = state.monitors.get_mut(id) {
        monitor.pack = Some(source.clone());
        return;
    }
    if ["edid-", "model-", "output-"].iter().any(|prefix| id.starts_with(prefix)) {
        state.monitors.insert(id.to_string(), wallpaper_ipc::MonitorConfig { pack: Some(source.clone()), ..Default::default() });
        return;
    }
    state.overrides.insert(id.to_string(), source.clone());
    for (key, monitor) in &mut state.monitors {
        let on_connector = match connected {
            Some(connected) => connected.iter().any(|m| &m.key == key && m.connector == id),
            None => monitor.last_connector == id,
        };
        if on_connector {
            monitor.pack = Some(source.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Registry, Config, tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let registry_dir = dir.path().join("registry");
        std::fs::create_dir_all(&registry_dir).unwrap();
        let renderer_dir = dir.path().join("renderer");
        std::fs::create_dir_all(&renderer_dir).unwrap();

        let mut registry = Registry::open_at(&registry_dir).unwrap();
        let pack_file = dir.path().join("pack.png");
        image::RgbImage::new(2, 2).save(&pack_file).unwrap();
        let source = pack_loader::PackSource::resolve(&pack_file).unwrap();
        registry.register(source).unwrap();

        let renderer_config = RendererConfig::open_at(&renderer_dir).unwrap();
        (registry, renderer_config, dir, pack_file)
    }

    /// Scenario 1 & 4: assigning a registered pack to an output writes
    /// `RendererConfig.overrides`, including a not-currently-connected name (no daemon
    /// running in this test, so the "configure ahead of time" case applies).
    #[test]
    fn assigns_a_registered_pack_to_an_output() {
        let (registry, renderer_config, _dir, pack_file) = setup();

        let result = run(
            AssignTarget::Output("DP-3".to_string()),
            &pack_file,
            &registry,
            &renderer_config,
            false,
        );
        assert!(result.is_ok());

        let state = RendererConfig::load(&renderer_config);
        assert!(state.overrides.contains_key("DP-3"));
    }

    /// Scenario 2: "same pack everywhere" sets `same_pack_everywhere`.
    #[test]
    fn same_everywhere_sets_the_toggle() {
        let (registry, renderer_config, _dir, pack_file) = setup();

        run(AssignTarget::SameEverywhere, &pack_file, &registry, &renderer_config, false).unwrap();

        let state = RendererConfig::load(&renderer_config);
        assert!(state.same_pack_everywhere.is_some());
    }

    /// Scenario 3: assigning a different pack to an already-assigned output replaces
    /// the old value.
    #[test]
    fn reassigning_an_output_replaces_the_old_pack() {
        let (registry, renderer_config, dir, pack_file) = setup();
        run(
            AssignTarget::Output("DP-3".to_string()),
            &pack_file,
            &registry,
            &renderer_config,
            false,
        )
        .unwrap();

        let mut registry = registry;
        let second_file = dir.path().join("second.png");
        image::RgbImage::new(2, 2).save(&second_file).unwrap();
        let second_source = pack_loader::PackSource::resolve(&second_file).unwrap();
        registry.register(second_source.clone()).unwrap();

        run(
            AssignTarget::Output("DP-3".to_string()),
            &second_file,
            &registry,
            &renderer_config,
            false,
        )
        .unwrap();

        let state = RendererConfig::load(&renderer_config);
        assert_eq!(state.overrides.get("DP-3"), Some(&second_source));
    }

    /// Scenario 5: assigning an unregistered pack fails clearly with no write.
    #[test]
    fn assigning_an_unregistered_pack_fails() {
        let (registry, renderer_config, dir, _pack_file) = setup();
        let unregistered = dir.path().join("never-registered.png");
        image::RgbImage::new(2, 2).save(&unregistered).unwrap();

        let result = run(
            AssignTarget::Output("DP-3".to_string()),
            &unregistered,
            &registry,
            &renderer_config,
            false,
        );
        assert!(matches!(result, Err(CliError::PackNotFound { .. })));
        assert!(RendererConfig::load(&renderer_config).overrides.is_empty());
    }

    #[test]
    fn connector_assignment_also_updates_the_registered_monitor_on_it() {
        let a = pack_loader::PackSource::StaticFile("/a.jpg".into());
        let mut state = RendererConfig::default();
        state.monitors.insert(
            "edid-x".into(),
            wallpaper_ipc::MonitorConfig { pack: None, label: String::new(), last_connector: "DP-3".into() },
        );
        assign_output(&mut state, "DP-3", &a, None);
        assert_eq!(state.monitors["edid-x"].pack, Some(a.clone()));
        assert_eq!(state.overrides.get("DP-3"), Some(&a));

        let b = pack_loader::PackSource::StaticFile("/b.jpg".into());
        assign_output(&mut state, "edid-x", &b, None);
        assert_eq!(state.monitors["edid-x"].pack, Some(b));
        assert!(!state.overrides.contains_key("edid-x"));
    }

    #[test]
    fn unknown_monitor_key_is_registered_for_future_connection() {
        let mut state = RendererConfig::default();
        let source = pack_loader::PackSource::StaticFile("/a.jpg".into());
        assign_output(&mut state, "edid-Dell-U2720Q-ABC123", &source, None);
        assert_eq!(state.monitors["edid-Dell-U2720Q-ABC123"].pack, Some(source.clone()));
        assert!(!state.overrides.contains_key("edid-Dell-U2720Q-ABC123"));
        state.register_monitors(&[wallpaper_ipc::ConnectedMonitor {
            key: "edid-Dell-U2720Q-ABC123".into(),
            connector: "DP-3".into(),
            label: "Dell U2720Q".into(),
        }]);
        assert_eq!(state.monitors["edid-Dell-U2720Q-ABC123"].pack, Some(source));
    }
}
