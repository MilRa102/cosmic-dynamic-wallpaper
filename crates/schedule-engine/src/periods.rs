//! Solar periods ("parts of the day") — the four fixed astronomical segments a day is
//! split into, and the pure [`resolve_schedule`] that turns a [`PeriodLayout`] (images
//! placed into blocks, in order) into concrete wall-clock intervals for one date and
//! location.
//!
//! ## Boundaries
//!
//! A solar day runs from morning civil dawn to the next morning's civil dawn:
//!
//! | Block   | From                                   | To                                     |
//! |---------|----------------------------------------|----------------------------------------|
//! | Morning | civil dawn (sun at −6°)                | a quarter of the daylight after sunrise |
//! | Day     | a quarter of the daylight after sunrise | a quarter of the daylight before sunset |
//! | Evening | a quarter of the daylight before sunset | civil dusk (sun at −6°)                |
//! | Night   | civil dusk                             | the next morning's civil dawn          |
//!
//! so Day is the middle half of the time the sun is up, centred on solar noon.
//!
//! The solar transit `T` and declination `δ` come from the `sunrise` crate (see
//! [`transit_and_declination`]); each elevation crossing is then `T ∓ H(h)` with the
//! standard hour-angle relation `cos H = (sin h − sin φ sin δ) / (cos φ cos δ)`.
//!
//! When the sun never reaches an elevation on a date (polar day/night) `cos H` falls
//! outside `[-1, 1]`; clamping it makes the crossing degenerate continuously instead of
//! disappearing: "always above `h`" puts the morning crossing at `T − 12h` and the
//! evening one at `T + 12h`, "always below `h`" collapses both onto `T`. Blocks bounded
//! by such crossings simply become zero-length on that date (no Night during polar day,
//! no Day during polar night) and their images are skipped — the previous image stays
//! on screen. The day is therefore always covered end to end.

use std::cell::RefCell;
use std::f64::consts::TAU;

use chrono::{DateTime, Local, NaiveDate, TimeDelta, Utc};
use sunrise::{Coordinates, SolarDay, SolarEvent};

use crate::anchor::TimeAnchor;
use crate::location::Location;
use crate::pack::{ImageId, PackImage};

/// Sunrise/sunset elevation, matching the `sunrise` crate (refraction + solar radius).
const HORIZON_ELEVATION_DEG: f64 = -5.0 / 6.0;
const CIVIL_ELEVATION_DEG: f64 = -6.0;

/// Blocks shorter than this are treated as not occurring on that date. A collapsed
/// block (polar day/night) is not exactly zero long: the transit drifts by up to ~30 s
/// from one day to the next.
pub const MIN_PERIOD_LENGTH: TimeDelta = TimeDelta::minutes(1);

/// Keeps `cos φ` away from zero at the geographic poles; 0.01° from the pole changes
/// nothing observable but keeps the hour-angle formula finite.
const MAX_EFFECTIVE_LATITUDE_DEG: f64 = 89.99;

/// One of the four fixed parts of the day, in chronological order starting from
/// morning civil dawn (see the module docs for the exact boundaries).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SolarPeriod {
    /// Civil dawn → a quarter of the daylight after sunrise.
    Morning,
    /// The middle half of the daylight, centred on solar noon.
    Day,
    /// A quarter of the daylight before sunset → civil dusk.
    Evening,
    /// Civil dusk → the next morning's civil dawn.
    Night,
}

impl SolarPeriod {
    /// Number of periods.
    pub const COUNT: usize = 4;

    /// Every period, in chronological order.
    pub const ALL: [SolarPeriod; Self::COUNT] =
        [SolarPeriod::Morning, SolarPeriod::Day, SolarPeriod::Evening, SolarPeriod::Night];

    /// Position in [`SolarPeriod::ALL`].
    pub fn index(self) -> usize {
        self as usize
    }

    /// The period that follows this one (cyclically).
    pub fn next(self) -> SolarPeriod {
        Self::ALL[(self.index() + 1) % Self::COUNT]
    }

    /// The period that precedes this one (cyclically).
    pub fn prev(self) -> SolarPeriod {
        Self::ALL[(self.index() + Self::COUNT - 1) % Self::COUNT]
    }

    /// Stable snake_case name, as written in pack manifests.
    pub fn as_str(self) -> &'static str {
        match self {
            SolarPeriod::Morning => "morning",
            SolarPeriod::Day => "day",
            SolarPeriod::Evening => "evening",
            SolarPeriod::Night => "night",
        }
    }

    /// Inverse of [`SolarPeriod::as_str`]; also accepts the legacy block names (see
    /// [`SolarPeriod::parse_with_rank`]).
    pub fn parse(name: &str) -> Option<SolarPeriod> {
        Self::parse_with_rank(name).map(|(period, _)| period)
    }

    /// Parses a period name, including the twelve finer-grained block names manifests
    /// were written with before the four parts of the day (`civil_dawn`, `noon`,
    /// `nautical_dusk`, …), each folded into the part of the day it falls in.
    ///
    /// The rank orders legacy blocks merged into the same part of the day
    /// chronologically — e.g. evening `astronomical_dusk` images before pre-dawn
    /// `night` ones in Night, which now starts at dusk. Images sharing a rank keep their
    /// manifest order.
    pub fn parse_with_rank(name: &str) -> Option<(SolarPeriod, u8)> {
        use SolarPeriod::{Day, Evening, Morning, Night};
        Some(match name {
            "civil_dawn" => (Morning, 0),
            "sunrise" => (Morning, 1),
            "morning" => (Morning, 2),
            "noon" | "day" => (Day, 0),
            "afternoon" => (Day, 1),
            "sunset" | "evening" => (Evening, 0),
            "civil_dusk" => (Evening, 1),
            "nautical_dusk" => (Night, 0),
            "astronomical_dusk" => (Night, 1),
            "night" => (Night, 2),
            "astronomical_dawn" => (Night, 3),
            "nautical_dawn" => (Night, 4),
            _ => return None,
        })
    }
}

impl std::fmt::Display for SolarPeriod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The solar transit (UTC) and the sun's declination (radians) for `date` at `lon`.
///
/// Both depend only on longitude and date. They are recovered from two `sunrise` crate
/// events that exist on every date: at the equator the h=0 hour angle is exactly 90°,
/// so `T = morning + 6h`; at 45° latitude `cos H = −tan δ`.
fn transit_and_declination(longitude: f64, date: NaiveDate) -> Option<(DateTime<Utc>, f64)> {
    let horizon = SolarEvent::Elevation { elevation: 0.0, morning: true };
    let equator_morning = SolarDay::new(Coordinates::new(0.0, longitude)?, date).event_time(horizon)?;
    let transit = equator_morning + TimeDelta::hours(6);
    let mid_morning = SolarDay::new(Coordinates::new(45.0, longitude)?, date).event_time(horizon)?;
    let hour_angle = (transit - mid_morning).num_milliseconds() as f64 / 86_400_000.0 * TAU;
    Some((transit, (-hour_angle.cos()).atan()))
}

/// Hour angle (radians, `0..=π`) at which the sun crosses `elevation_deg`, clamped so a
/// never-reached elevation degenerates instead of failing (see module docs).
fn hour_angle(latitude_rad: f64, declination: f64, elevation_deg: f64) -> f64 {
    let cos_h = (elevation_deg.to_radians().sin() - latitude_rad.sin() * declination.sin())
        / (latitude_rad.cos() * declination.cos());
    cos_h.clamp(-1.0, 1.0).acos()
}

fn day_fraction(hour_angle: f64) -> TimeDelta {
    TimeDelta::milliseconds((hour_angle / TAU * 86_400_000.0).round() as i64)
}

/// The monotonic edges delimiting the [`SolarPeriod`]s of one solar day.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeriodBoundaries {
    edges: [DateTime<Utc>; SolarPeriod::COUNT + 1],
}

impl PeriodBoundaries {
    /// Compute the boundaries for the solar day starting at `date`'s civil dawn, at
    /// `location`. `None` only if the underlying solar model rejects the
    /// coordinates/date (never for a valid [`Location`] and a realistic date).
    pub fn compute(location: &Location, date: NaiveDate) -> Option<Self> {
        let lat = location
            .latitude()
            .clamp(-MAX_EFFECTIVE_LATITUDE_DEG, MAX_EFFECTIVE_LATITUDE_DEG)
            .to_radians();
        let lon = location.longitude();
        let (transit, decl) = transit_and_declination(lon, date)?;
        let (next_transit, next_decl) = transit_and_declination(lon, date.succ_opt()?)?;

        let civil = day_fraction(hour_angle(lat, decl, CIVIL_ELEVATION_DEG));
        let half_day = day_fraction(hour_angle(lat, decl, HORIZON_ELEVATION_DEG) / 2.0);
        let mut edges = [
            transit - civil,
            transit - half_day,
            transit + half_day,
            transit + civil,
            next_transit - day_fraction(hour_angle(lat, next_decl, CIVIL_ELEVATION_DEG)),
        ];
        // Day-to-day drift of the transit/declination can leave this evening's dusk a
        // few seconds past the next morning's dawn (polar day); enforce monotonic edges.
        for i in 1..edges.len() {
            edges[i] = edges[i].max(edges[i - 1]);
        }
        Some(Self { edges })
    }

    /// `[start, end)` of `period` (local time); see [`PeriodBoundaries::is_empty`].
    pub fn span(&self, period: SolarPeriod) -> (DateTime<Local>, DateTime<Local>) {
        let i = period.index();
        (self.edges[i].with_timezone(&Local), self.edges[i + 1].with_timezone(&Local))
    }

    /// `true` if `period` doesn't occur on this date (shorter than [`MIN_PERIOD_LENGTH`]).
    pub fn is_empty(&self, period: SolarPeriod) -> bool {
        let i = period.index();
        self.edges[i + 1] - self.edges[i] < MIN_PERIOD_LENGTH
    }

    /// Start of the first period (civil dawn) to end of the last (next civil dawn).
    pub fn day_span(&self) -> (DateTime<Local>, DateTime<Local>) {
        (self.edges[0].with_timezone(&Local), self.edges[SolarPeriod::COUNT].with_timezone(&Local))
    }

    /// The period containing `at`, if `at` lies within [`PeriodBoundaries::day_span`].
    pub fn period_at(&self, at: DateTime<Local>) -> Option<SolarPeriod> {
        let at = at.with_timezone(&Utc);
        SolarPeriod::ALL
            .into_iter()
            .find(|p| self.edges[p.index()] <= at && at < self.edges[p.index() + 1])
    }

    /// Instant of slot `slot` of `of` evenly spaced slots inside `period`; `None` when the
    /// period has zero length on this date (the image is skipped, previous one holds).
    pub fn slot_instant(&self, period: SolarPeriod, slot: u8, of: u8) -> Option<DateTime<Local>> {
        if self.is_empty(period) || of == 0 || slot >= of {
            return None;
        }
        let i = period.index();
        let (start, end) = (self.edges[i], self.edges[i + 1]);
        let offset = (end - start).num_milliseconds() * i64::from(slot) / i64::from(of);
        Some((start + TimeDelta::milliseconds(offset)).with_timezone(&Local))
    }
}

/// Classify `at` into the solar period containing it (used to migrate event- and
/// clock-anchored packs into blocks). Checks the date of `at` and its neighbours since a
/// solar day doesn't align with the calendar day.
pub fn period_at(location: &Location, at: DateTime<Local>) -> Option<SolarPeriod> {
    let date = at.date_naive();
    [date, date.succ_opt()?, date.pred_opt()?]
        .into_iter()
        .filter_map(|d| PeriodBoundaries::compute(location, d))
        .find_map(|b| b.period_at(at))
}

/// Images distributed over the solar blocks, in display/playback order within each
/// block. The static half of a period-based pack: no times, no location.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeriodLayout {
    blocks: [Vec<ImageId>; SolarPeriod::COUNT],
}

impl PeriodLayout {
    /// An empty layout.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild a layout from period-anchored pack images (ordered by slot). Returns
    /// `None` if any image isn't [`TimeAnchor::Period`].
    pub fn from_images(images: &[PackImage]) -> Option<Self> {
        let mut slotted: [Vec<(u8, ImageId)>; SolarPeriod::COUNT] = Default::default();
        for img in images {
            let TimeAnchor::Period { period, slot, .. } = img.anchor else {
                return None;
            };
            slotted[period.index()].push((slot, img.id.clone()));
        }
        let mut layout = Self::new();
        for (block, mut entries) in layout.blocks.iter_mut().zip(slotted) {
            entries.sort_by_key(|(slot, _)| *slot);
            *block = entries.into_iter().map(|(_, id)| id).collect();
        }
        Some(layout)
    }

    /// The images of `period`, in order.
    pub fn block(&self, period: SolarPeriod) -> &[ImageId] {
        &self.blocks[period.index()]
    }

    /// Mutable access to the images of `period`.
    pub fn block_mut(&mut self, period: SolarPeriod) -> &mut Vec<ImageId> {
        &mut self.blocks[period.index()]
    }

    /// Total number of images across all blocks.
    pub fn len(&self) -> usize {
        self.blocks.iter().map(Vec::len).sum()
    }

    /// `true` if no block holds an image.
    pub fn is_empty(&self) -> bool {
        self.blocks.iter().all(Vec::is_empty)
    }

    /// Where `id` currently sits, as `(period, index)`.
    pub fn position(&self, id: &ImageId) -> Option<(SolarPeriod, usize)> {
        SolarPeriod::ALL
            .into_iter()
            .find_map(|p| self.block(p).iter().position(|x| x == id).map(|i| (p, i)))
    }

    /// Remove `id` from whichever block holds it.
    pub fn remove(&mut self, id: &ImageId) -> Option<(SolarPeriod, usize)> {
        let (period, index) = self.position(id)?;
        self.block_mut(period).remove(index);
        Some((period, index))
    }

    /// Move (or insert) `id` into `period` before position `index` (clamped to the block
    /// length). Indices refer to the target block *before* `id` is removed from it, so
    /// "drop onto the thumbnail at `index`" lands in front of that thumbnail. Only the
    /// source and target blocks change.
    pub fn place(&mut self, id: ImageId, period: SolarPeriod, index: usize) {
        let mut index = index;
        if let Some((from, from_index)) = self.remove(&id) {
            if from == period && from_index < index {
                index -= 1;
            }
        }
        let block = self.block_mut(period);
        let index = index.min(block.len());
        block.insert(index, id);
    }

    /// The period-anchored [`PackImage`]s for this layout, in chronological order.
    pub fn to_images(&self) -> Vec<PackImage> {
        SolarPeriod::ALL
            .into_iter()
            .flat_map(|period| {
                let block = self.block(period);
                let of = u8::try_from(block.len()).unwrap_or(u8::MAX);
                block.iter().enumerate().map(move |(slot, id)| {
                    let slot = u8::try_from(slot).unwrap_or(u8::MAX);
                    PackImage::new(id.clone(), TimeAnchor::Period { period, slot, of })
                })
            })
            .collect()
    }
}

/// One image's computed interval.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedImage {
    /// The image.
    pub id: ImageId,
    /// When it becomes active.
    pub start: DateTime<Local>,
    /// End of its share of the block.
    pub end: DateTime<Local>,
    /// When the next image actually replaces it — later than `end` when the following
    /// blocks are empty (they keep showing this image).
    pub active_until: DateTime<Local>,
}

/// One block's computed interval and contents.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedBlock {
    /// Which period.
    pub period: SolarPeriod,
    /// Block start.
    pub start: DateTime<Local>,
    /// Block end (see [`ResolvedBlock::is_degenerate`]).
    pub end: DateTime<Local>,
    /// Its images with their intervals; empty when the block is empty or doesn't occur.
    pub images: Vec<ResolvedImage>,
    /// The image shown during this block when it has no images of its own: the last
    /// image of the nearest preceding block that has any.
    pub inherited: Option<ImageId>,
}

impl ResolvedBlock {
    /// `true` when this period doesn't occur on the resolved date.
    pub fn is_degenerate(&self) -> bool {
        self.end - self.start < MIN_PERIOD_LENGTH
    }
}

/// The resolved schedule for one date: every block, covering one full solar day.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSchedule {
    /// The date resolved.
    pub date: NaiveDate,
    /// The blocks, in [`SolarPeriod::ALL`] order.
    pub blocks: Vec<ResolvedBlock>,
}

impl ResolvedSchedule {
    /// The block for `period`.
    pub fn block(&self, period: SolarPeriod) -> &ResolvedBlock {
        &self.blocks[period.index()]
    }
}

thread_local! {
    static BOUNDARY_CACHE: RefCell<Option<(Location, NaiveDate, PeriodBoundaries)>> = const { RefCell::new(None) };
}

/// [`PeriodBoundaries::compute`], memoizing the most recent result per thread (the
/// query engine and the editor ask for the same date many times in a row).
pub(crate) fn boundaries_cached(location: &Location, date: NaiveDate) -> Option<PeriodBoundaries> {
    BOUNDARY_CACHE.with(|cache| {
        if let Some((loc, d, b)) = *cache.borrow() {
            if loc == *location && d == date {
                return Some(b);
            }
        }
        let b = PeriodBoundaries::compute(location, date)?;
        *cache.borrow_mut() = Some((*location, date, b));
        Some(b)
    })
}

/// Resolve `layout` to concrete intervals for `date` at `location`.
///
/// Pure and deterministic: the same inputs always give the same output. Each block is
/// split evenly between its images by position, so reordering inside one block never
/// moves another block's times. Empty and non-occurring blocks report the image they
/// inherit. Returns `None` if the solar model rejects a date or no neighboring
/// image occurrence can be found within the query engine's bounded search.
pub fn resolve_schedule(layout: &PeriodLayout, location: &Location, date: NaiveDate) -> Option<ResolvedSchedule> {
    let bounds = PeriodBoundaries::compute(location, date)?;

    let mut blocks: Vec<ResolvedBlock> = SolarPeriod::ALL
        .into_iter()
        .map(|period| {
            let (start, end) = bounds.span(period);
            let ids = layout.block(period);
            let of = u8::try_from(ids.len()).unwrap_or(u8::MAX);
            let images = ids
                .iter()
                .enumerate()
                .filter_map(|(slot, id)| {
                    let slot = u8::try_from(slot).ok()?;
                    let s = bounds.slot_instant(period, slot, of)?;
                    let e = if slot + 1 < of { bounds.slot_instant(period, slot + 1, of)? } else { end };
                    Some(ResolvedImage { id: id.clone(), start: s, end: e, active_until: e })
                })
                .collect();
            ResolvedBlock { period, start, end, images, inherited: None }
        })
        .collect();

    let neighboring_image = |forward: bool| {
        let mut day = date;
        // `query` searches up to 512 days in its radius-doubling search.
        for _ in 0..512 {
            day = if forward { day.succ_opt()? } else { day.pred_opt()? };
            let other = PeriodBoundaries::compute(location, day)?;
            for index in 0..SolarPeriod::COUNT {
                let period = SolarPeriod::ALL[if forward { index } else { SolarPeriod::COUNT - 1 - index }];
                let ids = layout.block(period);
                let n = u8::try_from(ids.len()).unwrap_or(u8::MAX);
                if n == 0 {
                    continue;
                }
                let slot = if forward { 0 } else { n - 1 };
                if let Some(start) = other.slot_instant(period, slot, n) {
                    return Some((ids[usize::from(slot)].clone(), start));
                }
            }
        }
        None
    };

    let (previous, next) = if layout.is_empty() {
        (None, None)
    } else {
        (Some(neighboring_image(false)?), Some(neighboring_image(true)?))
    };
    let mut following_start = next.map(|(_, start)| start);
    for block in blocks.iter_mut().rev() {
        for image in block.images.iter_mut().rev() {
            if let Some(next) = following_start {
                image.active_until = next.max(image.end);
            }
            following_start = Some(image.start);
        }
    }

    let mut carried = previous.map(|(id, _)| id);
    for block in &mut blocks {
        if let Some(last) = block.images.last() {
            carried = Some(last.id.clone());
        } else {
            block.inherited.clone_from(&carried);
        }
    }

    Some(ResolvedSchedule { date, blocks })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(lat: f64, lon: f64) -> Location {
        Location::new(lat, lon).unwrap()
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn ids(names: &[&str]) -> Vec<ImageId> {
        names.iter().map(|n| ImageId::new(*n)).collect()
    }

    fn sample_layout() -> PeriodLayout {
        let mut layout = PeriodLayout::new();
        *layout.block_mut(SolarPeriod::Morning) = ids(&["m1", "m2", "m3"]);
        *layout.block_mut(SolarPeriod::Evening) = ids(&["set"]);
        *layout.block_mut(SolarPeriod::Night) = ids(&["night"]);
        layout
    }

    #[test]
    fn names_roundtrip() {
        for p in SolarPeriod::ALL {
            assert_eq!(SolarPeriod::parse(p.as_str()), Some(p));
            assert_eq!(p.next().prev(), p);
        }
        assert_eq!(SolarPeriod::parse("dusk"), None);
        assert_eq!(SolarPeriod::Night.next(), SolarPeriod::Morning);
    }

    #[test]
    fn legacy_block_names_fold_into_the_four_parts_of_the_day_in_chronological_rank() {
        let parsed = |n| SolarPeriod::parse_with_rank(n).unwrap();
        assert_eq!(parsed("civil_dawn"), (SolarPeriod::Morning, 0));
        assert_eq!(parsed("noon").0, SolarPeriod::Day);
        assert_eq!(parsed("afternoon").0, SolarPeriod::Day);
        assert_eq!(parsed("civil_dusk").0, SolarPeriod::Evening);
        // Night starts at dusk now: evening twilight comes before the pre-dawn hours.
        let night: Vec<u8> = ["nautical_dusk", "astronomical_dusk", "night", "astronomical_dawn", "nautical_dawn"]
            .into_iter()
            .map(|n| {
                let (period, rank) = parsed(n);
                assert_eq!(period, SolarPeriod::Night, "{n}");
                rank
            })
            .collect();
        assert!(night.windows(2).all(|w| w[0] < w[1]), "{night:?}");
    }

    #[test]
    fn boundaries_are_monotonic_and_span_one_day_everywhere() {
        for lat in [-90.0, -78.2, -66.0, -45.0, 0.0, 23.4, 51.5, 60.0, 64.0, 69.6, 78.2, 90.0] {
            for lon in [-179.0, -79.4, 0.0, 37.6, 139.7, 180.0] {
                for d in [date(2024, 1, 1), date(2024, 3, 20), date(2024, 6, 21), date(2024, 12, 21)] {
                    let b = PeriodBoundaries::compute(&loc(lat, lon), d).unwrap();
                    for w in b.edges.windows(2) {
                        assert!(w[0] <= w[1], "non-monotonic at {lat},{lon} {d}: {:?}", b.edges);
                    }
                    // Night ends at the next civil dawn, where the next solar day starts —
                    // consecutive dates tile time with no gap (up to transit drift).
                    let next = PeriodBoundaries::compute(&loc(lat, lon), d.succ_opt().unwrap()).unwrap();
                    assert!((b.edges[SolarPeriod::COUNT] - next.edges[0]).num_seconds().abs() <= 60);
                    // Dawn → next dawn: ~24h, give or take the day-to-day change in
                    // twilight length (minutes at high latitudes).
                    let len = b.edges[SolarPeriod::COUNT] - b.edges[0];
                    let tolerance = if f64::abs(lat) <= 52.0 { 10 } else { 24 * 60 };
                    assert!((len - TimeDelta::days(1)).num_minutes().abs() <= tolerance, "{lat},{lon} {d}: {len}");
                }
            }
        }
    }

    #[test]
    fn matches_sunrise_crate_events_at_mid_latitude() {
        let london = loc(51.5072, -0.1276);
        let d = date(2024, 6, 21);
        let b = PeriodBoundaries::compute(&london, d).unwrap();
        let day = SolarDay::new(Coordinates::new(51.5072, -0.1276).unwrap(), d);
        let event = |e: SolarEvent| day.event_time(e).unwrap().with_timezone(&Local);
        let close = |a: DateTime<Local>, expected: DateTime<Local>| {
            assert!((a - expected).num_seconds().abs() <= 5, "{a} vs {expected}");
        };
        close(b.span(SolarPeriod::Morning).0, event(SolarEvent::Dawn(sunrise::DawnType::Civil)));
        close(b.span(SolarPeriod::Night).0, event(SolarEvent::Dusk(sunrise::DawnType::Civil)));
        // Day is the middle half of the daylight.
        let (sunrise, sunset) = (event(SolarEvent::Sunrise), event(SolarEvent::Sunset));
        let quarter = (sunset - sunrise) / 4;
        let (day_start, day_end) = b.span(SolarPeriod::Day);
        close(day_start, sunrise + quarter);
        close(day_end, sunset - quarter);
    }

    #[test]
    fn polar_night_and_polar_day_degenerate_gracefully() {
        let tromso = loc(69.65, 18.96);
        let winter = PeriodBoundaries::compute(&tromso, date(2024, 12, 21)).unwrap();
        // The sun stays below the horizon but civil twilight still happens around noon.
        assert!(winter.is_empty(SolarPeriod::Day));
        assert!(!winter.is_empty(SolarPeriod::Morning) && !winter.is_empty(SolarPeriod::Evening));
        assert!(!winter.is_empty(SolarPeriod::Night));
        let summer = PeriodBoundaries::compute(&tromso, date(2024, 6, 21)).unwrap();
        assert!(summer.is_empty(SolarPeriod::Night));
        assert!(!summer.is_empty(SolarPeriod::Morning) && !summer.is_empty(SolarPeriod::Day));
        let (morning_start, _) = summer.span(SolarPeriod::Morning);
        let (_, evening_end) = summer.span(SolarPeriod::Evening);
        assert!(((evening_end - morning_start) - TimeDelta::days(1)).num_minutes().abs() <= 1);

        let pole_winter = PeriodBoundaries::compute(&loc(90.0, 0.0), date(2024, 12, 21)).unwrap();
        for p in [SolarPeriod::Morning, SolarPeriod::Day, SolarPeriod::Evening] {
            assert!(pole_winter.is_empty(p), "{p}");
        }
    }

    #[test]
    fn period_at_classifies_known_times() {
        let london = loc(51.5072, -0.1276);
        let b = PeriodBoundaries::compute(&london, date(2024, 3, 20)).unwrap();
        for p in SolarPeriod::ALL {
            let (s, e) = b.span(p);
            let mid = s + (e - s) / 2;
            assert_eq!(period_at(&london, mid), Some(p));
        }
    }

    #[test]
    fn layout_place_moves_within_and_across_blocks() {
        let mut layout = sample_layout();
        layout.place(ImageId::new("m3"), SolarPeriod::Morning, 0);
        assert_eq!(layout.block(SolarPeriod::Morning), ids(&["m3", "m1", "m2"]).as_slice());
        // Dropping onto the thumbnail at index 2 lands before it even when moving forward.
        layout.place(ImageId::new("m3"), SolarPeriod::Morning, 2);
        assert_eq!(layout.block(SolarPeriod::Morning), ids(&["m1", "m3", "m2"]).as_slice());
        layout.place(ImageId::new("m1"), SolarPeriod::Day, 99);
        assert_eq!(layout.block(SolarPeriod::Day), ids(&["m1"]).as_slice());
        assert_eq!(layout.block(SolarPeriod::Morning), ids(&["m3", "m2"]).as_slice());
        layout.place(ImageId::new("new"), SolarPeriod::Night, 0);
        assert_eq!(layout.block(SolarPeriod::Night), ids(&["new", "night"]).as_slice());
        assert_eq!(layout.len(), 6);
    }

    #[test]
    fn layout_images_roundtrip() {
        let layout = sample_layout();
        let images = layout.to_images();
        assert_eq!(images.len(), 5);
        assert_eq!(PeriodLayout::from_images(&images), Some(layout));
    }

    #[test]
    fn schedule_splits_block_evenly_and_covers_the_day() {
        let london = loc(51.5072, -0.1276);
        let d = date(2024, 3, 20);
        let s = resolve_schedule(&sample_layout(), &london, d).unwrap();
        let morning = s.block(SolarPeriod::Morning);
        assert_eq!(morning.images.len(), 3);
        assert_eq!(morning.images[0].start, morning.start);
        assert_eq!(morning.images[2].end, morning.end);
        let third = (morning.end - morning.start).num_seconds() / 3;
        for img in &morning.images {
            assert!(((img.end - img.start).num_seconds() - third).abs() <= 1);
        }
        // Images chain without gaps: each one is active until the next starts…
        let all: Vec<_> = s.blocks.iter().flat_map(|b| b.images.iter()).collect();
        for w in all.windows(2) {
            assert_eq!(w[0].active_until, w[1].start);
        }
        // …and the last one hands over to tomorrow's first image, ~24h after today's.
        let first = all[0];
        let last = all[all.len() - 1];
        assert_eq!(last.id.as_str(), "night");
        assert!(((last.active_until - first.start) - TimeDelta::days(1)).num_minutes().abs() <= 2);
        // The last morning image keeps showing through the empty Day block.
        let m3 = &morning.images[2];
        assert_eq!(m3.active_until, s.block(SolarPeriod::Evening).start);
        assert!(m3.active_until > m3.end);
    }

    #[test]
    fn empty_blocks_inherit_previous_image_including_across_midnight() {
        let s = resolve_schedule(&sample_layout(), &loc(51.5072, -0.1276), date(2024, 3, 20)).unwrap();
        assert_eq!(s.block(SolarPeriod::Morning).inherited, None);
        assert_eq!(s.block(SolarPeriod::Day).inherited.as_ref().map(ImageId::as_str), Some("m3"));

        let mut evening_only = PeriodLayout::new();
        *evening_only.block_mut(SolarPeriod::Evening) = ids(&["set"]);
        let s = resolve_schedule(&evening_only, &loc(51.5072, -0.1276), date(2024, 3, 20)).unwrap();
        // Wraps around: the morning shows last evening's image.
        assert_eq!(s.block(SolarPeriod::Morning).inherited.as_ref().map(ImageId::as_str), Some("set"));
        assert_eq!(s.block(SolarPeriod::Night).inherited.as_ref().map(ImageId::as_str), Some("set"));
    }

    #[test]
    fn editing_one_block_leaves_other_blocks_untouched() {
        let london = loc(51.5072, -0.1276);
        let d = date(2024, 3, 20);
        let before = resolve_schedule(&sample_layout(), &london, d).unwrap();
        let mut edited = sample_layout();
        edited.place(ImageId::new("m1"), SolarPeriod::Morning, 3);
        edited.block_mut(SolarPeriod::Morning).push(ImageId::new("m4"));
        let after = resolve_schedule(&edited, &london, d).unwrap();
        for p in SolarPeriod::ALL {
            if p == SolarPeriod::Morning {
                continue;
            }
            let (a, b) = (before.block(p), after.block(p));
            assert_eq!((a.start, a.end), (b.start, b.end));
            let starts = |blk: &ResolvedBlock| blk.images.iter().map(|i| (i.id.clone(), i.start, i.end)).collect::<Vec<_>>();
            assert_eq!(starts(a), starts(b), "{p} changed");
        }
    }

    #[test]
    fn polar_night_schedule_skips_the_day_block() {
        let tromso = loc(69.65, 18.96);
        let mut layout = sample_layout();
        *layout.block_mut(SolarPeriod::Day) = ids(&["noon"]);
        let s = resolve_schedule(&layout, &tromso, date(2024, 12, 21)).unwrap();
        let day = s.block(SolarPeriod::Day);
        assert!(day.is_degenerate() && day.images.is_empty());
        let m3 = s.block(SolarPeriod::Morning).images.last().unwrap();
        assert_eq!(m3.active_until, s.block(SolarPeriod::Evening).start);
    }

    #[test]
    fn polar_gaps_use_actual_images_on_neighboring_dates() {
        let location = loc(78.2, 15.6);
        let mut layout = PeriodLayout::new();
        *layout.block_mut(SolarPeriod::Day) = ids(&["day"]);
        *layout.block_mut(SolarPeriod::Night) = ids(&["night"]);
        let pack = crate::WallpaperPack::validate(layout.to_images()).unwrap();

        for d in [date(2024, 6, 21), date(2024, 12, 21)] {
            let schedule = resolve_schedule(&layout, &location, d).unwrap();
            for block in &schedule.blocks {
                if block.images.is_empty() && !block.is_degenerate() {
                    let at = block.start + (block.end - block.start) / 2;
                    let result = pack.query(Some(&location), at, TimeDelta::zero());
                    assert_eq!(block.inherited.as_ref(), Some(&result.active_before), "{d} {:?}", block.period);
                }
                for image in &block.images {
                    let at = image.start + (image.end - image.start) / 2;
                    let result = pack.query(Some(&location), at, TimeDelta::zero());
                    assert_eq!(Some(image.active_until), result.next_transition_at, "{d} {:?}", block.period);
                }
            }

        }
    }

    #[test]
    fn last_night_image_holds_until_night_returns_after_polar_day() {
        let location = loc(78.2, 15.6);
        let mut layout = PeriodLayout::new();
        *layout.block_mut(SolarPeriod::Night) = ids(&["night-1", "night-2"]);
        let pack = crate::WallpaperPack::validate(layout.to_images()).unwrap();
        let mut d = date(2024, 3, 1);
        while d < date(2024, 7, 1) {
            let today = PeriodBoundaries::compute(&location, d).unwrap();
            let tomorrow = PeriodBoundaries::compute(&location, d.succ_opt().unwrap()).unwrap();
            if !today.is_empty(SolarPeriod::Night) && tomorrow.is_empty(SolarPeriod::Night) {
                let schedule = resolve_schedule(&layout, &location, d).unwrap();
                let image = schedule.block(SolarPeriod::Night).images.last().unwrap();
                let query = pack.query(Some(&location), image.start + (image.end - image.start) / 2, TimeDelta::zero());
                assert_eq!(Some(image.active_until), query.next_transition_at);
                assert!(image.active_until > image.end + TimeDelta::days(30));
                return;
            }
            d = d.succ_opt().unwrap();
        }
        panic!("no polar-night gap found");
    }

    #[test]
    fn first_block_inherits_yesterdays_night_when_todays_night_disappears() {
        let location = loc(78.2, 15.6);
        let mut layout = PeriodLayout::new();
        *layout.block_mut(SolarPeriod::Day) = ids(&["day"]);
        *layout.block_mut(SolarPeriod::Night) = ids(&["night"]);
        let pack = crate::WallpaperPack::validate(layout.to_images()).unwrap();
        let mut d = date(2024, 3, 1);
        while d < date(2024, 7, 1) {
            let today = PeriodBoundaries::compute(&location, d).unwrap();
            let yesterday = PeriodBoundaries::compute(&location, d.pred_opt().unwrap()).unwrap();
            if today.is_empty(SolarPeriod::Night) && !yesterday.is_empty(SolarPeriod::Night) {
                let schedule = resolve_schedule(&layout, &location, d).unwrap();
                let morning = schedule.block(SolarPeriod::Morning);
                let at = morning.start + (morning.end - morning.start) / 2;
                let query = pack.query(Some(&location), at, TimeDelta::zero());
                assert_eq!(morning.inherited.as_ref().map(ImageId::as_str), Some("night"));
                assert_eq!(morning.inherited.as_ref(), Some(&query.active_before));
                return;
            }
            d = d.succ_opt().unwrap();
        }
        panic!("no polar-day transition found");
    }

    #[test]
    fn coordinate_change_moves_times() {
        let d = date(2024, 3, 20);
        let a = resolve_schedule(&sample_layout(), &loc(51.5, -0.1), d).unwrap();
        let b = resolve_schedule(&sample_layout(), &loc(51.5, 37.6), d).unwrap();
        let shift = a.block(SolarPeriod::Day).start - b.block(SolarPeriod::Day).start;
        // 37.7° of longitude ≈ 2.5 hours of solar time.
        assert!((shift.num_minutes() - 151).abs() <= 5, "{shift}");
    }

    #[test]
    fn validated_period_pack_query_follows_resolved_schedule() {
        let london = loc(51.5072, -0.1276);
        let d = date(2024, 3, 20);
        let layout = sample_layout();
        let pack = crate::WallpaperPack::validate(layout.to_images()).unwrap();
        assert_eq!(pack.anchor_kind(), crate::AnchorKind::Period);
        assert_eq!(pack.period_layout(), Some(layout.clone()));
        let s = resolve_schedule(&layout, &london, d).unwrap();
        for block in &s.blocks {
            for img in &block.images {
                let mid = img.start + (img.end - img.start) / 2;
                let r = pack.query(Some(&london), mid, TimeDelta::zero());
                assert_eq!(r.active_before, img.id);
                assert_eq!(r.next_transition_at, Some(img.active_until));
            }
        }
        // Inside an empty block the inherited image is what the engine shows.
        let day = s.block(SolarPeriod::Day);
        let r = pack.query(Some(&london), day.start + (day.end - day.start) / 2, TimeDelta::zero());
        assert_eq!(Some(r.active_before), day.inherited.clone());
    }

    #[test]
    fn validate_rejects_inconsistent_slots_and_mixing() {
        let p = |period, slot, of| TimeAnchor::Period { period, slot, of };
        let bad = vec![
            PackImage::new("a", p(SolarPeriod::Morning, 0, 2)),
            PackImage::new("b", p(SolarPeriod::Morning, 0, 2)),
        ];
        assert_eq!(
            crate::WallpaperPack::validate(bad),
            Err(crate::PackError::InvalidPeriodSlots { period: SolarPeriod::Morning })
        );
        let short = vec![PackImage::new("a", p(SolarPeriod::Day, 0, 2))];
        assert!(crate::WallpaperPack::validate(short).is_err());
        let mixed = vec![
            PackImage::new("a", p(SolarPeriod::Day, 0, 1)),
            PackImage::new("b", TimeAnchor::Clock(chrono::NaiveTime::MIN)),
        ];
        assert_eq!(crate::WallpaperPack::validate(mixed), Err(crate::PackError::MixedAnchorTypes));
    }
}
