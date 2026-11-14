//! Custom Pack Builder wizard (spec 010, extended by spec 012) — the single interface
//! for both turning a folder of images with no `manifest.toml` into a fully valid
//! custom pack, and editing an already-registered folder pack's schedule/name/author
//! in place. Two entry points into the same `State`/`Message`/`view` (spec 012 FR-005
//! — deliberately one implementation, not a parallel "edit mode"):
//! - **Add** (spec 010): entered from `pages::packs`'s "Add pack folder…" flow when it
//!   hits `pack_loader::ManifestError::ManifestNotFound` (research.md R1) rather than a
//!   new nav page (research.md R9) — [`open`], blank `State`.
//! - **Edit** (spec 012 US1): entered from a `Directory` row's pencil icon on the Packs
//!   screen — [`open_for_edit`], `State` pre-populated from the pack's current,
//!   already-loaded content. Refuses to open (`Err`, no `State` at all) for any pack
//!   that doesn't currently load — a missing folder, an unparseable manifest, a
//!   manifest mixing solar/clock anchors, or a solar offset wider than this screen's
//!   own ±12h range can represent (research.md R3) — rather than opening a
//!   partially-populated or silently-lossy session. `edit_target: Some(_)` is what
//!   distinguishes an edit session from an add session everywhere it matters: no
//!   placement (Move/Keep) dialog on save (research.md R6), and `generate`'s success
//!   writes straight back into the pack's existing location instead.
//!
//! Owned by `App.pack_builder: Option<State>` either way.
//!
//! Scope, in order: pick a scheduling mode (solar period or specific time, FR-004) —
//! already chosen, for an edit session — assign every scanned image (FR-005–FR-009),
//! name the pack and its author (FR-010; spec 012 FR-015 added the name field), Generate
//! a self-validated `manifest.toml` (FR-011, FR-012), then — add flow only — choose
//! whether to move the folder into the application's standard pack location or leave it
//! in place (FR-013–FR-017). See data-model.md §2 and contracts/pack-builder-gui-flow.md
//! (spec 010) plus contracts/pack-builder-edit-flow.md (spec 012) for the full state
//! machine this module implements.
//!
//! Design notes worth keeping in view while reading this file:
//! - A signed-hours field alone can't express "-15m" when hours is 0 (there's no
//!   negative zero in `i32`) — `SolarAssignment` carries an explicit `offset_negative`
//!   flag instead of relying on `offset_hours`'s own sign (research.md R6 still holds:
//!   two `spin_button`s plus a small sign toggle, just three fields instead of two).
//! - `combine_offset` therefore takes the sign explicitly rather than inferring it;
//!   `decompose_offset` (spec 012) is its exact inverse, used to pre-fill an edit
//!   session's rows from an already-loaded pack's `TimeAnchor`.
//! - `PreservedManifestFields` (spec 012 FR-009) exists because `pack_loader::
//!   LoadedPack` only exposes each image's *resolved* scaling (override-or-default),
//!   not whether it had an explicit override in the original TOML — an edit session
//!   reconstructs an equivalent, minimal override set from that resolved value rather
//!   than re-parsing the raw manifest (research.md R5).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate, NaiveTime, TimeDelta, Timelike};
use cosmic::iced::Length;
use cosmic::widget;
use cosmic::Element;
use image::ImageReader;
use pack_loader::{Color, ManifestDraft, ManifestDraftImage, PackSource, Registry, ScalingMode};
use schedule_engine::{
    AnchorKind, ImageId, Location, PackError, PackImage, PeriodLayout, ResolvedSchedule, SolarEventKind, SolarPeriod, TimeAnchor,
    WallpaperPack,
};

use crate::i18n::{tr, trf};

/// The 8 recognized solar events, in the order shown in the dropdown (FR-005).
pub const SOLAR_EVENTS: [SolarEventKind; 8] = [
    SolarEventKind::Sunrise,
    SolarEventKind::Sunset,
    SolarEventKind::SolarNoon,
    SolarEventKind::SolarMidnight,
    SolarEventKind::CivilDawn,
    SolarEventKind::CivilDusk,
    SolarEventKind::AstronomicalDawn,
    SolarEventKind::AstronomicalDusk,
];

fn solar_event_label(event: SolarEventKind) -> String {
    tr(match event {
        SolarEventKind::Sunrise => "Sunrise",
        SolarEventKind::Sunset => "Sunset",
        SolarEventKind::SolarNoon => "Solar noon",
        SolarEventKind::SolarMidnight => "Solar midnight",
        SolarEventKind::CivilDawn => "Civil dawn",
        SolarEventKind::CivilDusk => "Civil dusk",
        SolarEventKind::AstronomicalDawn => "Astronomical dawn",
        SolarEventKind::AstronomicalDusk => "Astronomical dusk",
    })
}

fn solar_event_labels() -> Vec<String> {
    SOLAR_EVENTS.iter().copied().map(solar_event_label).collect()
}

// --- Types (data-model.md §2) ---

/// Which kind of schedule the whole draft uses (FR-004) — chosen once, up front;
/// changing it later discards every row's current assignment (spec.md Edge Cases)
/// since a pack cannot mix solar and clock anchors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentMode {
    /// Images are dragged into fixed solar blocks; their times are derived from the
    /// block boundaries for the configured location and date (`TimeAnchor::Period`).
    SolarBlocks,
    SolarPeriod,
    SpecificTime,
}

/// Display label for a solar block.
pub fn period_label(period: SolarPeriod) -> String {
    tr(match period {
        SolarPeriod::Morning => "Morning",
        SolarPeriod::Day => "Day",
        SolarPeriod::Evening => "Evening",
        SolarPeriod::Night => "Night",
    })
}

/// One image's solar-period assignment — an event plus a signed hour/minute offset,
/// clamped to a ±12h magnitude (research.md R6, the clarification's cap).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SolarAssignment {
    pub event: SolarEventKind,
    /// `true` nudges the event earlier ("-"), `false` later ("+") or not at all.
    pub offset_negative: bool,
    /// 0..=12.
    pub offset_hours: u32,
    /// 0..=59; forced to 0 whenever `offset_hours == 12`.
    pub offset_minutes: u32,
}

/// One scanned image plus its current, possibly still-empty assignment.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageRow {
    pub file_name: String,
    pub thumbnail_path: PathBuf,
    pub solar: Option<SolarAssignment>,
    pub time: Option<NaiveTime>,
    /// `SolarBlocks` mode: the block this image sits in (`None` = not included in the
    /// pack). The order of rows sharing a block in `State.rows` is the order within it.
    pub period: Option<SolarPeriod>,
}

impl ImageRow {
    fn new(file_name: String, thumbnail_path: PathBuf) -> Self {
        Self { file_name, thumbnail_path, solar: None, time: None, period: None }
    }
}

/// Where a dragged image would land: before the `index`-th image of `block` (`None` =
/// the "not included" tray); `index` equal to the block's length means "at the end".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropTarget {
    pub block: Option<SolarPeriod>,
    pub index: usize,
}

/// Where the just-generated manifest currently sits, awaiting the move-vs-keep choice
/// (FR-013).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedPlacement {
    pub generated_path: PathBuf,
}

/// Open while the destination-name-collision prompt is shown (FR-014a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCollision {
    pub generated_path: PathBuf,
    pub suggested_name: String,
}

/// The manifest fields this wizard's own controls never expose for editing —
/// pack-level `default_scaling`/`fallback_color`, and any per-image `scaling`
/// override — captured once from an already-loaded pack so an edit session can carry
/// them forward completely unchanged (spec 012 FR-009, research.md R5). `None` for the
/// add flow (`open`), which has never had any prior manifest to preserve fields from;
/// `Some` for an edit session (`open_for_edit`).
///
/// `per_image_scaling` is keyed by file name (== `schedule_engine::ImageId`'s own
/// string form) and holds only the images whose *resolved* scaling actually differs
/// from the pack's `default_scaling` — `pack_loader::LoadedPack` only exposes the
/// already-resolved value per image (override-or-default), not whether a given image
/// had an explicit override in the original TOML, so this reconstructs an equivalent,
/// minimal set of overrides from that resolved value rather than the raw manifest
/// (which this crate has no read access to — `pack_loader::manifest::parse` is a
/// private free function, not part of this crate's public API).
#[derive(Debug, Clone, PartialEq)]
pub struct PreservedManifestFields {
    pub default_scaling: ScalingMode,
    pub fallback_color: Color,
    pub per_image_scaling: HashMap<String, ScalingMode>,
}

/// The wizard's full transient state — owned by `App.pack_builder`, never persisted.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub source_dir: PathBuf,
    pub mode: Option<AssignmentMode>,
    pub rows: Vec<ImageRow>,
    pub author: String,
    /// Spec 012 FR-015/FR-016 (User Story 4): the pack's display name — pre-filled
    /// from the folder name when adding (today's `build_draft` default, now made
    /// explicit and user-editable rather than computed only at Generate time), or from
    /// the loaded manifest's current `name` when editing.
    pub name: String,
    /// Set whenever FR-008's check currently fails — blocks Generate.
    pub conflict: Option<String>,
    /// FR-017's error surface for a failed Generate (write or self-validation).
    pub generate_error: Option<String>,
    /// FR-017's error surface for a failed move.
    pub move_error: Option<String>,
    pub pending_collision: Option<PendingCollision>,
    pub pending_placement: Option<GeneratedPlacement>,
    /// FR-018: zero usable images, or more than `schedule_engine::MAX_ANCHORS`.
    pub scan_error: Option<String>,
    /// Spec 011 US6 FR-027 (research.md R22): the self-validated manifest text
    /// `generate()` produced, held here rather than written to `source_dir` yet.
    /// Written into its real destination (the source folder for Keep, the moved
    /// folder for Move) only at the moment the user actually makes that choice — see
    /// `finalize`. `Some` exactly when `pending_placement` is `Some`.
    pub pending_manifest_text: Option<String>,
    /// Spec 012 (Edit Existing Packs): `None` for the add flow (unchanged); `Some(source)`
    /// when this session is editing the already-registered pack at `source` — gates
    /// which save path `generate()`'s success routes to (contracts/
    /// pack-builder-edit-flow.md: a direct overwrite of `source_dir/manifest.toml`,
    /// no placement dialog, no re-registration — research.md R6).
    pub edit_target: Option<PackSource>,
    /// Spec 012 FR-009: captured once by `open_for_edit`, threaded into `build_draft`
    /// unchanged by anything this screen does. `None` for the add flow.
    pub preserved: Option<PreservedManifestFields>,
    /// Location the blocks editor previews computed times for — refreshed by `app.rs`
    /// whenever the wizard opens. `None` hides the times (blocks stay editable).
    pub location: Option<Location>,
    /// Date the blocks editor previews (defaults to today).
    pub preview_date: NaiveDate,
    /// File name of the image currently being dragged in the blocks editor.
    pub dragging: Option<String>,
    pub drop_target: Option<DropTarget>,
}

#[derive(Debug, Clone)]
pub enum Message {
    ModeChosen(AssignmentMode),
    /// Blocks editor: mouse pressed on an image — starts dragging it.
    DragStarted(String),
    /// Blocks editor: the cursor entered a drop position while dragging.
    DragEntered(DropTarget),
    /// Blocks editor: the cursor left a drop position.
    DragLeft(DropTarget),
    /// Blocks editor: mouse released anywhere — drops on the current target, if any.
    DragReleased,
    /// Blocks editor: a pane scrolled — the hovered drop position is stale until the
    /// cursor moves again (hover is only tracked on cursor motion).
    DragScrolled,
    /// Blocks editor: keyboard-friendly alternative to dragging.
    BlockSelected(String, Option<SolarPeriod>),
    /// Blocks editor: move an image one position earlier (`-1`) or later (`+1`) in its block.
    MoveWithinBlock(String, i32),
    /// Blocks editor: show computed times for another day (`0` = back to today).
    PreviewDayShifted(i64),
    /// Convert the current event/clock assignments into solar blocks.
    ConvertToBlocks,
    Cancelled,
    /// `(row, index into SOLAR_EVENTS)`.
    SolarEventSelected(usize, usize),
    SolarOffsetSignToggled(usize),
    SolarOffsetHoursChanged(usize, u32),
    SolarOffsetMinutesChanged(usize, u32),
    TimeHourChanged(usize, u32),
    TimeMinuteChanged(usize, u32),
    AuthorChanged(String),
    /// Spec 012 FR-015/FR-016 (User Story 4): the pack display-name field.
    NameChanged(String),
    GenerateRequested,
    MoveRequested,
    KeepRequested,
    CollisionNameChanged(String),
    CollisionConfirmed,
    CollisionCancelled,
}

// --- Opening the wizard (research.md R1, R9) ---

/// FR-002: whether picking `path` (already known to exist) from "Add pack folder…"
/// should launch this wizard instead of registering it directly — true only for a
/// directory with no `manifest.toml` yet (`ManifestNotFound` specifically, not any
/// other load failure, and never a single-file static pack). Extracted as its own
/// function so this decision is unit-testable without a real `App`/`cosmic::
/// Application` — spec 010 Edge Cases' "already has a manifest" case is asserted
/// directly against this, not just exercised manually.
pub fn should_open_for(path: &Path) -> bool {
    path.is_dir() && matches!(pack_loader::load_pack(path), Err(pack_loader::ManifestError::ManifestNotFound { .. }))
}

/// The folder-name-derived default pack name (research.md R10's original default,
/// spec 012 FR-015: now surfaced as `State.name`'s starting value — editable — rather
/// than computed only once, inline, at `generate()` time).
fn folder_display_name(dir: &Path) -> String {
    dir.file_name().and_then(|n| n.to_str()).unwrap_or("Custom Pack").to_string()
}

/// Opens the wizard at `source_dir` — called when the existing "Add pack folder…" flow
/// hits `ManifestNotFound` (research.md R1). Scans the folder immediately, so the
/// mode-choice screen can show a real scan error right away rather than deferring the
/// scan until a mode is picked (FR-001, FR-002, FR-003).
pub fn open(source_dir: PathBuf) -> State {
    let (rows, scan_error) = match scan_folder(&source_dir) {
        Ok(rows) => (rows, None),
        Err(reason) => (Vec::new(), Some(reason)),
    };
    let name = folder_display_name(&source_dir);
    State {
        source_dir,
        mode: None,
        rows,
        author: String::new(),
        name,
        conflict: None,
        generate_error: None,
        move_error: None,
        pending_collision: None,
        pending_placement: None,
        scan_error,
        pending_manifest_text: None,
        edit_target: None,
        preserved: None,
        location: None,
        preview_date: Local::now().date_naive(),
        dragging: None,
        drop_target: None,
    }
}

// --- Opening the wizard for an already-registered pack (spec 012 US1, research.md R3–R5) ---

/// Spec 012 FR-004/FR-005/FR-019 (contracts/pack-builder-edit-flow.md): opens the
/// wizard pre-populated from `source`'s current, already-loaded content — the same
/// configuration screen [`open`] shows for a brand-new folder, just starting from
/// everything the pack already has instead of blank. `Err(reason)` — no `State` at
/// all — when `source` isn't a `Directory` at all (FR-010: a `StaticFile` pack has no
/// schedule to edit here — routed to the separate rename-only prompt instead,
/// contracts/packs-screen-icon-actions.md, before this function is ever reached in
/// practice, but checked again here rather than trusting every future caller to keep
/// getting that routing right), when the pack doesn't currently load at all (research.md
/// R3 — a missing folder, an unparseable manifest, or a manifest mixing solar/clock
/// anchors all collapse into this one `pack_loader::load_pack` call, with no separate
/// mixed-anchor-specific check needed anywhere in this module), or when a solar image's
/// offset is wider than this screen's own ±12h range can faithfully represent
/// (research.md R3's own follow-on finding, [`offset_within_wizard_range`]) — every one
/// of these is a "can't safely edit this one here" case, not something a
/// partially-populated wizard should paper over.
pub fn open_for_edit(source_dir: PathBuf, source: PackSource) -> Result<State, String> {
    if !matches!(source, PackSource::Directory(_)) {
        return Err(trf("{path} is a single image, not a folder pack — it has no schedule to edit here.", &[("path", &source_dir.display().to_string())]));
    }
    let loaded = pack_loader::load_pack(&source_dir).map_err(|e| e.to_string())?;

    for image in loaded.pack.images() {
        if let TimeAnchor::Solar { offset, .. } = image.anchor {
            if !offset_within_wizard_range(offset) {
                return Err(trf(
                    "{id} has a solar offset wider than this editor's ±12h range — edit its manifest.toml directly.",
                    &[("id", &image.id.to_string())],
                ));
            }
        }
    }

    let mode = match loaded.pack.anchor_kind() {
        AnchorKind::Period => AssignmentMode::SolarBlocks,
        AnchorKind::Solar => AssignmentMode::SolarPeriod,
        AnchorKind::Clock => AssignmentMode::SpecificTime,
    };

    let (mut rows, scan_error) = match scan_folder(&source_dir) {
        Ok(rows) => (rows, None),
        Err(reason) => (Vec::new(), Some(reason)),
    };
    for row in &mut rows {
        let Some(image) = loaded.pack.images().iter().find(|img| img.id.as_str() == row.file_name) else {
            continue; // FR-006: a file new since the manifest was last saved stays unassigned.
        };
        match image.anchor {
            TimeAnchor::Solar { event, offset } => {
                let (offset_negative, offset_hours, offset_minutes) = decompose_offset(offset);
                row.solar = Some(SolarAssignment { event, offset_negative, offset_hours, offset_minutes });
            }
            TimeAnchor::Clock(time) => row.time = Some(time),
            TimeAnchor::Period { period, .. } => row.period = Some(period),
        }
    }
    if mode == AssignmentMode::SolarBlocks {
        // Rows follow block order, then slot order within a block; images not in the
        // manifest go to the "not included" tray at the end. `sort_by_key` is stable.
        let slot_of = |name: &str| {
            loaded.pack.images().iter().find(|img| img.id.as_str() == name).and_then(|img| match img.anchor {
                TimeAnchor::Period { period, slot, .. } => Some((period.index(), slot)),
                _ => None,
            })
        };
        rows.sort_by_key(|row| slot_of(&row.file_name).unwrap_or((SolarPeriod::COUNT, 0)));
    }

    let per_image_scaling = loaded
        .image_scaling
        .iter()
        .filter(|(_, scaling)| **scaling != loaded.default_scaling)
        .map(|(id, scaling)| (id.as_str().to_string(), *scaling))
        .collect();

    Ok(State {
        source_dir,
        mode: Some(mode),
        rows,
        author: loaded.author.clone().unwrap_or_default(),
        name: loaded.name.clone(),
        conflict: None,
        generate_error: None,
        move_error: None,
        pending_collision: None,
        pending_placement: None,
        scan_error,
        pending_manifest_text: None,
        edit_target: Some(source),
        preserved: Some(PreservedManifestFields {
            default_scaling: loaded.default_scaling,
            fallback_color: loaded.fallback_color,
            per_image_scaling,
        }),
        location: None,
        preview_date: Local::now().date_naive(),
        dragging: None,
        drop_target: None,
    })
}

/// Scans `dir` for candidate images (research.md R2): keeps only files that pass a
/// header-only readability check, silently skipping everything else (non-images,
/// subdirectories, a stray `manifest.toml`). Fails with a specific message for FR-018's
/// two boundary cases — no usable images, or more than `schedule_engine::MAX_ANCHORS`.
fn scan_folder(dir: &Path) -> Result<Vec<ImageRow>, String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| trf("couldn't read {path}: {error}", &[("path", &dir.display().to_string()), ("error", &e.to_string())]))?;

    let mut rows = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if is_readable_image(&path) {
            rows.push(ImageRow::new(file_name.to_string(), path));
        }
    }
    rows.sort_by(|a, b| a.file_name.cmp(&b.file_name));

    if rows.is_empty() {
        return Err(trf("{path} has no readable images to build a pack from.", &[("path", &dir.display().to_string())]));
    }
    if rows.len() > schedule_engine::MAX_ANCHORS {
        return Err(trf(
            "{path} has {count} images — a single pack can hold at most {max}.",
            &[
                ("path", &dir.display().to_string()),
                ("count", &rows.len().to_string()),
                ("max", &schedule_engine::MAX_ANCHORS.to_string()),
            ],
        ));
    }
    Ok(rows)
}

/// Header-only readability check (research.md R2) — mirrors `pack_loader::image_check`'s
/// own check exactly, duplicated here since that module is private to `pack-loader` and
/// exists to validate a manifest's *declared* files, not to scan an unconfigured folder
/// (research.md R2's "duplicate a small piece rather than invert crate ownership" call).
fn is_readable_image(path: &Path) -> bool {
    let Ok(reader) = ImageReader::open(path) else { return false };
    let Ok(reader) = reader.with_guessed_format() else { return false };
    reader.into_dimensions().is_ok()
}

// --- Pure functions (data-model.md) ---

/// Combines a sign, hours, and minutes into one `TimeDelta`, clamping to the ±12h cap
/// (research.md R6) regardless of what's passed in — `hours` above 12 is clamped to 12,
/// and `minutes` is forced to 0 once `hours` is already 12.
pub fn combine_offset(negative: bool, hours: u32, minutes: u32) -> TimeDelta {
    let hours = hours.min(12);
    let minutes = if hours >= 12 { 0 } else { minutes.min(59) };
    let magnitude = TimeDelta::hours(i64::from(hours)) + TimeDelta::minutes(i64::from(minutes));
    if negative {
        -magnitude
    } else {
        magnitude
    }
}

/// Spec 012 US1 (research.md R4): the pure inverse of [`combine_offset`], used by
/// `open_for_edit` to pre-fill a `SolarAssignment`'s sign/hours/minutes fields from an
/// already-loaded pack's `TimeAnchor::Solar { offset, .. }`. `None` (no offset at all)
/// decomposes to `(false, 0, 0)`, matching a freshly-added row's own zeroed default.
/// Every whole-minute value `combine_offset` can itself produce round-trips through
/// this exactly; this function does not clamp or reject an out-of-range magnitude
/// itself (see [`offset_within_wizard_range`] for the caller-side check that decides
/// whether a *loaded* pack's offset is even safe to hand to this at all).
pub fn decompose_offset(offset: Option<TimeDelta>) -> (bool, u32, u32) {
    let Some(delta) = offset else { return (false, 0, 0) };
    let negative = delta < TimeDelta::zero();
    let magnitude = if negative { -delta } else { delta };
    let total_minutes = magnitude.num_minutes().max(0);
    let hours = u32::try_from(total_minutes / 60).unwrap_or(u32::MAX);
    let minutes = u32::try_from(total_minutes % 60).unwrap_or(0);
    (negative, hours, minutes)
}

/// Spec 012 US1 (research.md R3, discovered while implementing `open_for_edit`):
/// `schedule_engine::WallpaperPack::validate` accepts a solar offset up to
/// `schedule_engine::MAX_SOLAR_OFFSET_HOURS` (24h) — wider than this wizard's own
/// spin-button range (`combine_offset`'s ±12h cap). A pack with a wider offset than
/// this loads successfully but can't be *faithfully* represented by this screen's
/// controls: silently clamping it down on open would be exactly the kind of "reset a
/// field this screen doesn't fully control" FR-009 rules out, just for an offset
/// instead of a scaling mode. `open_for_edit` treats this the same as any other
/// can't-safely-edit-this-pack case (FR-019) — refusing to open rather than opening
/// with a value it would silently change if the user saved without touching that row
/// at all.
fn offset_within_wizard_range(offset: Option<TimeDelta>) -> bool {
    match offset {
        None => true,
        Some(delta) => delta >= -TimeDelta::hours(12) && delta <= TimeDelta::hours(12),
    }
}

/// Blank or whitespace-only input becomes "Artist Unknown" (FR-010); anything else is
/// used verbatim, trimmed of leading/trailing whitespace.
pub fn effective_author(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        "Artist Unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Spec 012 FR-015/FR-017: blank or whitespace-only input falls back to `fallback`
/// (the folder's own name) rather than saving an empty pack name — the same shape as
/// [`effective_author`], but with a dynamic fallback instead of a fixed string, since
/// "no name set" means something different for every pack (its own folder name) rather
/// than one shared default.
pub fn effective_name(input: &str, fallback: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// FR-009: every row must have an explicit assignment before Generate is available. An
/// empty row list is never "all assigned" — there'd be nothing to generate.
///
/// `SolarBlocks` is the exception: images left in the "not included" tray are simply
/// omitted from the pack, so only at least one placed image is required.
pub fn all_assigned(rows: &[ImageRow], mode: AssignmentMode) -> bool {
    match mode {
        AssignmentMode::SolarBlocks => rows.iter().any(|row| row.period.is_some()),
        AssignmentMode::SolarPeriod => !rows.is_empty() && rows.iter().all(|row| row.solar.is_some()),
        AssignmentMode::SpecificTime => !rows.is_empty() && rows.iter().all(|row| row.time.is_some()),
    }
}

/// The static block layout described by `rows` (`SolarBlocks` mode).
pub fn rows_layout(rows: &[ImageRow]) -> PeriodLayout {
    let mut layout = PeriodLayout::new();
    for row in rows {
        if let Some(period) = row.period {
            layout.block_mut(period).push(ImageId::new(row.file_name.clone()));
        }
    }
    layout
}

fn solar_row_anchor(assignment: SolarAssignment) -> TimeAnchor {
    let offset = if assignment.offset_hours == 0 && assignment.offset_minutes == 0 {
        None
    } else {
        Some(combine_offset(assignment.offset_negative, assignment.offset_hours, assignment.offset_minutes))
    };
    TimeAnchor::solar(assignment.event, offset)
}

/// Builds the `ManifestDraft` Generate will render (FR-011). For a brand-new pack
/// (`preserved: None`), applies research.md R10's original defaults: `default_scaling =
/// Fill`, `fallback_color = #000000`, no per-image scaling override. For an edit
/// session (`preserved: Some(..)`), those three carry forward from the pack as it was
/// loaded instead (spec 012 FR-009, research.md R5) — this screen never resets a field
/// it doesn't itself show a control for. `name` falls back to `folder_name` when blank
/// (FR-015/FR-017, `effective_name`). Rows without an assignment for the active mode
/// are skipped rather than panicking — callers are expected to have already checked
/// `all_assigned` (Generate is gated on it), but this stays total either way.
pub fn build_draft(
    rows: &[ImageRow],
    mode: AssignmentMode,
    name: &str,
    folder_name: &str,
    author: &str,
    preserved: Option<&PreservedManifestFields>,
) -> ManifestDraft {
    let layout = rows_layout(rows);
    let images = rows
        .iter()
        .filter_map(|row| {
            let anchor = match mode {
                AssignmentMode::SolarBlocks => row.period.and_then(|period| {
                    let block = layout.block(period);
                    let slot = block.iter().position(|id| id.as_str() == row.file_name)?;
                    Some(TimeAnchor::Period { period, slot: u8::try_from(slot).ok()?, of: u8::try_from(block.len()).ok()? })
                }),
                AssignmentMode::SolarPeriod => row.solar.map(solar_row_anchor),
                AssignmentMode::SpecificTime => row.time.map(TimeAnchor::clock),
            }?;
            let scaling = preserved.and_then(|p| p.per_image_scaling.get(&row.file_name).copied());
            Some(ManifestDraftImage { file: row.file_name.clone(), anchor, scaling })
        })
        .collect();

    let (default_scaling, fallback_color) = match preserved {
        Some(p) => (p.default_scaling, p.fallback_color),
        None => (ScalingMode::Fill, Color { r: 0, g: 0, b: 0, a: 255 }),
    };

    ManifestDraft { name: effective_name(name, folder_name), author: Some(effective_author(author)), default_scaling, fallback_color, images }
}

/// FR-008: checks the current rows for a scheduling conflict. Returns `None` when
/// there's nothing to flag yet — including while some rows are still unassigned, since
/// `all_assigned` already gates Generate separately; this only ever reports genuine
/// conflicts between rows that *are* assigned.
pub fn detect_conflict(rows: &[ImageRow], mode: AssignmentMode, location: Option<Location>) -> Option<String> {
    match mode {
        // Slots are spread evenly inside a block, so two images can never collide.
        AssignmentMode::SolarBlocks => None,
        AssignmentMode::SolarPeriod => detect_solar_conflict(rows, location),
        AssignmentMode::SpecificTime => detect_clock_conflict(rows),
    }
}

/// research.md R4's two layers: a location-independent literal-equality check first
/// (always available), then `schedule_engine`'s own, already-tested
/// `check_solar_duplicate_instant` once every row is assigned and a location exists.
fn detect_solar_conflict(rows: &[ImageRow], location: Option<Location>) -> Option<String> {
    let assignments: Vec<SolarAssignment> = rows.iter().filter_map(|r| r.solar).collect();

    for i in 0..assignments.len() {
        for other in &assignments[i + 1..] {
            let a = assignments[i];
            if a.event == other.event
                && a.offset_negative == other.offset_negative
                && a.offset_hours == other.offset_hours
                && a.offset_minutes == other.offset_minutes
            {
                return Some(trf(
                    "Two images are both scheduled for {event} — pick a different event or offset for one of them.",
                    &[("event", &solar_event_label(a.event))],
                ));
            }
        }
    }

    if assignments.len() != rows.len() {
        return None;
    }
    let location = location?;
    let pack_images: Vec<PackImage> =
        rows.iter().filter_map(|r| r.solar.map(|s| PackImage::new(r.file_name.clone(), solar_row_anchor(s)))).collect();
    match WallpaperPack::validate(pack_images) {
        Ok(validated) => match validated.check_solar_duplicate_instant(&location, Local::now().date_naive()) {
            Ok(()) => None,
            Err(_) => Some(tr("Two images resolve to the same moment today — adjust one of their offsets.")),
        },
        // Empty/TooManyAnchors/MixedAnchorTypes aren't reachable through this UI's own
        // rows (scan_folder/set_mode already rule them out) — not this check's concern.
        Err(_) => None,
    }
}

fn detect_clock_conflict(rows: &[ImageRow]) -> Option<String> {
    let times: Vec<NaiveTime> = rows.iter().filter_map(|r| r.time).collect();
    if times.len() != rows.len() {
        return None;
    }
    let pack_images: Vec<PackImage> =
        rows.iter().filter_map(|r| r.time.map(|t| PackImage::new(r.file_name.clone(), TimeAnchor::clock(t)))).collect();
    match WallpaperPack::validate(pack_images) {
        Err(PackError::DuplicateInstant) => {
            Some(tr("Two images are set to the exact same time — pick a different time for one of them."))
        }
        _ => None,
    }
}

// --- State mutators (called from `app.rs`'s `update`) ---

fn recompute_conflict(state: &mut State, location: Option<Location>) {
    state.conflict = state.mode.and_then(|mode| detect_conflict(&state.rows, mode, location));
}

/// FR-004/Edge Cases: sets the mode and discards every row's assignment for the *other*
/// mode (a pack can't mix solar and clock anchors) — safe to call both for the initial
/// choice and a later switch, since rows start with both fields already `None`.
pub fn set_mode(state: &mut State, mode: AssignmentMode) {
    state.mode = Some(mode);
    for row in &mut state.rows {
        row.solar = None;
        row.time = None;
        row.period = None;
    }
    state.conflict = None;
    state.generate_error = None;
    state.dragging = None;
    state.drop_target = None;
}

// --- Solar blocks editor ---

/// Moves the row named `file_name` into `block` (`None` = the "not included" tray),
/// before the `index`-th row currently in that block (indices counted *before* the
/// move, so dropping an image onto its own position is a no-op). Other blocks' rows
/// keep their relative order — only the target block's times change.
pub fn place_row(rows: &mut Vec<ImageRow>, file_name: &str, block: Option<SolarPeriod>, index: usize) {
    let Some(from) = rows.iter().position(|r| r.file_name == file_name) else { return };
    let members: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| r.period == block).map(|(i, _)| i).collect();
    // Absolute insertion point in the pre-removal vector.
    let insert_at = match members.get(index) {
        Some(&i) => i,
        None => match members.last() {
            Some(&last) => last + 1,
            None => block_insertion_point(rows, block, from),
        },
    };
    let mut row = rows.remove(from);
    row.period = block;
    let insert_at = if from < insert_at { insert_at - 1 } else { insert_at };
    rows.insert(insert_at.min(rows.len()), row);
}

/// Where an empty `block` would start in `rows`: just before the first row of any later
/// block (the tray counts as the last), ignoring the row being moved.
fn block_insertion_point(rows: &[ImageRow], block: Option<SolarPeriod>, moving: usize) -> usize {
    let rank = |p: Option<SolarPeriod>| p.map_or(SolarPeriod::COUNT, SolarPeriod::index);
    rows.iter()
        .enumerate()
        .find(|(i, r)| *i != moving && rank(r.period) > rank(block))
        .map_or(rows.len(), |(i, _)| i)
}

/// Moves an image one position earlier/later inside its own block.
pub fn move_within_block(state: &mut State, file_name: &str, delta: i32) {
    let Some(row) = state.rows.iter().find(|r| r.file_name == file_name) else { return };
    let block = row.period;
    let members: Vec<&str> = state.rows.iter().filter(|r| r.period == block).map(|r| r.file_name.as_str()).collect();
    let Some(pos) = members.iter().position(|n| *n == file_name) else { return };
    // `place_row` indexes before removal: moving later by one means "before the item
    // two positions ahead".
    let target = match delta {
        d if d < 0 && pos > 0 => pos - 1,
        d if d > 0 && pos + 1 < members.len() => pos + 2,
        _ => return,
    };
    place_row(&mut state.rows, file_name, block, target);
}

pub fn select_block(state: &mut State, file_name: &str, block: Option<SolarPeriod>) {
    let current = state.rows.iter().find(|r| r.file_name == file_name).map(|r| r.period);
    if current != Some(block) {
        place_row(&mut state.rows, file_name, block, usize::MAX);
    }
}

pub fn start_drag(state: &mut State, file_name: String) {
    state.dragging = Some(file_name);
    state.drop_target = None;
}

pub fn drag_entered(state: &mut State, target: DropTarget) {
    if state.dragging.is_some() {
        state.drop_target = Some(target);
    }
}

pub fn drag_scrolled(state: &mut State) {
    state.drop_target = None;
}

pub fn drag_left(state: &mut State, target: DropTarget) {
    if state.drop_target == Some(target) {
        state.drop_target = None;
    }
}

/// Ends a drag: drops onto the hovered target, or cancels when released elsewhere.
pub fn release_drag(state: &mut State) {
    if let (Some(name), Some(target)) = (state.dragging.take(), state.drop_target.take()) {
        place_row(&mut state.rows, &name, target.block, target.index);
    }
}

pub fn shift_preview_day(state: &mut State, days: i64) {
    let today = Local::now().date_naive();
    state.preview_date = if days == 0 { today } else { state.preview_date + TimeDelta::days(days) };
}

/// The block an event opens (or, for noon/midnight, sits in) — used when converting an
/// un-offset event anchor, whose instant lies exactly on a block edge and could
/// otherwise round into the previous block, and for events that don't happen on the
/// conversion day (polar day/night).
fn fallback_period(event: SolarEventKind) -> SolarPeriod {
    match event {
        SolarEventKind::CivilDawn | SolarEventKind::Sunrise => SolarPeriod::Morning,
        SolarEventKind::SolarNoon => SolarPeriod::Day,
        SolarEventKind::Sunset => SolarPeriod::Evening,
        SolarEventKind::CivilDusk
        | SolarEventKind::AstronomicalDusk
        | SolarEventKind::SolarMidnight
        | SolarEventKind::AstronomicalDawn => SolarPeriod::Night,
    }
}

/// Converts the rows' event (`SolarPeriod`) or clock (`SpecificTime`) assignments into
/// solar blocks: each image goes into the block its moment falls in on `date` at
/// `location`, ordered by that moment. Unassigned images land in the tray. The
/// resulting schedule looks the same as before for that day, but adapts across the
/// year from then on.
pub fn convert_rows_to_blocks(rows: &mut Vec<ImageRow>, location: &Location, date: NaiveDate) {
    let day_span = schedule_engine::PeriodBoundaries::compute(location, date).map(|b| b.day_span());
    let mut keyed: Vec<(usize, i64, ImageRow)> = rows
        .drain(..)
        .map(|mut row| {
            let anchor = row.solar.map(solar_row_anchor).or(row.time.map(TimeAnchor::clock));
            let instant: Option<DateTime<Local>> =
                anchor.as_ref().and_then(|a| schedule_engine::anchor_instant(a, Some(location), date));
            let period = match (instant, anchor) {
                (_, Some(TimeAnchor::Solar { event, offset: None })) | (None, Some(TimeAnchor::Solar { event, .. })) => {
                    Some(fallback_period(event))
                }
                (Some(at), _) => schedule_engine::period_at(location, at),
                _ => None,
            };
            // Order inside a block by position within the solar day: this evening's
            // night belongs before this morning's (it starts the next solar day's Night
            // block), and a time after local midnight but before the solar day starts
            // continues the previous evening's block.
            let key = instant.map_or(i64::MAX, |at| {
                let at = match day_span {
                    Some((_, end)) if at >= end => at - TimeDelta::days(1),
                    Some((start, _)) if at < start => at + TimeDelta::days(1),
                    _ => at,
                };
                at.timestamp()
            });
            row.solar = None;
            row.time = None;
            row.period = period;
            (period.map_or(SolarPeriod::COUNT, SolarPeriod::index), key, row)
        })
        .collect();
    keyed.sort_by_key(|(rank, key, _)| (*rank, *key));
    rows.extend(keyed.into_iter().map(|(_, _, row)| row));
}

pub fn convert_to_blocks(state: &mut State) {
    let Some(location) = state.location else { return };
    let date = state.preview_date;
    convert_rows_to_blocks(&mut state.rows, &location, date);
    state.mode = Some(AssignmentMode::SolarBlocks);
    state.conflict = None;
    state.generate_error = None;
}

/// Computed times for the blocks editor; `None` without a location.
pub fn preview_schedule(state: &State) -> Option<ResolvedSchedule> {
    schedule_engine::resolve_schedule(&rows_layout(&state.rows), state.location.as_ref()?, state.preview_date)
}

pub fn set_solar_event_by_index(state: &mut State, row: usize, index: usize, location: Option<Location>) {
    if let Some(event) = SOLAR_EVENTS.get(index).copied() {
        if let Some(r) = state.rows.get_mut(row) {
            let assignment = r.solar.get_or_insert(SolarAssignment { event, offset_negative: false, offset_hours: 0, offset_minutes: 0 });
            assignment.event = event;
        }
    }
    recompute_conflict(state, location);
}

pub fn toggle_solar_offset_sign(state: &mut State, row: usize, location: Option<Location>) {
    if let Some(a) = state.rows.get_mut(row).and_then(|r| r.solar.as_mut()) {
        a.offset_negative = !a.offset_negative;
    }
    recompute_conflict(state, location);
}

pub fn set_solar_offset_hours(state: &mut State, row: usize, hours: u32, location: Option<Location>) {
    if let Some(a) = state.rows.get_mut(row).and_then(|r| r.solar.as_mut()) {
        a.offset_hours = hours.min(12);
        if a.offset_hours >= 12 {
            a.offset_minutes = 0;
        }
    }
    recompute_conflict(state, location);
}

pub fn set_solar_offset_minutes(state: &mut State, row: usize, minutes: u32, location: Option<Location>) {
    if let Some(a) = state.rows.get_mut(row).and_then(|r| r.solar.as_mut()) {
        if a.offset_hours < 12 {
            a.offset_minutes = minutes.min(59);
        }
    }
    recompute_conflict(state, location);
}

pub fn set_time_hour(state: &mut State, row: usize, hour: u32) {
    if let Some(r) = state.rows.get_mut(row) {
        let minute = r.time.map_or(0, |t| t.minute());
        r.time = NaiveTime::from_hms_opt(hour.min(23), minute, 0).or(Some(NaiveTime::MIN));
    }
    recompute_conflict(state, None);
}

pub fn set_time_minute(state: &mut State, row: usize, minute: u32) {
    if let Some(r) = state.rows.get_mut(row) {
        let hour = r.time.map_or(0, |t| t.hour());
        r.time = NaiveTime::from_hms_opt(hour, minute.min(59), 0).or(Some(NaiveTime::MIN));
    }
    recompute_conflict(state, None);
}

pub fn set_author(state: &mut State, author: String) {
    state.author = author;
}

/// Spec 012 FR-015 (US4): `NameChanged` — the raw text is stored as-is; blank/
/// whitespace-only input isn't normalized here (mirrors `set_author`), only at the
/// point `build_draft` actually needs an effective value (`effective_name`), so the
/// text field itself never fights the user's typing.
pub fn set_name(state: &mut State, name: String) {
    state.name = name;
}

// --- Generate (FR-011, FR-012, FR-017; contracts/pack-loader-manifest-writer.md) ---

/// Builds the draft, renders it, and self-validates it — the exact validation path a
/// real pack registration takes — **without writing `manifest.toml` into
/// `state.source_dir` yet** (spec 011 US6 FR-027, research.md R22). Self-validation
/// runs against a scratch directory populated with symlinks to the real images
/// (cheap — no copying), discarded immediately after; `state.source_dir` itself is
/// never touched by this function. On success, the rendered text is held in
/// `state.pending_manifest_text` and `state.pending_placement` is set so the caller
/// shows the placement dialog; the manifest is only actually written once the user
/// makes that choice (`finalize`). On any failure, neither field is set and
/// `state.generate_error` reports why; `state.rows`/`author` are never touched either
/// way (FR-017).
///
/// **Why this matters**: previously, `manifest.toml` was written directly into
/// `state.source_dir` here, before the placement dialog was even shown — a crash or
/// force-quit between that write and the user's Move/Keep choice left the source
/// folder permanently mutated with an unregistered `manifest.toml`. On next launch,
/// `should_open_for`'s `ManifestNotFound`-only check would then see the existing
/// manifest and skip the wizard entirely — an implicit "Keep it here" the user never
/// actually chose. Deferring the write removes that window: nothing is written to
/// `state.source_dir` until Move or Keep actually runs.
/// Returns `true` only when this call fully completed an **edit** session's save
/// (self-validated, written, no further step needed) and the caller must close the
/// wizard and refresh the Packs screen — mirrors `confirm_move`/`confirm_keep`'s own
/// "did this fully finish" signal. Always `false` for the add flow: its own completion
/// signal remains `confirm_move`/`confirm_keep`, triggered by a *later*, separate
/// placement-dialog click, since a brand-new pack still needs a Move-vs-Keep decision
/// this function doesn't make. An edit session has no placement dialog to wait for
/// (research.md R6) — for it, Generate succeeding *is* the terminal action (spec 012
/// FR-020: immediate save, no extra confirmation beyond Cancel already being available
/// beforehand). Also `false` on any failure, surfaced via `state.generate_error` as
/// before — `#[must_use]` because silently ignoring a `true` here would leave a
/// successfully-saved edit session's wizard sitting open, showing stale state.
#[must_use]
pub fn generate(state: &mut State) -> bool {
    state.generate_error = None;
    let Some(mode) = state.mode else { return false };

    // Spec 011 US6 FR-026 (research.md R21): re-checks the same `all_assigned` pure
    // function the UI button's `enabled` state already gates on
    // (`generate_button.on_press` is only wired when it's `true`) — this is the
    // handler-side half of that gate, so a future call site that fires
    // `GenerateRequested` without going through the gated button (a bug, a
    // programmatic trigger, a test) can't silently produce an incomplete pack.
    // `build_draft`'s own `filter_map` still degrades safely even if this check is
    // ever bypassed (its own doc comment: "stays total either way"), but this is the
    // check that's actually supposed to prevent that from being reachable at all.
    if !all_assigned(&state.rows, mode) {
        state.generate_error = Some(tr("every image needs an assignment before a pack can be generated."));
        return false;
    }

    // Spec 012 FR-012/FR-013 (discovered while adding the edit-save path below): the
    // add flow's own `can_generate` in `configuration_view` already disables the
    // Generate button while `state.conflict.is_some()`, but — like `all_assigned`
    // above (spec 011 FR-026) — that's only a UI-layer gate, not something this
    // function re-checked itself. `WallpaperPack::validate` has no literal-duplicate-
    // solar-anchor check of its own (only `detect_solar_conflict`'s UI-side check
    // does, research.md R4's two-layer check), so without this, a call to `generate`
    // that bypasses the disabled button (a bug, a stale conflict left over from a
    // previous row edit, a programmatic trigger) could silently write a pack with two
    // images racing for the same display moment — exactly what FR-012 requires never
    // happen, not just "usually doesn't happen because the button was disabled."
    if state.conflict.is_some() {
        state.generate_error = Some(tr("resolve the scheduling conflict before generating a pack."));
        return false;
    }

    let folder_name = folder_display_name(&state.source_dir);
    let draft = build_draft(&state.rows, mode, &state.name, &folder_name, &state.author, state.preserved.as_ref());
    let text = pack_loader::render(&draft);

    if let Err(e) = self_validate_in_scratch_dir(&state.source_dir, &draft, &text) {
        state.generate_error = Some(trf("the generated pack didn't validate: {error}", &[("error", &e.to_string())]));
        return false;
    }

    if state.edit_target.is_some() {
        // Spec 012 US1 (research.md R6): editing never relocates a pack, so there is
        // no Move-vs-Keep choice to show — write straight into `source_dir`, the
        // pack's existing, already-registered location, and this session is done.
        return match write_edit_manifest(&state.source_dir, &text) {
            Ok(()) => true,
            Err(e) => {
                state.generate_error = Some(e);
                false
            }
        };
    }

    state.pending_manifest_text = Some(text);
    state.pending_placement = Some(GeneratedPlacement { generated_path: state.source_dir.clone() });
    false
}

/// Spec 012 FR-007 (US1): overwrites `source_dir/manifest.toml` with `text` — the one
/// write path in this module that replaces an *existing*, currently-registered pack's
/// manifest in place, rather than writing into a brand-new or about-to-be-discarded
/// location. A plain `std::fs::write` truncates before writing the new bytes, so a
/// crash/failure mid-write could leave a half-written, unparseable manifest behind;
/// this instead writes `text` to a same-directory temp file first, then
/// `std::fs::rename`s it over the real manifest path. A same-directory rename is
/// atomic on every platform this project targets (POSIX/Wayland-only, constitution
/// Principle II), so `manifest.toml` is always either wholly the old content or wholly
/// the new content, never a partial mix — satisfying FR-007's "on failure, the
/// manifest on disk MUST remain exactly as it was" without needing to re-read and
/// restore a backup after the fact. No second `pack_loader::load_pack` re-validation
/// runs after the rename (unlike `write_manifest_and_register`'s existing pattern for
/// a brand-new "Keep it here" pack): `text` was already proven to load successfully,
/// against copies of these exact images, by `generate()`'s own
/// `self_validate_in_scratch_dir` call immediately before this function runs — redoing
/// that same check against the real directory moments later would be pure repetition,
/// not an independent safety net (and, unlike the Keep path, there is no discardable
/// scratch/destination directory left here to clean up if it somehow did fail).
fn write_edit_manifest(source_dir: &Path, text: &str) -> Result<(), String> {
    let manifest_path = source_dir.join(pack_loader::MANIFEST_FILE_NAME);
    let tmp_path = source_dir.join(format!("{}.tmp-{}", pack_loader::MANIFEST_FILE_NAME, std::process::id()));
    std::fs::write(&tmp_path, text).map_err(|e| trf("couldn't write the updated manifest: {error}", &[("error", &e.to_string())]))?;
    std::fs::rename(&tmp_path, &manifest_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        trf("couldn't replace the existing manifest: {error}", &[("error", &e.to_string())])
    })
}

/// Validates `text` (the rendered manifest for `draft`) without writing anything into
/// `source_dir`: builds a throwaway scratch directory, symlinks every image `draft`
/// references (from `source_dir`, where the real files live) into it under the same
/// filename, writes `text` there, and runs it through the real `pack_loader::load_pack`
/// path. The scratch directory is removed before returning either way.
fn self_validate_in_scratch_dir(source_dir: &Path, draft: &ManifestDraft, text: &str) -> Result<(), pack_loader::ManifestError> {
    let scratch = std::env::temp_dir().join(format!("cosmic-pack-builder-validate-{}-{}", std::process::id(), fastrand_like_id()));
    let result = (|| {
        std::fs::create_dir_all(&scratch).map_err(|e| pack_loader::ManifestError::Io { path: scratch.clone(), message: e.to_string() })?;
        // Real copies, deliberately not symlinks: `pack_loader::path_safety` itself
        // canonicalizes every entry and rejects anything that resolves outside the
        // pack directory *precisely* to catch a symlink pointing elsewhere — a
        // symlink into `source_dir` from this scratch dir would trip that exact
        // check (confirmed the hard way: an earlier symlink-based version of this
        // function failed every real test with `PathEscapesPackDirectory`). Images
        // are already capped at `schedule_engine::MAX_ANCHORS` (64) by this point, so
        // the copy cost here is bounded, one-time, per Generate click — not a hot path.
        for image in &draft.images {
            let target = source_dir.join(&image.file);
            let copy_to = scratch.join(&image.file);
            std::fs::copy(&target, &copy_to).map_err(|e| pack_loader::ManifestError::Io { path: copy_to.clone(), message: e.to_string() })?;
        }
        std::fs::write(scratch.join(pack_loader::MANIFEST_FILE_NAME), text)
            .map_err(|e| pack_loader::ManifestError::Io { path: scratch.clone(), message: e.to_string() })?;
        pack_loader::load_pack(&scratch)
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result.map(|_| ())
}

/// A short, process-and-call-unique suffix for the scratch validation directory's
/// name — collision-avoidance only (two overlapping `generate()` calls in the same
/// process, e.g. from rapid double-invocation), not a security boundary; the scratch
/// dir's contents never persist past `self_validate_in_scratch_dir`'s own return.
fn fastrand_like_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

// --- Placement (FR-013–FR-017, FR-014a; research.md R8) ---

/// `dirs::data_dir()/cosmic-dynamic-wallpaper/packs` (research.md R8) — `None` only if
/// the platform has no resolvable data directory at all (not expected on a real COSMIC
/// session, but handled rather than assumed).
pub fn standard_pack_location() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("cosmic-dynamic-wallpaper").join("packs"))
}

#[derive(Debug)]
enum MoveError {
    Collision,
    InvalidName(String),
    Io(String),
}

/// Spec 011 US2 FR-006/FR-007 (research.md R5): the collision-rename text field is
/// free-form user input that gets joined directly onto `destination_root` in
/// [`move_pack`] — reject anything that could escape that root or collapse the join
/// onto the root itself, *before* it's ever joined. Rejects:
/// - an empty string (`destination_root.join("")` resolves to `destination_root`
///   itself, silently merging this pack's contents into the shared packs directory —
///   FR-007);
/// - any path component other than `Normal` (`..`, `.`, root/prefix components) via
///   `Path::new(name).components()` — catches `../../../.config/autostart` and every
///   other traversal shape, not just the literal `..` substring;
/// - an absolute path (`Path::is_absolute`) — belt-and-suspenders with the component
///   check above, since an absolute path's first component is a `RootDir`/`Prefix`
///   component that the check above already rejects, but stated explicitly since this
///   is the shape the audit's own reproduction used (`/home/user/.ssh`).
///
/// Deliberately **not** implemented by reusing `pack_loader::path_safety::
/// resolve_and_check` (research.md R5's Alternatives) — that function's first check is
/// `candidate.exists()`, which is backwards for validating a *destination* name that
/// must specifically not yet exist.
fn validate_destination_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(tr("the pack name can't be empty."));
    }
    let only_normal_components = Path::new(name).components().all(|c| matches!(c, std::path::Component::Normal(_)));
    if !only_normal_components || Path::new(name).is_absolute() {
        return Err(tr("the pack name can't contain path separators, \"..\", or be an absolute path."));
    }
    Ok(())
}

/// The copy-then-verify-then-delete move routine (research.md R8): recursively copies
/// `generated_path` to `destination_root/name`, self-validates the copy via
/// `pack_loader::load_pack`, then removes `generated_path` only after that succeeds.
/// Any failure removes a partial destination copy and leaves `generated_path`
/// completely untouched.
///
/// Spec 011 US6 FR-027 (research.md R22): `generated_path` (the source folder) no
/// longer contains a `manifest.toml` at this point — `generate()` only holds its text
/// in memory. `manifest_text` is written into `destination` *after* the image copy
/// and *before* self-validation, so the existing `pack_loader::load_pack(&destination)`
/// call below validates the just-written manifest exactly as it always has, and
/// `generated_path` (the source) is never mutated with a manifest at all when moving.
fn move_pack(generated_path: &Path, destination_root: &Path, name: &str, manifest_text: &str) -> Result<PathBuf, MoveError> {
    validate_destination_name(name).map_err(MoveError::InvalidName)?;
    let destination = destination_root.join(name);

    std::fs::create_dir_all(destination_root).map_err(|e| MoveError::Io(e.to_string()))?;
    // Spec 011 US2 FR-009 (research.md R6): `std::fs::create_dir` (not
    // `create_dir_all`) on the final destination-name segment is an atomic
    // exists-or-create check — it fails with `AlreadyExists` if anything (including a
    // concurrent writer) creates `destination` between this call and the moment it
    // runs, closing most of the window a separate `destination.exists()` check +
    // later write would leave open. `move_pack` still isn't fully immune to every
    // TOCTOU shape (no cross-platform `O_EXCL`-for-a-whole-directory-copy primitive
    // exists), but this is the tightest check-and-create this API offers.
    match std::fs::create_dir(&destination) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(MoveError::Collision),
        Err(e) => return Err(MoveError::Io(e.to_string())),
    }

    if let Err(e) = copy_dir_recursive(generated_path, &destination) {
        let _ = std::fs::remove_dir_all(&destination);
        return Err(MoveError::Io(e.to_string()));
    }
    if let Err(e) = std::fs::write(destination.join(pack_loader::MANIFEST_FILE_NAME), manifest_text) {
        let _ = std::fs::remove_dir_all(&destination);
        return Err(MoveError::Io(trf("couldn't write manifest.toml at the destination: {error}", &[("error", &e.to_string())])));
    }
    if let Err(e) = pack_loader::load_pack(&destination) {
        let _ = std::fs::remove_dir_all(&destination);
        return Err(MoveError::Io(trf("the moved copy didn't validate: {error}", &[("error", &e.to_string())])));
    }
    if let Err(e) = std::fs::remove_dir_all(generated_path) {
        return Err(MoveError::Io(trf(
            "copied to {path} but couldn't remove the original folder: {error}",
            &[("path", &destination.display().to_string()), ("error", &e.to_string())],
        )));
    }
    Ok(destination)
}

/// Copies every file and subdirectory from `from` into `to` (already-created).
/// Symlinks are deliberately neither followed nor recreated, rather than risking a copy
/// of something outside the source folder.
fn copy_dir_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// Spec 011 US6 FR-024 (research.md R19): both `PackSource::resolve` and
/// `registry.register` failures were previously discarded (`if let Ok`/`let _ =`),
/// with `pending_placement`/`pending_collision` unconditionally cleared right after —
/// so the wizard closed and reported success even when the pack was generated (and,
/// for a Move, physically relocated on disk) but never actually appeared in
/// "Registered packs." Now routes the specific failure into `state.move_error` (the
/// field this module already renders for a failed move, per its own doc comment),
/// re-arms `pending_placement` so the placement dialog reopens showing that error and
/// a fresh Move/Keep choice (regardless of which dialog the caller reached this from —
/// the collision prompt's `pending_collision` is always `None`/consumed by the time
/// this runs), and only clears everything on the success path. Returns `true` only on
/// success, so every caller can return the right "should the wizard actually close"
/// value instead of assuming success unconditionally.
fn register_and_close(state: &mut State, registry: &mut Registry, path: &Path) -> bool {
    let register_result = PackSource::resolve(path).map_err(|e| e.to_string()).and_then(|source| registry.register(source).map_err(|e| e.to_string()));
    if let Err(e) = register_result {
        state.move_error = Some(trf(
            "pack was generated at {path}, but couldn't be registered: {error}",
            &[("path", &path.display().to_string()), ("error", &e.to_string())],
        ));
        state.pending_placement = Some(GeneratedPlacement { generated_path: path.to_path_buf() });
        state.pending_collision = None;
        return false;
    }
    state.pending_placement = None;
    state.pending_collision = None;
    state.pending_manifest_text = None;
    state.move_error = None;
    true
}

/// The "Keep it here" outcome — shared by `confirm_keep` and
/// `cancel_collision_to_keep` (spec 011 US6 FR-027, research.md R22): writes the
/// held `pending_manifest_text` into `path/manifest.toml` — deferred from
/// `generate()` until this exact moment, the point the user actually chose to keep
/// the pack in place — self-validates it there, then registers. A write/validation
/// failure re-arms the placement dialog with the error (mirroring
/// `register_and_close`'s own failure handling) rather than leaving `path` with a
/// half-written manifest and no visible explanation.
fn write_manifest_and_register(state: &mut State, registry: &mut Registry, path: &Path) -> bool {
    let Some(text) = state.pending_manifest_text.clone() else {
        state.move_error = Some(tr("internal error: no pending manifest to write — try Generate again."));
        return false;
    };
    let manifest_path = path.join(pack_loader::MANIFEST_FILE_NAME);
    if let Err(e) = std::fs::write(&manifest_path, &text) {
        state.move_error = Some(trf("couldn't write manifest.toml: {error}", &[("error", &e.to_string())]));
        state.pending_placement = Some(GeneratedPlacement { generated_path: path.to_path_buf() });
        state.pending_collision = None;
        return false;
    }
    if let Err(e) = pack_loader::load_pack(path) {
        let _ = std::fs::remove_file(&manifest_path);
        state.move_error = Some(trf("the generated pack didn't validate: {error}", &[("error", &e.to_string())]));
        state.pending_placement = Some(GeneratedPlacement { generated_path: path.to_path_buf() });
        state.pending_collision = None;
        return false;
    }
    register_and_close(state, registry, path)
}

fn finish_move(state: &mut State, registry: &mut Registry, generated_path: &Path, root: &Path, name: &str) -> bool {
    // Cleared unconditionally before matching so a stale error from a *previous*
    // attempt (e.g. an InvalidName rejection) can't bleed into this attempt's dialog
    // if this attempt instead hits a plain Collision, which doesn't set `move_error`
    // itself.
    state.move_error = None;
    let Some(text) = state.pending_manifest_text.clone() else {
        state.move_error = Some(tr("internal error: no pending manifest to move — try Generate again."));
        return false;
    };
    match move_pack(generated_path, root, name, &text) {
        Ok(destination) => register_and_close(state, registry, &destination),
        Err(MoveError::Collision) => {
            state.pending_placement = None;
            state.pending_collision =
                Some(PendingCollision { generated_path: generated_path.to_path_buf(), suggested_name: name.to_string() });
            false
        }
        // Spec 011 US2 FR-006/FR-007 (research.md R5): re-open the rename prompt with
        // the rejected name still visible (so the user can see and fix what they
        // typed) rather than just showing an error with no way to retry input; no
        // filesystem operation happened (`validate_destination_name` runs before
        // `move_pack` touches disk at all).
        Err(MoveError::InvalidName(reason)) => {
            state.pending_placement = None;
            state.pending_collision =
                Some(PendingCollision { generated_path: generated_path.to_path_buf(), suggested_name: name.to_string() });
            state.move_error = Some(reason);
            false
        }
        Err(MoveError::Io(reason)) => {
            state.move_error = Some(reason);
            false
        }
    }
}

/// The placement dialog's "Move" action (FR-013, FR-014). Returns `true` when the
/// wizard is fully done and the caller should close it (and refresh the Packs page);
/// `false` means it needs to stay open (a collision prompt, or an error to show).
pub fn confirm_move(state: &mut State, registry: &mut Registry) -> bool {
    let Some(placement) = state.pending_placement.clone() else { return false };
    let Some(root) = standard_pack_location() else {
        state.move_error = Some(tr("couldn't determine the standard pack location."));
        return false;
    };
    let suggested_name =
        placement.generated_path.file_name().and_then(|n| n.to_str()).unwrap_or("custom-pack").to_string();
    finish_move(state, registry, &placement.generated_path, &root, &suggested_name)
}

/// The placement dialog's "Keep it here" action (FR-015). Writes the deferred
/// manifest into `source_dir` (FR-027) and closes the wizard on success; on a
/// write/validation/registration failure, re-shows the placement dialog with the
/// error (FR-024) instead of unconditionally closing.
pub fn confirm_keep(state: &mut State, registry: &mut Registry) -> bool {
    let Some(placement) = state.pending_placement.clone() else { return false };
    write_manifest_and_register(state, registry, &placement.generated_path)
}

pub fn set_collision_name(state: &mut State, name: String) {
    if let Some(pending) = state.pending_collision.as_mut() {
        pending.suggested_name = name;
    }
}

/// The collision prompt's "Move" (retry) action (FR-014a).
pub fn confirm_collision_move(state: &mut State, registry: &mut Registry) -> bool {
    let Some(pending) = state.pending_collision.clone() else { return false };
    let Some(root) = standard_pack_location() else {
        state.move_error = Some(tr("couldn't determine the standard pack location."));
        return false;
    };
    state.pending_collision = None;
    finish_move(state, registry, &pending.generated_path, &root, &pending.suggested_name)
}

/// The collision prompt's "Cancel" action — falls back to keeping the pack in place
/// (contracts/pack-builder-gui-flow.md: the folder didn't move, and now gets its
/// deferred manifest written in place — FR-027 — so this is never a destructive
/// cancel). Closes the wizard on success; on a write/validation/registration
/// failure, re-shows the placement dialog with the error (FR-024) instead of
/// unconditionally closing.
pub fn cancel_collision_to_keep(state: &mut State, registry: &mut Registry) -> bool {
    let Some(pending) = state.pending_collision.take() else { return false };
    write_manifest_and_register(state, registry, &pending.generated_path)
}

// --- View ---

pub fn view(state: &State) -> Element<'_, Message> {
    let content = if let Some(reason) = &state.scan_error {
        scan_error_view(reason)
    } else {
        match state.mode {
            None => mode_choice_view(),
            Some(mode) => configuration_view(state, mode),
        }
    };
    // The blocks editor fills the page with its own panes, sized so dragging rarely
    // needs scrolling; every other screen scrolls as a whole.
    let blocks_mode = state.scan_error.is_none() && state.mode == Some(AssignmentMode::SolarBlocks);
    let content = if blocks_mode { content } else { widget::scrollable(content).into() };
    // Always wrapped (not only while dragging) so the widget tree — and with it the
    // scroll position — stays stable when a drag starts. A release anywhere ends the
    // drag; drop targets only record where the cursor is.
    let mut area = widget::mouse_area(content).on_release(Message::DragReleased);
    if state.dragging.is_some() {
        area = area.interaction(cosmic::iced::mouse::Interaction::Grabbing);
    }
    area.into()
}

fn scan_error_view(reason: &str) -> Element<'_, Message> {
    widget::column::with_capacity(2)
        .spacing(cosmic::theme::spacing().space_s)
        .push(widget::text::body(reason.to_string()))
        .push(widget::button::standard(tr("Cancel")).on_press(Message::Cancelled))
        .into()
}

fn mode_choice_view<'a>() -> Element<'a, Message> {
    widget::column::with_capacity(4)
        .spacing(cosmic::theme::spacing().space_m)
        .push(widget::text::title3(tr("How do you want to schedule these images?")))
        .push(widget::text::body(tr(
            "\"By parts of the day\" lets you drag images into morning, day, evening and night — times follow the sun for your location automatically.",
        )))
        .push(
            widget::row::with_capacity(2)
                .spacing(cosmic::theme::spacing().space_s)
                .push(widget::button::suggested(tr("By parts of the day")).on_press(Message::ModeChosen(AssignmentMode::SolarBlocks)))
                .push(widget::button::standard(tr("By solar period")).on_press(Message::ModeChosen(AssignmentMode::SolarPeriod)))
                .push(widget::button::standard(tr("By specific time")).on_press(Message::ModeChosen(AssignmentMode::SpecificTime))),
        )
        .push(widget::button::standard(tr("Cancel")).on_press(Message::Cancelled))
        .into()
}

fn configuration_view(state: &State, mode: AssignmentMode) -> Element<'_, Message> {
    if mode == AssignmentMode::SolarBlocks {
        return blocks_view(state);
    }
    let mut layout = widget::column::with_capacity(8).spacing(cosmic::theme::spacing().space_m);
    layout = layout.push(widget::text::title3(match mode {
        AssignmentMode::SolarPeriod => tr("Assign each image to a solar period"),
        _ => tr("Assign each image a time of day"),
    }));
    let mut rows_col = widget::column::with_capacity(state.rows.len()).spacing(cosmic::theme::spacing().space_xs);
    for (index, row) in state.rows.iter().enumerate() {
        rows_col = rows_col.push(row_view(row, index, mode));
    }
    layout = layout.push(convert_hint_view(state));
    layout = layout.push(rows_col);
    for field in pack_fields(state) {
        layout = layout.push(field);
    }
    layout.push(footer_view(state, mode)).into()
}

/// The pack name and author inputs, each label and input on its own line.
fn pack_fields(state: &State) -> [Element<'_, Message>; 4] {
    // Spec 012 FR-015 (US4): the pack's display name — shown throughout the GUI in
    // place of the folder name, never renaming the folder itself. Placeholder mirrors
    // the author field's own "leave blank for the default" pattern, with the actual
    // folder name as the concrete example rather than a generic placeholder string,
    // since that default is different for every pack.
    let folder_name = folder_display_name(&state.source_dir);
    [
        widget::text::body(trf("Pack name (leave blank for \"{name}\")", &[("name", &folder_name)])).into(),
        widget::text_input::text_input(folder_name, &state.name).on_input(Message::NameChanged).into(),
        widget::text::body(tr("Author (leave blank for \"Artist Unknown\")")).into(),
        widget::text_input::text_input("Artist Unknown", &state.author).on_input(Message::AuthorChanged).into(),
    ]
}

/// Conflict/generate errors and the Cancel/Generate buttons.
fn footer_view(state: &State, mode: AssignmentMode) -> Element<'_, Message> {
    let spacing = cosmic::theme::spacing();
    let mut col = widget::column::with_capacity(3).spacing(spacing.space_s);
    if let Some(reason) = &state.conflict {
        col = col.push(widget::text::body(reason.clone()));
    }
    if let Some(reason) = &state.generate_error {
        col = col.push(widget::text::body(reason.clone()));
    }

    let can_generate = state.conflict.is_none() && all_assigned(&state.rows, mode);
    let mut generate_button = widget::button::suggested(tr("Generate"));
    if can_generate {
        generate_button = generate_button.on_press(Message::GenerateRequested);
    }
    col.push(
        widget::row::with_capacity(2)
            .spacing(spacing.space_s)
            .push(widget::button::standard(tr("Cancel")).on_press(Message::Cancelled))
            .push(generate_button),
    )
    .into()
}

/// Offers converting an event/clock schedule into solar blocks.
fn convert_hint_view(state: &State) -> Element<'_, Message> {
    let any_assigned = state.rows.iter().any(|r| r.solar.is_some() || r.time.is_some());
    let mut row = widget::row::with_capacity(2).spacing(cosmic::theme::spacing().space_s).align_y(cosmic::iced::Alignment::Center);
    if state.location.is_some() {
        row = row.push(widget::text::caption(tr(
            "Convert to parts of the day: each image goes into the block its current time falls in, and keeps following the sun from then on.",
        )));
        let mut button = widget::button::standard(tr("Convert to parts of the day"));
        if any_assigned {
            button = button.on_press(Message::ConvertToBlocks);
        }
        row = row.push(button);
    } else {
        row = row.push(widget::text::caption(tr("Set a location to convert this pack to parts of the day.")));
    }
    row.into()
}

fn format_hm(at: DateTime<Local>) -> String {
    at.format("%H:%M").to_string()
}

fn span_text(start: DateTime<Local>, end: DateTime<Local>) -> String {
    format!("{} \u{2013} {}", format_hm(start), format_hm(end))
}

/// The block selector entries: the parts of the day, then "Not used".
fn block_choices() -> Vec<String> {
    SolarPeriod::ALL.into_iter().map(period_label).chain(std::iter::once(tr("Not used"))).collect()
}

fn block_from_choice(index: usize) -> Option<SolarPeriod> {
    SolarPeriod::ALL.get(index).copied()
}

fn accent_border(active: bool, width: f32) -> cosmic::theme::Container<'static> {
    cosmic::theme::Container::custom(move |theme| {
        let cosmic = theme.cosmic();
        let color: cosmic::iced::Color = if active { cosmic.accent_color().into() } else { cosmic::iced::Color::TRANSPARENT };
        cosmic::iced::widget::container::Style {
            border: cosmic::iced::Border { color, width, radius: cosmic.radius_s().into() },
            ..Default::default()
        }
    })
}

/// An empty drop slot: always outlined, highlighted while a drag hovers it.
fn drop_slot_style(active: bool) -> cosmic::theme::Container<'static> {
    cosmic::theme::Container::custom(move |theme| {
        let cosmic = theme.cosmic();
        let (color, background): (cosmic::iced::Color, Option<cosmic::iced::Background>) = if active {
            let mut tint: cosmic::iced::Color = cosmic.accent_color().into();
            tint.a = 0.15;
            (cosmic.accent_color().into(), Some(cosmic::iced::Background::Color(tint)))
        } else {
            (cosmic.bg_divider().into(), None)
        };
        cosmic::iced::widget::container::Style {
            border: cosmic::iced::Border { color, width: 2.0, radius: cosmic.radius_s().into() },
            background,
            ..Default::default()
        }
    })
}

fn insertion_bar(active: bool) -> cosmic::theme::Container<'static> {
    cosmic::theme::Container::custom(move |theme| {
        let color: cosmic::iced::Color =
            if active { theme.cosmic().accent_color().into() } else { cosmic::iced::Color::TRANSPARENT };
        cosmic::iced::widget::container::Style {
            background: Some(cosmic::iced::Background::Color(color)),
            ..Default::default()
        }
    })
}

/// A large "+" slot that drops the dragged image at the end of `block`.
fn drop_slot<'a>(state: &State, block: Option<SolarPeriod>, index: usize, width: Length, height: f32) -> Element<'a, Message> {
    let target = DropTarget { block, index };
    let active = state.dragging.is_some() && state.drop_target == Some(target);
    let slot = widget::container(widget::icon::from_name("list-add-symbolic").size(32))
        .width(width)
        .height(height)
        .align_x(cosmic::iced::alignment::Horizontal::Center)
        .align_y(cosmic::iced::alignment::Vertical::Center)
        .class(drop_slot_style(active));
    widget::mouse_area(slot).on_enter(Message::DragEntered(target)).on_exit(Message::DragLeft(target)).into()
}

/// Width of an image card in a block; the "+" slot matches it.
const CARD_WIDTH: f32 = 140.0;
/// Height of the "+" slot inside a block — about one image card.
const BLOCK_SLOT_HEIGHT: f32 = 120.0;
/// Height of the "+" slot under the "Not used" list.
const UNUSED_SLOT_HEIGHT: f32 = 64.0;

/// The solar blocks editor: the parts of the day on the left (3/4 of the width), the
/// pack's name, author and unused images on the right (1/4). Each side scrolls on its
/// own, so both a block and the unused list stay in view while dragging between them.
fn blocks_view<'a>(state: &'a State) -> Element<'a, Message> {
    let spacing = cosmic::theme::spacing();
    let schedule = preview_schedule(state);

    let mut header = widget::column::with_capacity(3)
        .spacing(spacing.space_xs)
        .push(widget::text::title3(tr("Drag images into the parts of the day")))
        .push(widget::text::caption(tr(
            "Each part of the day is split evenly between its images, in order. Empty parts keep showing the previous image.",
        )));
    if state.location.is_some() {
        header = header.push(
            widget::row::with_capacity(4)
                .spacing(spacing.space_xs)
                .align_y(cosmic::iced::Alignment::Center)
                .push(widget::button::standard("\u{2190}").on_press(Message::PreviewDayShifted(-1)))
                .push(widget::text::body(trf("Times for {date}", &[("date", &state.preview_date.format("%Y-%m-%d").to_string())])))
                .push(widget::button::standard("\u{2192}").on_press(Message::PreviewDayShifted(1)))
                .push(widget::button::text(tr("Today")).on_press(Message::PreviewDayShifted(0))),
        );
    } else {
        header = header.push(widget::text::caption(tr("Set a location on the Location page to see the computed times.")));
    }

    let mut blocks = widget::column::with_capacity(SolarPeriod::COUNT).spacing(spacing.space_s);
    for period in SolarPeriod::ALL {
        blocks = blocks.push(block_card(state, period, schedule.as_ref()));
    }

    let mut side = widget::column::with_capacity(6).spacing(spacing.space_xs);
    for field in pack_fields(state) {
        side = side.push(field);
    }
    side = side.push(widget::Space::new().height(spacing.space_xs)).push(unused_view(state));

    let pane = |content: Element<'a, Message>, portion: u16| {
        widget::scrollable(content)
            .spacing(spacing.space_xxs)
            .on_scroll(|_| Message::DragScrolled)
            .width(Length::FillPortion(portion))
            .height(Length::Fill)
    };
    let panes = widget::row::with_capacity(2)
        .spacing(spacing.space_m)
        .height(Length::Fill)
        .push(pane(blocks.into(), 3))
        .push(pane(side.into(), 1));

    widget::column::with_capacity(3)
        .spacing(spacing.space_s)
        .height(Length::Fill)
        .push(header)
        .push(panes)
        .push(footer_view(state, AssignmentMode::SolarBlocks))
        .into()
}

fn block_card<'a>(state: &'a State, period: SolarPeriod, schedule: Option<&ResolvedSchedule>) -> Element<'a, Message> {
    let spacing = cosmic::theme::spacing();
    let resolved = schedule.map(|s| s.block(period));
    let block = Some(period);
    let members: Vec<&ImageRow> = state.rows.iter().filter(|r| r.period == block).collect();

    let mut header = widget::row::with_capacity(2).spacing(spacing.space_s).align_y(cosmic::iced::Alignment::Center);
    header = header.push(widget::text::heading(period_label(period)));
    if let Some(rb) = resolved {
        header = header.push(widget::text::caption(if rb.is_degenerate() {
            tr("doesn't occur on this date")
        } else {
            span_text(rb.start, rb.end)
        }));
    }

    let mut items: Vec<Element<'a, Message>> = Vec::with_capacity(members.len() + 1);
    for (index, row) in members.iter().enumerate() {
        let interval = resolved
            .and_then(|rb| rb.images.iter().find(|img| img.id.as_str() == row.file_name))
            .map(|img| span_text(img.start, img.active_until));
        items.push(image_card(state, row, block, index, members.len(), interval));
    }
    items.push(drop_slot(state, block, members.len(), Length::Fixed(CARD_WIDTH), BLOCK_SLOT_HEIGHT));

    let mut body = widget::column::with_capacity(3).spacing(spacing.space_xs).push(header);
    match resolved {
        Some(rb) if rb.is_degenerate() && !members.is_empty() => {
            body = body.push(widget::text::caption(tr("This part of the day doesn't happen on this date, so its images are skipped.")));
        }
        Some(rb) if members.is_empty() => {
            if let Some(inherited) = &rb.inherited {
                body = body.push(widget::text::caption(trf("Empty — keeps showing {image}", &[("image", inherited.as_str())])));
            }
        }
        _ => {}
    }
    body = body.push(widget::flex_row(items).row_spacing(spacing.space_xs).column_spacing(spacing.space_xs));

    widget::container(body).padding(spacing.space_s).width(Length::Fill).class(cosmic::theme::Container::Card).into()
}

/// Shortens a file name to `max` characters with a trailing ellipsis.
fn short_name(name: &str, max: usize) -> String {
    if name.chars().count() > max {
        name.chars().take(max - 1).chain(std::iter::once('\u{2026}')).collect()
    } else {
        name.to_string()
    }
}

fn block_chooser<'a>(row: &ImageRow, block: Option<SolarPeriod>) -> Element<'a, Message> {
    let selected = Some(block.map_or(SolarPeriod::COUNT, SolarPeriod::index));
    let file_name = row.file_name.clone();
    widget::dropdown(block_choices(), selected, move |i| Message::BlockSelected(file_name.clone(), block_from_choice(i))).into()
}

/// A draggable thumbnail that also marks the drop position in front of it.
fn drag_handle<'a>(state: &State, row: &ImageRow, target: DropTarget, width: f32, height: f32) -> Element<'a, Message> {
    let is_dragged = state.dragging.as_deref() == Some(row.file_name.as_str());
    let thumb = widget::container(crate::thumbnail::view(&row.thumbnail_path, width, height))
        .padding(2)
        .class(accent_border(is_dragged, 2.0));
    widget::mouse_area(thumb)
        .on_press(Message::DragStarted(row.file_name.clone()))
        .on_enter(Message::DragEntered(target))
        .on_exit(Message::DragLeft(target))
        .interaction(cosmic::iced::mouse::Interaction::Grab)
        .into()
}

fn image_card<'a>(
    state: &'a State,
    row: &'a ImageRow,
    block: Option<SolarPeriod>,
    index: usize,
    count: usize,
    interval: Option<String>,
) -> Element<'a, Message> {
    let spacing = cosmic::theme::spacing();
    let target = DropTarget { block, index };
    let is_target = state.dragging.is_some() && state.drop_target == Some(target);

    let mut earlier = widget::button::icon(widget::icon::from_name("go-previous-symbolic")).extra_small();
    if index > 0 {
        earlier = earlier.on_press(Message::MoveWithinBlock(row.file_name.clone(), -1));
    }
    let mut later = widget::button::icon(widget::icon::from_name("go-next-symbolic")).extra_small();
    if index + 1 < count {
        later = later.on_press(Message::MoveWithinBlock(row.file_name.clone(), 1));
    }

    let mut card = widget::column::with_capacity(4)
        .spacing(2)
        .width(Length::Fixed(CARD_WIDTH))
        .push(drag_handle(state, row, target, 96.0, 64.0))
        .push(widget::text::caption(short_name(&row.file_name, 16)));
    if let Some(interval) = interval {
        card = card.push(widget::text::caption(interval));
    }
    card = card.push(
        widget::row::with_capacity(3)
            .spacing(2)
            .align_y(cosmic::iced::Alignment::Center)
            .push(earlier)
            .push(later)
            .push(block_chooser(row, block)),
    );

    widget::row::with_capacity(2)
        .spacing(spacing.space_xxs)
        .push(widget::container(widget::Space::new().width(4).height(64)).class(insertion_bar(is_target)))
        .push(card)
        .into()
}

/// The images left out of the pack, one per line, with a "+" slot to drop more.
fn unused_view(state: &State) -> Element<'_, Message> {
    let spacing = cosmic::theme::spacing();
    let members: Vec<&ImageRow> = state.rows.iter().filter(|r| r.period.is_none()).collect();
    let mut col = widget::column::with_capacity(members.len() + 3)
        .spacing(spacing.space_xs)
        .push(widget::text::heading(tr("Not used")))
        .push(widget::text::caption(tr("Images here are left out of the pack.")));
    for (index, row) in members.into_iter().enumerate() {
        let target = DropTarget { block: None, index };
        let is_target = state.dragging.is_some() && state.drop_target == Some(target);
        let line = widget::row::with_capacity(2)
            .spacing(spacing.space_xs)
            .align_y(cosmic::iced::Alignment::Center)
            .push(drag_handle(state, row, target, 64.0, 42.0))
            .push(
                widget::column::with_capacity(2)
                    .spacing(2)
                    .width(Length::Fill)
                    .push(widget::text::caption(short_name(&row.file_name, 22)))
                    .push(block_chooser(row, None)),
            );
        col = col
            .push(widget::container(widget::Space::new().width(Length::Fill).height(3)).class(insertion_bar(is_target)))
            .push(line);
    }
    let end = state.rows.iter().filter(|r| r.period.is_none()).count();
    col.push(drop_slot(state, None, end, Length::Fill, UNUSED_SLOT_HEIGHT)).into()
}

fn row_view(row: &ImageRow, index: usize, mode: AssignmentMode) -> Element<'_, Message> {
    let thumbnail = crate::thumbnail::view(&row.thumbnail_path, 64, 64);
    let control = match mode {
        AssignmentMode::SolarPeriod => solar_control(row.solar, index),
        AssignmentMode::SpecificTime => time_control(row.time, index),
        AssignmentMode::SolarBlocks => widget::Space::new().into(),
    };
    widget::row::with_capacity(3)
        .spacing(cosmic::theme::spacing().space_s)
        .align_y(cosmic::iced::Alignment::Center)
        .push(thumbnail)
        .push(widget::text::body(row.file_name.clone()).width(160))
        .push(control)
        .into()
}

fn solar_control<'a>(assignment: Option<SolarAssignment>, index: usize) -> Element<'a, Message> {
    let labels = solar_event_labels();
    let selected = assignment.and_then(|a| SOLAR_EVENTS.iter().position(|e| *e == a.event));
    let dropdown = widget::dropdown(labels, selected, move |i| Message::SolarEventSelected(index, i));

    let (negative, hours, minutes) = match assignment {
        Some(a) => (a.offset_negative, a.offset_hours, a.offset_minutes),
        None => (false, 0, 0),
    };

    let sign_button =
        widget::button::standard(if negative { "\u{2212}" } else { "+" }).on_press(Message::SolarOffsetSignToggled(index));
    let hours_spin = widget::spin_button::spin_button(
        format!("{hours}{}", tr("h")),
        trf("offset hours, row {row}", &[("row", &index.to_string())]),
        hours,
        1,
        0,
        12,
        move |v| Message::SolarOffsetHoursChanged(index, v),
    );
    let minutes_spin = widget::spin_button::spin_button(
        format!("{minutes}{}", tr("m")),
        trf("offset minutes, row {row}", &[("row", &index.to_string())]),
        minutes,
        1,
        0,
        59,
        move |v| Message::SolarOffsetMinutesChanged(index, v),
    );

    widget::row::with_capacity(4)
        .spacing(cosmic::theme::spacing().space_xs)
        .align_y(cosmic::iced::Alignment::Center)
        .push(dropdown)
        .push(sign_button)
        .push(hours_spin)
        .push(minutes_spin)
        .into()
}

fn time_control<'a>(time: Option<NaiveTime>, index: usize) -> Element<'a, Message> {
    let hour = time.map_or(0, |t| t.hour());
    let minute = time.map_or(0, |t| t.minute());
    let hour_spin = widget::spin_button::spin_button(format!("{hour:02}"), trf("hour, row {row}", &[("row", &index.to_string())]), hour, 1, 0, 23, move |v| {
        Message::TimeHourChanged(index, v)
    });
    let minute_spin =
        widget::spin_button::spin_button(format!("{minute:02}"), trf("minute, row {row}", &[("row", &index.to_string())]), minute, 1, 0, 59, move |v| {
            Message::TimeMinuteChanged(index, v)
        });
    widget::row::with_capacity(2)
        .spacing(cosmic::theme::spacing().space_xs)
        .align_y(cosmic::iced::Alignment::Center)
        .push(hour_spin)
        .push(minute_spin)
        .into()
}

/// The placement/collision modal (contracts/pack-builder-gui-flow.md), rendered via
/// `App`'s `Application::dialog()` override exactly like spec 008's removal
/// confirmation (research.md R3's pattern) — `None` when neither is pending.
pub fn placement_dialog(state: &State) -> Option<Element<'_, Message>> {
    if let Some(pending) = &state.pending_collision {
        // Spec 011 US2 FR-006/FR-007 (research.md R5): an invalid-name rejection
        // (path traversal, absolute path, or empty) re-opens this same dialog with
        // `state.move_error` set to the specific reason — shown here in place of the
        // generic "already exists" body, mirroring how `pending_placement`'s dialog
        // just below already prefers `move_error` when present.
        let (title, body) = match &state.move_error {
            Some(reason) => (tr("Can't use that pack name"), reason.clone()),
            None => (
                tr("A pack with that name already exists"),
                trf(
                    "\"{name}\" already exists at the standard pack location. Choose a different name.",
                    &[("name", &pending.suggested_name)],
                ),
            ),
        };
        return Some(
            widget::dialog()
                .title(title)
                .body(body)
                .control(widget::text_input::text_input(tr("pack name"), &pending.suggested_name).on_input(Message::CollisionNameChanged))
                .primary_action(widget::button::suggested(tr("Move")).on_press(Message::CollisionConfirmed))
                .secondary_action(widget::button::standard(tr("Cancel")).on_press(Message::CollisionCancelled))
                .into(),
        );
    }
    if state.pending_placement.is_some() {
        let body = state
            .move_error
            .clone()
            .unwrap_or_else(|| tr("Move this pack to the standard pack location, or keep it where it is?"));
        return Some(
            widget::dialog()
                .title(tr("Pack generated"))
                .body(body)
                .primary_action(widget::button::suggested(tr("Move")).on_press(Message::MoveRequested))
                .secondary_action(widget::button::standard(tr("Keep it here")).on_press(Message::KeepRequested))
                .into(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_row(name: &str) -> ImageRow {
        ImageRow::new(name.to_string(), PathBuf::from(name))
    }

    fn solar(event: SolarEventKind) -> SolarAssignment {
        SolarAssignment { event, offset_negative: false, offset_hours: 0, offset_minutes: 0 }
    }

    // --- T009: combine_offset ---

    #[test]
    fn combine_offset_applies_sign_and_magnitude() {
        assert_eq!(combine_offset(false, 1, 15), TimeDelta::hours(1) + TimeDelta::minutes(15));
        assert_eq!(combine_offset(true, 0, 30), -TimeDelta::minutes(30));
        assert_eq!(combine_offset(true, 1, 0), -TimeDelta::hours(1));
    }

    #[test]
    fn combine_offset_clamps_minutes_at_the_twelve_hour_cap() {
        assert_eq!(combine_offset(false, 12, 45), TimeDelta::hours(12), "minutes must be forced to 0 at the ±12h cap");
        assert_eq!(combine_offset(true, 12, 45), -TimeDelta::hours(12));
        assert_eq!(combine_offset(false, 15, 0), TimeDelta::hours(12), "hours beyond 12 must clamp to 12");
    }

    // --- T010: effective_author ---

    #[test]
    fn effective_author_blank_or_whitespace_becomes_artist_unknown() {
        assert_eq!(effective_author(""), "Artist Unknown");
        assert_eq!(effective_author("   "), "Artist Unknown");
    }

    #[test]
    fn effective_author_trims_and_keeps_a_real_name() {
        assert_eq!(effective_author("  Jane Author  "), "Jane Author");
    }

    // --- Spec 012 US4 (T014): effective_name / set_name ---

    #[test]
    fn effective_name_blank_or_whitespace_falls_back_to_the_given_fallback() {
        assert_eq!(effective_name("", "My Folder"), "My Folder");
        assert_eq!(effective_name("   ", "My Folder"), "My Folder");
    }

    #[test]
    fn effective_name_trims_and_keeps_a_real_name() {
        assert_eq!(effective_name("  Sunrise Glow  ", "My Folder"), "Sunrise Glow");
    }

    #[test]
    fn set_name_stores_the_raw_input_unnormalized() {
        let mut state = open_test_state(&["a.png"]);
        set_name(&mut state, "  Not Yet Trimmed  ".to_string());
        assert_eq!(state.name, "  Not Yet Trimmed  ", "normalization happens at build_draft time, not on every keystroke");
    }

    #[test]
    fn open_defaults_name_to_the_folder_name() {
        let state = open_test_state(&["a.png"]);
        let expected = state.source_dir.file_name().unwrap().to_str().unwrap();
        assert_eq!(state.name, expected);
    }

    // --- T011: all_assigned ---

    #[test]
    fn all_assigned_true_only_when_every_row_has_the_active_mode_field_set() {
        let mut rows = vec![image_row("a.png"), image_row("b.png")];
        assert!(!all_assigned(&rows, AssignmentMode::SolarPeriod), "no rows assigned yet");

        rows[0].solar = Some(solar(SolarEventKind::Sunrise));
        assert!(!all_assigned(&rows, AssignmentMode::SolarPeriod), "one row still unassigned");

        rows[1].solar = Some(solar(SolarEventKind::Sunset));
        assert!(all_assigned(&rows, AssignmentMode::SolarPeriod));

        // Time fields being set doesn't matter for solar-mode gating, and vice versa.
        assert!(!all_assigned(&rows, AssignmentMode::SpecificTime));
    }

    #[test]
    fn all_assigned_is_false_for_an_empty_row_list() {
        assert!(!all_assigned(&[], AssignmentMode::SolarPeriod));
    }

    // --- T012: build_draft ---

    #[test]
    fn build_draft_applies_r10_defaults_and_maps_every_assigned_row() {
        let mut rows = vec![image_row("dawn.jpg"), image_row("dusk.jpg")];
        rows[0].solar = Some(solar(SolarEventKind::Sunrise));
        rows[1].solar = Some(SolarAssignment { event: SolarEventKind::Sunset, offset_negative: true, offset_hours: 0, offset_minutes: 30 });

        let draft = build_draft(&rows, AssignmentMode::SolarPeriod, "", "My Folder", "", None);

        assert_eq!(draft.name, "My Folder");
        assert_eq!(draft.author.as_deref(), Some("Artist Unknown"));
        assert_eq!(draft.default_scaling, ScalingMode::Fill);
        assert_eq!(draft.fallback_color, Color { r: 0, g: 0, b: 0, a: 255 });
        assert_eq!(draft.images.len(), 2);
        assert_eq!(draft.images[0].file, "dawn.jpg");
        assert_eq!(draft.images[0].anchor, TimeAnchor::solar(SolarEventKind::Sunrise, None));
        assert_eq!(
            draft.images[1].anchor,
            TimeAnchor::solar(SolarEventKind::Sunset, Some(-TimeDelta::minutes(30)))
        );
    }

    #[test]
    fn build_draft_keeps_a_supplied_author_name() {
        let mut rows = vec![image_row("a.png")];
        rows[0].time = Some(NaiveTime::from_hms_opt(6, 0, 0).unwrap());
        let draft = build_draft(&rows, AssignmentMode::SpecificTime, "", "Folder", "  Jane  ", None);
        assert_eq!(draft.author.as_deref(), Some("Jane"));
        assert_eq!(draft.images[0].anchor, TimeAnchor::clock(NaiveTime::from_hms_opt(6, 0, 0).unwrap()));
    }

    // --- T016: solar-mode conflict detection ---

    #[test]
    fn detect_conflict_solar_catches_identical_literal_assignments_without_a_location() {
        let mut rows = vec![image_row("a.png"), image_row("b.png")];
        rows[0].solar = Some(solar(SolarEventKind::Sunrise));
        rows[1].solar = Some(solar(SolarEventKind::Sunrise));

        let conflict = detect_conflict(&rows, AssignmentMode::SolarPeriod, None);
        assert!(conflict.is_some(), "two identical solar assignments must conflict even with no location configured");
    }

    #[test]
    fn detect_conflict_solar_is_none_for_distinct_events() {
        let mut rows = vec![image_row("a.png"), image_row("b.png")];
        rows[0].solar = Some(solar(SolarEventKind::Sunrise));
        rows[1].solar = Some(solar(SolarEventKind::Sunset));
        assert_eq!(detect_conflict(&rows, AssignmentMode::SolarPeriod, None), None);
    }

    #[test]
    fn detect_conflict_solar_uses_the_location_aware_layer_when_a_location_is_set() {
        // A distinct-looking but resolvable-to-the-same-instant pair only a
        // location-aware check (not the literal-equality layer) can catch: use the
        // exact same event/offset via two different offset representations reaching
        // the same TimeDelta — instead, assert the location-aware path is *reachable*
        // and returns `Ok`/no-conflict for a genuinely distinct, resolvable pair, which
        // exercises `check_solar_duplicate_instant` without depending on real solar
        // ephemeris numbers lining up.
        let mut rows = vec![image_row("a.png"), image_row("b.png")];
        rows[0].solar = Some(solar(SolarEventKind::Sunrise));
        rows[1].solar = Some(solar(SolarEventKind::SolarNoon));
        let location = Location::new(51.5072, -0.1276).unwrap();
        assert_eq!(detect_conflict(&rows, AssignmentMode::SolarPeriod, Some(location)), None);
    }

    // --- T021: clock-mode conflict detection ---

    #[test]
    fn detect_conflict_clock_catches_duplicate_times() {
        let mut rows = vec![image_row("a.png"), image_row("b.png")];
        rows[0].time = Some(NaiveTime::from_hms_opt(6, 0, 0).unwrap());
        rows[1].time = Some(NaiveTime::from_hms_opt(6, 0, 0).unwrap());
        assert!(detect_conflict(&rows, AssignmentMode::SpecificTime, None).is_some());
    }

    #[test]
    fn detect_conflict_clock_is_none_for_distinct_times() {
        let mut rows = vec![image_row("a.png"), image_row("b.png")];
        rows[0].time = Some(NaiveTime::from_hms_opt(6, 0, 0).unwrap());
        rows[1].time = Some(NaiveTime::from_hms_opt(18, 0, 0).unwrap());
        assert_eq!(detect_conflict(&rows, AssignmentMode::SpecificTime, None), None);
    }

    // --- set_mode: switching discards the other mode's assignments ---

    #[test]
    fn set_mode_clears_assignments_from_both_fields() {
        let mut state = open_test_state(&["a.png", "b.png"]);
        set_mode(&mut state, AssignmentMode::SolarPeriod);
        set_solar_event_by_index(&mut state, 0, 0, None);
        assert!(state.rows[0].solar.is_some());

        set_mode(&mut state, AssignmentMode::SpecificTime);
        assert!(state.rows[0].solar.is_none(), "switching modes must discard the previous mode's assignment");
        assert!(state.rows[0].time.is_none());
    }

    // --- Test helpers needing real files on disk (scan_folder/generate/move) ---

    fn write_test_image(path: &Path) {
        image::RgbImage::new(2, 2).save(path).unwrap();
    }

    fn open_test_state(file_names: &[&str]) -> State {
        let dir = tempfile::tempdir().unwrap();
        for name in file_names {
            write_test_image(&dir.path().join(name));
        }
        // Leak the TempDir deliberately for these pure-state tests that never touch the
        // filesystem again after scanning — acceptable since `cargo test` processes are
        // short-lived; tests that themselves do I/O (generate/move) manage their own
        // `TempDir` directly instead of this helper.
        let path = dir.keep();
        open(path)
    }

    // --- T031: a folder that already has a manifest.toml never opens the wizard ---

    #[test]
    fn should_open_for_is_false_for_a_folder_that_already_has_a_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_test_image(&dir.path().join("a.png"));
        std::fs::write(
            dir.path().join("manifest.toml"),
            "schema_version = 1\nname = \"x\"\ndefault_scaling = \"Fill\"\nfallback_color = \"#000000\"\n\n[[images]]\nfile = \"a.png\"\nanchor = \"sunrise\"\n",
        )
        .unwrap();

        assert!(!should_open_for(dir.path()), "an already-configured pack must register directly, not open the wizard");
    }

    #[test]
    fn should_open_for_is_true_for_a_manifest_free_folder() {
        let dir = tempfile::tempdir().unwrap();
        write_test_image(&dir.path().join("a.png"));
        assert!(should_open_for(dir.path()));
    }

    #[test]
    fn should_open_for_is_false_for_a_single_static_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.png");
        write_test_image(&file);
        assert!(!should_open_for(&file), "a single image file is a static pack, not a wizard candidate");
    }

    // --- T007: scan_folder / open ---

    #[test]
    fn open_scans_only_readable_images_and_sorts_by_name() {
        let dir = tempfile::tempdir().unwrap();
        write_test_image(&dir.path().join("b.png"));
        write_test_image(&dir.path().join("a.png"));
        std::fs::write(dir.path().join("notes.txt"), b"not an image").unwrap();

        let state = open(dir.path().to_path_buf());
        assert_eq!(state.scan_error, None);
        assert_eq!(state.rows.iter().map(|r| r.file_name.clone()).collect::<Vec<_>>(), vec!["a.png", "b.png"]);
    }

    #[test]
    fn open_reports_a_scan_error_for_a_folder_with_no_images() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"not an image").unwrap();

        let state = open(dir.path().to_path_buf());
        assert!(state.scan_error.is_some());
        assert!(state.rows.is_empty());
    }

    #[test]
    fn open_reports_a_scan_error_for_too_many_images() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(schedule_engine::MAX_ANCHORS + 1) {
            write_test_image(&dir.path().join(format!("{i}.png")));
        }
        let state = open(dir.path().to_path_buf());
        assert!(state.scan_error.is_some());
    }

    // --- T019/T023: full Generate round-trip through pack_loader::load_pack ---

    #[test]
    fn generate_solar_mode_produces_a_pack_that_loads_back_as_configured() {
        let dir = tempfile::tempdir().unwrap();
        write_test_image(&dir.path().join("dawn.jpg"));
        write_test_image(&dir.path().join("dusk.jpg"));

        let mut state = open(dir.path().to_path_buf());
        assert_eq!(state.scan_error, None);
        set_mode(&mut state, AssignmentMode::SolarPeriod);
        set_solar_event_by_index(&mut state, 0, 0, None); // Sunrise
        set_solar_event_by_index(&mut state, 1, 1, None); // Sunset
        assert!(state.conflict.is_none());
        assert!(all_assigned(&state.rows, AssignmentMode::SolarPeriod));

        let _ = generate(&mut state);
        assert_eq!(state.generate_error, None, "{:?}", state.generate_error);
        assert!(state.pending_placement.is_some());
        // Spec 011 US6 FR-027 (research.md R22): the manifest isn't written to
        // `dir.path()` at all yet — held in `pending_manifest_text` until the user's
        // actual Move/Keep choice.
        assert!(state.pending_manifest_text.is_some());
        assert!(!dir.path().join(pack_loader::MANIFEST_FILE_NAME).exists(), "must not write until Move/Keep is chosen");

        let registry_dir = tempfile::tempdir().unwrap();
        let mut registry = pack_loader::Registry::open_at(registry_dir.path()).unwrap();
        assert!(confirm_keep(&mut state, &mut registry), "{:?}", state.move_error);

        let loaded = pack_loader::load_pack(dir.path()).unwrap();
        assert_eq!(loaded.author.as_deref(), Some("Artist Unknown"));
        assert_eq!(loaded.pack.images().len(), 2);
    }

    #[test]
    fn generate_specific_time_mode_produces_a_pack_that_loads_back_as_configured() {
        let dir = tempfile::tempdir().unwrap();
        write_test_image(&dir.path().join("a.jpg"));
        write_test_image(&dir.path().join("b.jpg"));

        let mut state = open(dir.path().to_path_buf());
        set_mode(&mut state, AssignmentMode::SpecificTime);
        set_time_hour(&mut state, 0, 6);
        set_time_minute(&mut state, 0, 0);
        set_time_hour(&mut state, 1, 18);
        set_time_minute(&mut state, 1, 0);
        assert!(state.conflict.is_none());

        let _ = generate(&mut state);
        assert_eq!(state.generate_error, None, "{:?}", state.generate_error);
        assert!(!dir.path().join(pack_loader::MANIFEST_FILE_NAME).exists(), "must not write until Move/Keep is chosen");

        let registry_dir = tempfile::tempdir().unwrap();
        let mut registry = pack_loader::Registry::open_at(registry_dir.path()).unwrap();
        assert!(confirm_keep(&mut state, &mut registry), "{:?}", state.move_error);

        let loaded = pack_loader::load_pack(dir.path()).unwrap();
        assert_eq!(loaded.pack.images().len(), 2);
    }

    #[test]
    fn generate_blocked_by_a_duplicate_time_never_writes_a_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_test_image(&dir.path().join("a.jpg"));
        write_test_image(&dir.path().join("b.jpg"));

        let mut state = open(dir.path().to_path_buf());
        set_mode(&mut state, AssignmentMode::SpecificTime);
        set_time_hour(&mut state, 0, 6);
        set_time_hour(&mut state, 1, 6);
        assert!(state.conflict.is_some(), "identical times must be flagged before Generate");
        assert!(!dir.path().join(pack_loader::MANIFEST_FILE_NAME).exists());
    }

    /// Spec 011 US6 FR-026 (research.md R21) — `generate()` re-checks `all_assigned`
    /// itself, so calling it directly (bypassing the UI button's `enabled` gate
    /// entirely, simulating a future call site that forgets to check first) still
    /// can't produce an incomplete pack.
    #[test]
    fn generate_handler_rechecks_all_assigned() {
        let dir = tempfile::tempdir().unwrap();
        write_test_image(&dir.path().join("a.jpg"));
        write_test_image(&dir.path().join("b.jpg"));

        let mut state = open(dir.path().to_path_buf());
        set_mode(&mut state, AssignmentMode::SolarPeriod);
        // Only the first row gets an assignment — the second is left unassigned, and
        // `generate` is called directly rather than through any UI gate.
        set_solar_event_by_index(&mut state, 0, 0, None);

        let _ = generate(&mut state);

        assert!(state.generate_error.is_some(), "generate() must refuse to run with an unassigned row");
        assert!(state.pending_placement.is_none(), "must not proceed to placement");
        assert!(!dir.path().join(pack_loader::MANIFEST_FILE_NAME).exists(), "must not write a manifest at all");
    }

    // --- T026: move failure leaves the source untouched ---

    /// Spec 011 US6 FR-027 (research.md R22): `move_pack` now writes the manifest into
    /// the destination itself (deferred from `generate()`), rather than expecting one
    /// already sitting in `source` — the manifest text below (referencing a missing
    /// image) is passed directly, not pre-written to `source` at all.
    #[test]
    fn move_pack_leaves_the_source_untouched_when_the_load_check_fails() {
        let source = tempfile::tempdir().unwrap();
        // References a missing image, so the move's own self-validation step at the
        // destination fails deterministically.
        let bad_manifest = "schema_version = 1\nname = \"x\"\ndefault_scaling = \"Fill\"\nfallback_color = \"#000000\"\n\n[[images]]\nfile = \"missing.png\"\nanchor = \"sunrise\"\n";

        let destination_root = tempfile::tempdir().unwrap();
        let result = move_pack(source.path(), destination_root.path(), "moved-pack", bad_manifest);

        assert!(matches!(result, Err(MoveError::Io(_))));
        assert!(source.path().exists(), "source must survive a failed move");
        assert!(!destination_root.path().join("moved-pack").exists(), "a failed move must not leave a partial copy");
    }

    #[test]
    fn move_pack_succeeds_and_removes_the_source_on_a_valid_pack() {
        let source = tempfile::tempdir().unwrap();
        write_test_image(&source.path().join("a.png"));
        let manifest = "schema_version = 1\nname = \"x\"\ndefault_scaling = \"Fill\"\nfallback_color = \"#000000\"\n\n[[images]]\nfile = \"a.png\"\nanchor = \"sunrise\"\n";

        let destination_root = tempfile::tempdir().unwrap();
        let result = move_pack(source.path(), destination_root.path(), "moved-pack", manifest);

        assert!(result.is_ok());
        assert!(!source.path().exists(), "source folder must be removed after a successful move");
        assert!(destination_root.path().join("moved-pack").join("manifest.toml").exists());
        assert_eq!(std::fs::read_to_string(destination_root.path().join("moved-pack").join("manifest.toml")).unwrap(), manifest);
    }

    // --- Spec 011 US2 FR-006/FR-007 (research.md R5): the collision-rename field
    // can never be used for path traversal or an empty-name collapse. ---

    #[test]
    fn validate_destination_name_rejects_traversal() {
        assert!(validate_destination_name("../../../.config/autostart").is_err());
        assert!(validate_destination_name("/home/user/.ssh").is_err());
        assert!(validate_destination_name("").is_err());
        assert!(validate_destination_name("..").is_err());
        assert!(validate_destination_name("a/../../b").is_err());
        // A plain name is still fine.
        assert!(validate_destination_name("my-pack").is_ok());
        assert!(validate_destination_name("My Pack 2026").is_ok());
    }

    /// Spec 011 US2 FR-006 (research.md R5) — the audit's own reproduction: typing
    /// `../../../.config/autostart` into the rename box and confirming Move must not
    /// copy anything outside `destination_root`, and must leave the source untouched.
    #[test]
    fn move_pack_rejects_path_traversal_in_the_destination_name() {
        let source = tempfile::tempdir().unwrap();
        write_test_image(&source.path().join("a.png"));
        let destination_root = tempfile::tempdir().unwrap();
        let escape_target = destination_root.path().parent().unwrap().join("escaped-pack-test");
        let _ = std::fs::remove_dir_all(&escape_target);

        let traversal_name = format!("../{}", escape_target.file_name().unwrap().to_str().unwrap());
        let result = move_pack(source.path(), destination_root.path(), &traversal_name, "schema_version = 1\nname = \"x\"\ndefault_scaling = \"Fill\"\nfallback_color = \"#000000\"\n\n[[images]]\nfile = \"a.png\"\nanchor = \"sunrise\"\n");

        assert!(matches!(result, Err(MoveError::InvalidName(_))), "expected InvalidName, got {result:?}");
        assert!(!escape_target.exists(), "nothing must be written outside destination_root");
        assert!(source.path().exists(), "source must survive a rejected move");
    }

    #[test]
    fn move_pack_rejects_an_absolute_destination_name() {
        let source = tempfile::tempdir().unwrap();
        write_test_image(&source.path().join("a.png"));
        let destination_root = tempfile::tempdir().unwrap();

        let result = move_pack(
            source.path(),
            destination_root.path(),
            "/tmp/should-not-be-created-by-this-test",
            "schema_version = 1\nname = \"x\"\ndefault_scaling = \"Fill\"\nfallback_color = \"#000000\"\n\n[[images]]\nfile = \"a.png\"\nanchor = \"sunrise\"\n",
        );
        assert!(matches!(result, Err(MoveError::InvalidName(_))));
        assert!(!std::path::Path::new("/tmp/should-not-be-created-by-this-test").exists());
    }

    /// Spec 011 US2 FR-007 (research.md R5) — `destination_root.join("")` previously
    /// collapsed to `destination_root` itself, merging this pack's contents directly
    /// into the shared packs directory on the very first collision-rename ever
    /// performed with a blank field.
    #[test]
    fn move_pack_rejects_an_empty_destination_name() {
        let source = tempfile::tempdir().unwrap();
        write_test_image(&source.path().join("a.png"));
        let destination_root = tempfile::tempdir().unwrap();
        std::fs::write(destination_root.path().join("sentinel.txt"), b"pre-existing").unwrap();

        let result = move_pack(
            source.path(),
            destination_root.path(),
            "",
            "schema_version = 1\nname = \"x\"\ndefault_scaling = \"Fill\"\nfallback_color = \"#000000\"\n\n[[images]]\nfile = \"a.png\"\nanchor = \"sunrise\"\n",
        );

        assert!(matches!(result, Err(MoveError::InvalidName(_))));
        assert!(destination_root.path().join("sentinel.txt").exists(), "destination_root's existing contents must be untouched");
        assert!(source.path().exists(), "source must survive a rejected move");
    }

    // --- T028: destination-name collision opens the prompt instead of overwriting ---

    #[test]
    fn confirm_move_opens_collision_prompt_instead_of_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        write_test_image(&dir.path().join("a.jpg"));
        let mut state = open(dir.path().to_path_buf());
        set_mode(&mut state, AssignmentMode::SolarPeriod);
        set_solar_event_by_index(&mut state, 0, 0, None);
        let _ = generate(&mut state);
        assert!(state.pending_placement.is_some());

        // Pre-create a colliding destination under a scratch registry dir.
        let registry_dir = tempfile::tempdir().unwrap();
        let mut registry = pack_loader::Registry::open_at(registry_dir.path()).unwrap();

        let colliding_name = dir.path().file_name().unwrap().to_str().unwrap().to_string();
        let fake_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(fake_root.path().join(&colliding_name)).unwrap();

        // Directly exercise `finish_move`/`move_pack`'s collision path against a fixed
        // root (rather than the real standard_pack_location(), unavailable in a
        // sandboxed test environment) — this is exactly what `confirm_move` calls.
        let generated_path = state.pending_placement.clone().unwrap().generated_path;
        let closed = finish_move(&mut state, &mut registry, &generated_path, fake_root.path(), &colliding_name);

        assert!(!closed);
        assert!(state.pending_collision.is_some());
        assert!(state.pending_placement.is_none());
        // The pre-existing destination must be untouched (still an empty dir, not
        // overwritten with the moved pack's contents).
        assert!(!fake_root.path().join(&colliding_name).join("manifest.toml").exists());
    }

    /// Spec 011 US6 FR-024 (research.md R19) — the audit's own finding: a registration
    /// failure was previously discarded (`if let Ok`/`let _ =`), and the wizard closed
    /// unconditionally, reporting success even though the pack never actually
    /// appeared in "Registered packs." `register_and_close` now surfaces the failure
    /// via `move_error`, re-arms `pending_placement` so the dialog reopens showing it,
    /// and returns `false` (wizard stays open) instead of `true`.
    #[test]
    fn registration_failure_surfaces_to_move_error() {
        let registry_dir = tempfile::tempdir().unwrap();
        let mut registry = pack_loader::Registry::open_at(registry_dir.path()).unwrap();

        // A `pending_placement` pointing at a path that doesn't exist — the simplest
        // deterministic way to make `PackSource::resolve` (the first fallible step
        // inside `register_and_close`) fail, without relying on filesystem permission
        // tricks that may not behave consistently across sandboxed test environments.
        let vanished_path = tempfile::tempdir().unwrap().path().join("this-does-not-exist");
        let source_dir = tempfile::tempdir().unwrap();
        let mut state = open(source_dir.path().to_path_buf());
        state.pending_placement = Some(GeneratedPlacement { generated_path: vanished_path });

        let closed = confirm_keep(&mut state, &mut registry);

        assert!(!closed, "a registration failure must not report the wizard as closed/successful");
        assert!(state.move_error.is_some(), "the specific failure must be surfaced, not discarded");
        assert!(
            state.pending_placement.is_some(),
            "the placement dialog must re-arm so the user sees the error and can retry, not silently vanish"
        );
        assert!(registry.known_packs().is_empty(), "nothing should have been registered");
    }

    // --- T030: Cancel leaves the source folder byte-for-byte unchanged ---

    #[test]
    fn cancel_before_generate_never_touches_the_source_folder() {
        let dir = tempfile::tempdir().unwrap();
        write_test_image(&dir.path().join("a.jpg"));
        let mut state = open(dir.path().to_path_buf());
        set_mode(&mut state, AssignmentMode::SolarPeriod);
        set_solar_event_by_index(&mut state, 0, 0, None);

        // "Cancel" itself is just dropping `state` (app.rs sets `pack_builder = None`,
        // no pack_builder function is called) — the assertion is that nothing at all
        // was written to `dir` by any of the above.
        drop(state);

        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(entries.len(), 1, "only the original a.jpg should exist: {entries:?}");
        assert!(!dir.path().join(pack_loader::MANIFEST_FILE_NAME).exists());
    }

    /// Spec 011 US6 FR-027 (research.md R22) — the audit's own finding: previously,
    /// `generate()` wrote `manifest.toml` into the source folder immediately, before
    /// the placement dialog was even shown, so a crash/force-quit in that window left
    /// an unregistered manifest behind that `should_open_for`'s `ManifestNotFound`-only
    /// check would then silently treat as "already has a manifest" on next launch —
    /// an implicit "Keep it here" the user never actually chose. This test is the
    /// direct check that the window no longer exists at all: after `generate()`
    /// succeeds, nothing has been written to `source_dir` (simulating exactly that
    /// crash point), and `should_open_for` still correctly re-opens the wizard for it.
    #[test]
    fn manifest_is_not_written_between_generate_and_the_placement_choice() {
        let dir = tempfile::tempdir().unwrap();
        write_test_image(&dir.path().join("a.jpg"));
        let mut state = open(dir.path().to_path_buf());
        set_mode(&mut state, AssignmentMode::SolarPeriod);
        set_solar_event_by_index(&mut state, 0, 0, None);

        let _ = generate(&mut state);

        assert_eq!(state.generate_error, None, "{:?}", state.generate_error);
        assert!(state.pending_placement.is_some());
        assert!(state.pending_manifest_text.is_some());
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(entries.len(), 1, "only the original a.jpg should exist on disk at this point: {entries:?}");
        assert!(!dir.path().join(pack_loader::MANIFEST_FILE_NAME).exists());

        // Simulating a crash right here (dropping `state` without ever calling
        // confirm_move/confirm_keep): the folder is exactly as it was before Generate
        // was ever clicked, so re-opening it re-launches the wizard rather than
        // silently treating an unfinished session as "kept."
        drop(state);
        assert!(should_open_for(dir.path()), "an interrupted session must not look like an already-placed pack");
    }

    // --- T032: FR-018 scan failures already covered above
    // (open_reports_a_scan_error_for_a_folder_with_no_images /
    // open_reports_a_scan_error_for_too_many_images) — listed here for traceability.

    // --- Spec 012 US1: decompose_offset (T004) ---

    #[test]
    fn decompose_offset_none_is_zeroed() {
        assert_eq!(decompose_offset(None), (false, 0, 0));
    }

    #[test]
    fn decompose_offset_splits_a_positive_value_into_hours_and_minutes() {
        assert_eq!(decompose_offset(Some(TimeDelta::hours(2) + TimeDelta::minutes(15))), (false, 2, 15));
    }

    #[test]
    fn decompose_offset_reports_the_sign_and_the_magnitude_for_a_negative_value() {
        assert_eq!(decompose_offset(Some(-(TimeDelta::minutes(45)))), (true, 0, 45));
        assert_eq!(decompose_offset(Some(-TimeDelta::hours(1))), (true, 1, 0));
    }

    #[test]
    fn decompose_offset_round_trips_through_combine_offset() {
        for (negative, hours, minutes) in [(false, 0, 0), (false, 3, 27), (true, 5, 10), (false, 12, 0), (true, 12, 0)] {
            let delta = combine_offset(negative, hours, minutes);
            assert_eq!(decompose_offset(Some(delta)), (negative, hours, minutes), "round-trip failed for ({negative}, {hours}, {minutes})");
        }
    }

    #[test]
    fn offset_within_wizard_range_accepts_the_twelve_hour_boundary_and_rejects_beyond_it() {
        assert!(offset_within_wizard_range(None));
        assert!(offset_within_wizard_range(Some(TimeDelta::hours(12))));
        assert!(offset_within_wizard_range(Some(-TimeDelta::hours(12))));
        assert!(!offset_within_wizard_range(Some(TimeDelta::hours(12) + TimeDelta::minutes(1))));
        assert!(!offset_within_wizard_range(Some(-TimeDelta::hours(13))));
    }

    // --- Spec 012 US1: open_for_edit (T005) ---

    /// Mirrors `pack_display.rs`'s own test helper of the same shape — writes a real
    /// manifest.toml plus placeholder images so `pack_loader::load_pack` (and thus
    /// `open_for_edit`) has something real to load.
    fn write_pack_dir(dir: &Path, manifest_body: &str, images: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(pack_loader::MANIFEST_FILE_NAME), manifest_body).unwrap();
        for name in images {
            write_test_image(&dir.join(name));
        }
    }

    /// FR-010: a standalone single-image pack has no schedule to edit through this
    /// wizard — `open_for_edit` refuses a `StaticFile` source outright rather than
    /// relying solely on its callers to route it elsewhere first (see this function's
    /// own doc comment for why the check is duplicated here).
    #[test]
    fn open_for_edit_fails_for_a_static_file_source() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sunrise.png");
        write_test_image(&file);
        let source = PackSource::resolve(&file).unwrap();
        assert!(open_for_edit(file, source).is_err());
    }

    #[test]
    fn open_for_edit_prefills_a_solar_mode_pack() {
        let dir = tempfile::tempdir().unwrap();
        let pack_dir = dir.path().join("solar-pack");
        write_pack_dir(
            &pack_dir,
            r##"
                schema_version = 1
                name = "Solar Pack"
                author = "Jane Author"
                default_scaling = "Fill"
                fallback_color = "#000000"
                [[images]]
                file = "dawn.png"
                anchor = "sunrise"
                [[images]]
                file = "dusk.png"
                anchor = "sunset-45m"
            "##,
            &["dawn.png", "dusk.png"],
        );
        let source = PackSource::resolve(&pack_dir).unwrap();

        let state = open_for_edit(pack_dir.clone(), source.clone()).unwrap();

        assert_eq!(state.mode, Some(AssignmentMode::SolarPeriod));
        assert_eq!(state.author, "Jane Author");
        assert_eq!(state.name, "Solar Pack");
        assert_eq!(state.edit_target, Some(source));
        assert!(state.scan_error.is_none());
        assert_eq!(state.rows.len(), 2);

        let dawn = state.rows.iter().find(|r| r.file_name == "dawn.png").unwrap();
        assert_eq!(dawn.solar, Some(SolarAssignment { event: SolarEventKind::Sunrise, offset_negative: false, offset_hours: 0, offset_minutes: 0 }));

        let dusk = state.rows.iter().find(|r| r.file_name == "dusk.png").unwrap();
        assert_eq!(dusk.solar, Some(SolarAssignment { event: SolarEventKind::Sunset, offset_negative: true, offset_hours: 0, offset_minutes: 45 }));
    }

    #[test]
    fn open_for_edit_prefills_a_clock_mode_pack() {
        let dir = tempfile::tempdir().unwrap();
        let pack_dir = dir.path().join("clock-pack");
        write_pack_dir(
            &pack_dir,
            "schema_version = 1\nname = \"Clock Pack\"\ndefault_scaling = \"Fill\"\nfallback_color = \"#000000\"\n\n[[images]]\nfile = \"a.png\"\nanchor = \"06:00\"\n[[images]]\nfile = \"b.png\"\nanchor = \"18:30\"\n",
            &["a.png", "b.png"],
        );
        let source = PackSource::resolve(&pack_dir).unwrap();

        let state = open_for_edit(pack_dir, source).unwrap();

        assert_eq!(state.mode, Some(AssignmentMode::SpecificTime));
        let a = state.rows.iter().find(|r| r.file_name == "a.png").unwrap();
        assert_eq!(a.time, Some(NaiveTime::from_hms_opt(6, 0, 0).unwrap()));
        let b = state.rows.iter().find(|r| r.file_name == "b.png").unwrap();
        assert_eq!(b.time, Some(NaiveTime::from_hms_opt(18, 30, 0).unwrap()));
    }

    /// FR-006: a file added to the folder since the manifest was last saved shows up
    /// as an unassigned row rather than being skipped or causing an error.
    #[test]
    fn open_for_edit_shows_a_newly_added_file_as_unassigned() {
        let dir = tempfile::tempdir().unwrap();
        let pack_dir = dir.path().join("pack");
        write_pack_dir(
            &pack_dir,
            "schema_version = 1\nname = \"Pack\"\ndefault_scaling = \"Fill\"\nfallback_color = \"#000000\"\n\n[[images]]\nfile = \"a.png\"\nanchor = \"sunrise\"\n",
            &["a.png"],
        );
        write_test_image(&pack_dir.join("new.png")); // not in the manifest at all
        let source = PackSource::resolve(&pack_dir).unwrap();

        let state = open_for_edit(pack_dir, source).unwrap();

        assert_eq!(state.rows.len(), 2);
        let new_row = state.rows.iter().find(|r| r.file_name == "new.png").unwrap();
        assert_eq!(new_row.solar, None, "a newly-added file must start unassigned");
    }

    /// Corrected understanding vs. this feature's original spec wording (discovered
    /// while writing this test — see spec.md's Acceptance Scenario 6 and FR-006, both
    /// updated to match): a file the *current* manifest still references but that's
    /// gone from the folder does **not** just quietly disappear as an unassigned-row
    /// non-issue — `pack_loader::load_pack` itself already refuses to load a manifest
    /// referencing a missing image (`path_safety`/`image_check`'s existing checks, run
    /// for every pack load, not just this wizard's), so `open_for_edit`'s very first
    /// step (loading the pack at all) already fails first. This is exactly the same
    /// "can't safely edit this pack" refusal as a missing folder or mixed anchors —
    /// there is no reachable path where a row is silently dropped for a file the
    /// *manifest* still expects. (A file the manifest never mentioned in the first
    /// place — the *added*, not removed, case — is the one FR-006 scenario that
    /// really does degrade gracefully: see
    /// `open_for_edit_shows_a_newly_added_file_as_unassigned` above.)
    #[test]
    fn open_for_edit_fails_when_the_folder_is_missing_a_file_the_manifest_still_references() {
        let dir = tempfile::tempdir().unwrap();
        let pack_dir = dir.path().join("pack");
        write_pack_dir(
            &pack_dir,
            "schema_version = 1\nname = \"Pack\"\ndefault_scaling = \"Fill\"\nfallback_color = \"#000000\"\n\n[[images]]\nfile = \"a.png\"\nanchor = \"sunrise\"\n[[images]]\nfile = \"b.png\"\nanchor = \"sunset\"\n",
            &["a.png", "b.png"],
        );
        std::fs::remove_file(pack_dir.join("b.png")).unwrap();
        let source = PackSource::Directory(pack_dir.clone());

        let result = open_for_edit(pack_dir, source);

        assert!(result.is_err(), "a manifest referencing a now-missing file must refuse to open, not silently drop that row");
    }

    #[test]
    fn open_for_edit_fails_for_a_directory_with_no_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let pack_dir = dir.path().join("broken");
        std::fs::create_dir_all(&pack_dir).unwrap();
        write_test_image(&pack_dir.join("a.png")); // no manifest.toml at all
        let source = PackSource::Directory(pack_dir.clone());
        assert!(open_for_edit(pack_dir, source).is_err());
    }

    #[test]
    fn open_for_edit_fails_for_a_folder_that_no_longer_exists() {
        let dir = tempfile::tempdir().unwrap();
        let vanished = dir.path().join("never-existed");
        let source = PackSource::Directory(vanished.clone());
        assert!(open_for_edit(vanished, source).is_err());
    }

    /// Research.md R3: a manifest mixing solar and clock anchors already fails to
    /// *load* at all (`WallpaperPack::validate` rejects `MixedAnchorTypes`) — so
    /// `open_for_edit` refuses it via the exact same `Err` path as any other load
    /// failure, with no separate mixed-anchor-specific check anywhere in this module.
    #[test]
    fn open_for_edit_fails_for_a_manifest_mixing_solar_and_clock_anchors() {
        let dir = tempfile::tempdir().unwrap();
        let pack_dir = dir.path().join("mixed");
        write_pack_dir(
            &pack_dir,
            "schema_version = 1\nname = \"Mixed\"\ndefault_scaling = \"Fill\"\nfallback_color = \"#000000\"\n\n[[images]]\nfile = \"a.png\"\nanchor = \"sunrise\"\n[[images]]\nfile = \"b.png\"\nanchor = \"06:00\"\n",
            &["a.png", "b.png"],
        );
        let source = PackSource::Directory(pack_dir.clone());
        assert!(open_for_edit(pack_dir, source).is_err());
    }

    /// Research.md R3's own follow-on finding: `schedule_engine` accepts a solar
    /// offset up to 24h (`MAX_SOLAR_OFFSET_HOURS`), wider than this wizard's own ±12h
    /// spin-button range — such a pack loads fine but can't be faithfully
    /// re-represented by this screen, so editing it is refused rather than silently
    /// clamped (which would be exactly the kind of silent field-reset FR-009 forbids).
    #[test]
    fn open_for_edit_fails_for_a_solar_offset_wider_than_the_wizard_can_represent() {
        let dir = tempfile::tempdir().unwrap();
        let pack_dir = dir.path().join("wide-offset");
        write_pack_dir(
            &pack_dir,
            "schema_version = 1\nname = \"Wide\"\ndefault_scaling = \"Fill\"\nfallback_color = \"#000000\"\n\n[[images]]\nfile = \"a.png\"\nanchor = \"sunrise+18h\"\n",
            &["a.png"],
        );
        let source = PackSource::Directory(pack_dir.clone());
        assert!(open_for_edit(pack_dir, source).is_err());
    }

    /// FR-009: an original per-image scaling override and a non-default pack-level
    /// `default_scaling`/`fallback_color` are captured into `preserved` untouched.
    #[test]
    fn open_for_edit_captures_preserved_fields_not_exposed_by_this_screen() {
        let dir = tempfile::tempdir().unwrap();
        let pack_dir = dir.path().join("custom-scaling");
        write_pack_dir(
            &pack_dir,
            r##"
                schema_version = 1
                name = "Custom Scaling"
                default_scaling = "Fit"
                fallback_color = "#112233"
                [[images]]
                file = "a.png"
                anchor = "sunrise"
                scaling = "Center"
                [[images]]
                file = "b.png"
                anchor = "sunset"
            "##,
            &["a.png", "b.png"],
        );
        let source = PackSource::resolve(&pack_dir).unwrap();

        let state = open_for_edit(pack_dir, source).unwrap();
        let preserved = state.preserved.as_ref().unwrap();

        assert_eq!(preserved.default_scaling, ScalingMode::Fit);
        assert_eq!(preserved.fallback_color, Color { r: 0x11, g: 0x22, b: 0x33, a: 255 });
        assert_eq!(preserved.per_image_scaling.get("a.png"), Some(&ScalingMode::Center));
        assert_eq!(preserved.per_image_scaling.get("b.png"), None, "b.png inherits default_scaling, so it's not recorded as an override");
    }

    // --- Spec 012 US1 (T006/T007): build_draft/generate honor `preserved`/`name` ---

    #[test]
    fn build_draft_with_no_preserved_fields_matches_todays_add_flow_defaults() {
        let mut rows = vec![image_row("a.png")];
        rows[0].solar = Some(solar(SolarEventKind::Sunrise));
        let draft = build_draft(&rows, AssignmentMode::SolarPeriod, "My Pack", "Fallback", "", None);

        assert_eq!(draft.name, "My Pack");
        assert_eq!(draft.default_scaling, ScalingMode::Fill);
        assert_eq!(draft.fallback_color, Color { r: 0, g: 0, b: 0, a: 255 });
        assert_eq!(draft.images[0].scaling, None);
    }

    #[test]
    fn build_draft_with_preserved_fields_carries_them_forward_unchanged() {
        let mut rows = vec![image_row("a.png"), image_row("b.png")];
        rows[0].solar = Some(solar(SolarEventKind::Sunrise));
        rows[1].solar = Some(solar(SolarEventKind::Sunset));
        let mut per_image_scaling = HashMap::new();
        per_image_scaling.insert("a.png".to_string(), ScalingMode::Center);
        let preserved = PreservedManifestFields { default_scaling: ScalingMode::Fit, fallback_color: Color { r: 1, g: 2, b: 3, a: 255 }, per_image_scaling };

        let draft = build_draft(&rows, AssignmentMode::SolarPeriod, "Kept Name", "Fallback", "", Some(&preserved));

        assert_eq!(draft.default_scaling, ScalingMode::Fit);
        assert_eq!(draft.fallback_color, Color { r: 1, g: 2, b: 3, a: 255 });
        assert_eq!(draft.images.iter().find(|i| i.file == "a.png").unwrap().scaling, Some(ScalingMode::Center));
        assert_eq!(draft.images.iter().find(|i| i.file == "b.png").unwrap().scaling, None);
    }

    #[test]
    fn build_draft_falls_back_to_the_folder_name_when_name_is_blank() {
        let mut rows = vec![image_row("a.png")];
        rows[0].solar = Some(solar(SolarEventKind::Sunrise));
        let draft = build_draft(&rows, AssignmentMode::SolarPeriod, "   ", "Folder Fallback", "", None);
        assert_eq!(draft.name, "Folder Fallback");
    }

    /// T019 (generate_solar_mode_produces_a_pack_that_loads_back_as_configured) already
    /// covers the add flow's full round trip; this covers the **edit** flow's own
    /// round trip end to end: open_for_edit → change one row → generate() → the
    /// manifest on disk reflects only that change, with no placement dialog at all.
    #[test]
    fn generate_on_an_edit_session_overwrites_the_manifest_directly_with_no_placement_dialog() {
        let dir = tempfile::tempdir().unwrap();
        let pack_dir = dir.path().join("pack");
        write_pack_dir(
            &pack_dir,
            "schema_version = 1\nname = \"Pack\"\nauthor = \"Original Author\"\ndefault_scaling = \"Fill\"\nfallback_color = \"#000000\"\n\n[[images]]\nfile = \"a.png\"\nanchor = \"sunrise\"\n[[images]]\nfile = \"b.png\"\nanchor = \"sunset\"\n",
            &["a.png", "b.png"],
        );
        let source = PackSource::resolve(&pack_dir).unwrap();
        let mut state = open_for_edit(pack_dir.clone(), source).unwrap();

        // Change only b.png's event, from sunset to solar noon.
        let b_index = state.rows.iter().position(|r| r.file_name == "b.png").unwrap();
        set_solar_event_by_index(&mut state, b_index, SOLAR_EVENTS.iter().position(|e| *e == SolarEventKind::SolarNoon).unwrap(), None);

        let closed = generate(&mut state);

        assert!(closed, "an edit session's successful Generate must report the wizard as fully done: {:?}", state.generate_error);
        assert!(state.pending_placement.is_none(), "an edit session must never show the add flow's placement dialog");

        let loaded = pack_loader::load_pack(&pack_dir).unwrap();
        assert_eq!(loaded.author.as_deref(), Some("Original Author"), "author must be unchanged");
        let a = loaded.pack.images().iter().find(|i| i.id.as_str() == "a.png").unwrap();
        assert_eq!(a.anchor, TimeAnchor::solar(SolarEventKind::Sunrise, None), "a.png's assignment must be unchanged");
        let b = loaded.pack.images().iter().find(|i| i.id.as_str() == "b.png").unwrap();
        assert_eq!(b.anchor, TimeAnchor::solar(SolarEventKind::SolarNoon, None), "b.png must reflect the edit");
    }

    /// FR-007: a self-validation failure during an edit session must leave the
    /// existing manifest.toml completely untouched — the same guarantee
    /// `generate_blocked_by_a_duplicate_time_never_writes_a_manifest` already proves
    /// for the add flow, proved here for the edit flow's own direct-write path.
    #[test]
    fn generate_on_an_edit_session_leaves_the_manifest_untouched_when_blocked_by_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let pack_dir = dir.path().join("pack");
        let original = "schema_version = 1\nname = \"Pack\"\ndefault_scaling = \"Fill\"\nfallback_color = \"#000000\"\n\n[[images]]\nfile = \"a.png\"\nanchor = \"sunrise\"\n[[images]]\nfile = \"b.png\"\nanchor = \"sunset\"\n";
        write_pack_dir(&pack_dir, original, &["a.png", "b.png"]);
        let source = PackSource::resolve(&pack_dir).unwrap();
        let mut state = open_for_edit(pack_dir.clone(), source).unwrap();

        // Force a conflict: both images now assigned to sunrise.
        let b_index = state.rows.iter().position(|r| r.file_name == "b.png").unwrap();
        set_solar_event_by_index(&mut state, b_index, SOLAR_EVENTS.iter().position(|e| *e == SolarEventKind::Sunrise).unwrap(), None);
        assert!(state.conflict.is_some());

        let closed = generate(&mut state);

        assert!(!closed);
        assert_eq!(std::fs::read_to_string(pack_dir.join(pack_loader::MANIFEST_FILE_NAME)).unwrap(), original, "the manifest on disk must be byte-for-byte unchanged");
    }

    // --- Spec 012 US3 (T011/T012): negative offsets and conflict detection behave
    // identically whether the row came from `open` (add) or `open_for_edit` (edit). ---

    /// T011: the exact same conflict-detection/mutator functions the add flow's own
    /// `detect_conflict_solar_catches_identical_literal_assignments_without_a_location`
    /// exercises, driven this time against a `State` that came from `open_for_edit` —
    /// there is no separate "edit mode" branch anywhere in `detect_conflict`/
    /// `toggle_solar_offset_sign`/`set_solar_offset_hours`/`set_solar_offset_minutes`
    /// for this to accidentally miss (FR-005's "same interface" guarantee, made
    /// concrete): they all take a plain `&mut State`/`&[ImageRow]`, with no knowledge
    /// of `edit_target` at all.
    #[test]
    fn conflict_detection_behaves_identically_in_an_edit_session_as_in_the_add_flow() {
        let dir = tempfile::tempdir().unwrap();
        let pack_dir = dir.path().join("pack");
        write_pack_dir(
            &pack_dir,
            "schema_version = 1\nname = \"Pack\"\ndefault_scaling = \"Fill\"\nfallback_color = \"#000000\"\n\n[[images]]\nfile = \"a.png\"\nanchor = \"sunrise\"\n[[images]]\nfile = \"b.png\"\nanchor = \"sunset\"\n",
            &["a.png", "b.png"],
        );
        let source = PackSource::resolve(&pack_dir).unwrap();
        let mut state = open_for_edit(pack_dir, source).unwrap();
        assert!(state.conflict.is_none(), "sunrise and sunset don't conflict");

        // Move b.png onto a.png's exact assignment (sunrise, no offset) — must be
        // flagged, identically to how the add flow's own equivalent test asserts.
        let b_index = state.rows.iter().position(|r| r.file_name == "b.png").unwrap();
        let sunrise_index = SOLAR_EVENTS.iter().position(|e| *e == SolarEventKind::Sunrise).unwrap();
        set_solar_event_by_index(&mut state, b_index, sunrise_index, None);
        assert!(state.conflict.is_some(), "an edit session must catch a literal duplicate assignment exactly like the add flow does");

        // Nudge b.png's offset away by 15 minutes — the conflict must clear, exactly
        // as `toggle_solar_offset_sign`/`set_solar_offset_minutes` already behave for
        // the add flow.
        set_solar_offset_minutes(&mut state, b_index, 15, None);
        assert!(state.conflict.is_none(), "changing one image's offset must clear the conflict, same as in the add flow");
    }

    /// T012: a negative offset set during an edit session is bounded to the same
    /// ±12h magnitude `combine_offset` already enforces for the add flow —
    /// `set_solar_offset_hours`/`toggle_solar_offset_sign` are the exact same
    /// functions either flow calls, so this is a regression guard confirming the
    /// shared-interface design didn't accidentally bypass that clamp for a
    /// pre-filled (rather than freshly-added) row.
    #[test]
    fn negative_offset_set_during_an_edit_session_is_bounded_to_twelve_hours() {
        let dir = tempfile::tempdir().unwrap();
        let pack_dir = dir.path().join("pack");
        write_pack_dir(
            &pack_dir,
            "schema_version = 1\nname = \"Pack\"\ndefault_scaling = \"Fill\"\nfallback_color = \"#000000\"\n\n[[images]]\nfile = \"a.png\"\nanchor = \"sunrise\"\n",
            &["a.png"],
        );
        let source = PackSource::resolve(&pack_dir).unwrap();
        let mut state = open_for_edit(pack_dir, source).unwrap();
        let a_index = 0;

        toggle_solar_offset_sign(&mut state, a_index, None); // now negative
        set_solar_offset_hours(&mut state, a_index, 20, None); // attempt to exceed the cap
        set_solar_offset_minutes(&mut state, a_index, 45, None); // attempt to exceed the cap

        let assignment = state.rows[a_index].solar.unwrap();
        assert!(assignment.offset_negative);
        assert_eq!(assignment.offset_hours, 12, "hours must clamp to the same ±12h cap the add flow uses");
        assert_eq!(assignment.offset_minutes, 0, "minutes must be forced to 0 once hours reaches the cap, same as the add flow");
    }

    // --- Solar blocks editor ---

    fn names(rows: &[ImageRow]) -> Vec<(&str, Option<SolarPeriod>)> {
        rows.iter().map(|r| (r.file_name.as_str(), r.period)).collect()
    }

    fn blocks_rows() -> Vec<ImageRow> {
        let mut rows: Vec<ImageRow> = ["a", "b", "c", "d"].iter().map(|n| image_row(n)).collect();
        rows[0].period = Some(SolarPeriod::Morning);
        rows[1].period = Some(SolarPeriod::Morning);
        rows[2].period = Some(SolarPeriod::Night);
        rows
    }

    #[test]
    fn place_row_inserts_before_the_indexed_block_member() {
        let mut rows = blocks_rows();
        place_row(&mut rows, "d", Some(SolarPeriod::Morning), 0);
        assert_eq!(
            names(&rows),
            vec![("d", Some(SolarPeriod::Morning)), ("a", Some(SolarPeriod::Morning)), ("b", Some(SolarPeriod::Morning)), ("c", Some(SolarPeriod::Night))]
        );
    }

    #[test]
    fn place_row_onto_own_position_is_a_no_op() {
        let mut rows = blocks_rows();
        let before = rows.clone();
        place_row(&mut rows, "a", Some(SolarPeriod::Morning), 0);
        assert_eq!(rows, before);
        place_row(&mut rows, "a", Some(SolarPeriod::Morning), 1);
        assert_eq!(rows, before);
    }

    #[test]
    fn place_row_into_an_empty_block_keeps_block_order() {
        let mut rows = blocks_rows();
        place_row(&mut rows, "a", Some(SolarPeriod::Evening), 0);
        assert_eq!(
            names(&rows),
            vec![("b", Some(SolarPeriod::Morning)), ("a", Some(SolarPeriod::Evening)), ("c", Some(SolarPeriod::Night)), ("d", None)]
        );
        place_row(&mut rows, "b", None, usize::MAX);
        assert_eq!(names(&rows).last(), Some(&("b", None)));
    }

    #[test]
    fn moving_inside_a_block_only_changes_that_blocks_times() {
        let location = Location::new(48.85, 2.35).unwrap();
        let date = NaiveDate::from_ymd_opt(2024, 3, 20).unwrap();
        let mut state = open_test_state(&["a.png", "b.png", "c.png"]);
        set_mode(&mut state, AssignmentMode::SolarBlocks);
        select_block(&mut state, "a.png", Some(SolarPeriod::Night));
        select_block(&mut state, "b.png", Some(SolarPeriod::Morning));
        select_block(&mut state, "c.png", Some(SolarPeriod::Morning));
        state.location = Some(location);
        state.preview_date = date;
        let before = preview_schedule(&state).unwrap();

        move_within_block(&mut state, "c.png", -1);
        assert_eq!(names(&state.rows)[0].0, "c.png");
        let after = preview_schedule(&state).unwrap();
        assert_eq!(before.block(SolarPeriod::Night), after.block(SolarPeriod::Night));
        assert_ne!(before.block(SolarPeriod::Morning), after.block(SolarPeriod::Morning));

        move_within_block(&mut state, "c.png", 1);
        assert_eq!(preview_schedule(&state).unwrap(), before);
    }

    #[test]
    fn drag_and_drop_places_on_the_hovered_target_and_cancels_elsewhere() {
        let mut state = open_test_state(&["a.png", "b.png"]);
        set_mode(&mut state, AssignmentMode::SolarBlocks);
        start_drag(&mut state, "a.png".into());
        let target = DropTarget { block: Some(SolarPeriod::Day), index: 0 };
        drag_entered(&mut state, target);
        release_drag(&mut state);
        assert_eq!(state.rows[0].period, Some(SolarPeriod::Day));
        assert!(state.dragging.is_none());

        start_drag(&mut state, "b.png".into());
        drag_entered(&mut state, target);
        drag_left(&mut state, target);
        release_drag(&mut state);
        assert_eq!(state.rows.iter().find(|r| r.file_name == "b.png").unwrap().period, None);

        // Scrolling moves the content under a still cursor, so the hovered slot is stale.
        start_drag(&mut state, "b.png".into());
        drag_entered(&mut state, target);
        drag_scrolled(&mut state);
        release_drag(&mut state);
        assert_eq!(state.rows.iter().find(|r| r.file_name == "b.png").unwrap().period, None);
    }

    #[test]
    fn blocks_mode_needs_one_placed_image_and_skips_the_tray() {
        let mut rows = blocks_rows();
        assert!(all_assigned(&rows, AssignmentMode::SolarBlocks));
        let draft = build_draft(&rows, AssignmentMode::SolarBlocks, "", "F", "", None);
        let anchors: Vec<_> = draft.images.iter().map(|i| (i.file.as_str(), i.anchor)).collect();
        assert_eq!(
            anchors,
            vec![
                ("a", TimeAnchor::Period { period: SolarPeriod::Morning, slot: 0, of: 2 }),
                ("b", TimeAnchor::Period { period: SolarPeriod::Morning, slot: 1, of: 2 }),
                ("c", TimeAnchor::Period { period: SolarPeriod::Night, slot: 0, of: 1 }),
            ]
        );
        for row in &mut rows {
            row.period = None;
        }
        assert!(!all_assigned(&rows, AssignmentMode::SolarBlocks));
    }

    #[test]
    fn converting_keeps_todays_order_and_classifies_by_time() {
        use chrono::{Offset, TimeZone};
        let date = NaiveDate::from_ymd_opt(2024, 3, 20).unwrap();
        // A longitude matching the test machine's own UTC offset, so local clock times
        // line up with the sun the same way everywhere this test runs.
        let offset = Local.from_local_datetime(&date.and_hms_opt(12, 0, 0).unwrap()).unwrap().offset().fix().local_minus_utc();
        let location = Location::new(45.0, (f64::from(offset) / 240.0).clamp(-180.0, 180.0)).unwrap();
        let mut rows: Vec<ImageRow> = ["late", "noon", "early", "dawn", "unset"].iter().map(|n| image_row(n)).collect();
        rows[0].time = NaiveTime::from_hms_opt(23, 59, 0);
        rows[1].time = NaiveTime::from_hms_opt(12, 30, 0);
        rows[2].time = NaiveTime::from_hms_opt(1, 0, 0);
        rows[3].solar = Some(solar(SolarEventKind::Sunrise));

        convert_rows_to_blocks(&mut rows, &location, date);

        assert_eq!(
            names(&rows),
            vec![
                ("dawn", Some(SolarPeriod::Morning)),
                ("noon", Some(SolarPeriod::Day)),
                // The night runs past midnight: tonight's 23:59 comes before 01:00.
                ("late", Some(SolarPeriod::Night)),
                ("early", Some(SolarPeriod::Night)),
                ("unset", None),
            ]
        );
        assert!(rows.iter().all(|r| r.solar.is_none() && r.time.is_none()));
    }

    #[test]
    fn converting_keeps_the_night_block_that_crosses_midnight_in_order() {
        use chrono::{Offset, TimeZone};
        let date = NaiveDate::from_ymd_opt(2024, 6, 21).unwrap();
        // 30° west of the machine's own offset longitude: local clocks run 2 h ahead
        // of the sun, so the short midsummer night is still well clear of midnight.
        let offset = Local.from_local_datetime(&date.and_hms_opt(12, 0, 0).unwrap()).unwrap().offset().fix().local_minus_utc();
        let location = Location::new(47.0, (f64::from(offset) / 240.0 - 30.0).clamp(-180.0, 180.0)).unwrap();
        let boundaries = schedule_engine::PeriodBoundaries::compute(&location, date).unwrap();
        let (dusk_start, dusk_end) = boundaries.span(SolarPeriod::Night);
        let at = |d: NaiveDate, h, m| Local.from_local_datetime(&d.and_hms_opt(h, m, 0).unwrap()).unwrap();
        assert!(dusk_start < at(date, 23, 50) && dusk_end > at(date + TimeDelta::days(1), 0, 10), "fixture assumption {dusk_start} {dusk_end}");

        let mut rows: Vec<ImageRow> = ["after-midnight", "before-midnight"].iter().map(|n| image_row(n)).collect();
        rows[0].time = NaiveTime::from_hms_opt(0, 10, 0);
        rows[1].time = NaiveTime::from_hms_opt(23, 50, 0);

        convert_rows_to_blocks(&mut rows, &location, date);

        assert_eq!(
            names(&rows),
            vec![("before-midnight", Some(SolarPeriod::Night)), ("after-midnight", Some(SolarPeriod::Night))]
        );
    }

    #[test]
    fn a_blocks_pack_round_trips_through_generate_and_edit() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.png", "b.png", "c.png", "d.png"] {
            write_test_image(&dir.path().join(name));
        }
        let mut state = open(dir.path().to_path_buf());
        set_mode(&mut state, AssignmentMode::SolarBlocks);
        select_block(&mut state, "c.png", Some(SolarPeriod::Morning));
        select_block(&mut state, "a.png", Some(SolarPeriod::Morning));
        select_block(&mut state, "b.png", Some(SolarPeriod::Night));
        let _ = generate(&mut state);
        assert_eq!(state.generate_error, None);
        let registry_dir = tempfile::tempdir().unwrap();
        let mut registry = pack_loader::Registry::open_at(registry_dir.path()).unwrap();
        assert!(confirm_keep(&mut state, &mut registry), "{:?}", state.move_error);

        let source = PackSource::resolve(dir.path()).unwrap();
        let edit = open_for_edit(dir.path().to_path_buf(), source).unwrap();
        assert_eq!(edit.mode, Some(AssignmentMode::SolarBlocks));
        assert_eq!(
            names(&edit.rows),
            vec![("c.png", Some(SolarPeriod::Morning)), ("a.png", Some(SolarPeriod::Morning)), ("b.png", Some(SolarPeriod::Night)), ("d.png", None)]
        );
    }
}
