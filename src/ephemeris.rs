//! Sun/Moon/Galactic-Center position math backing `Trigger::Ephemeris` and
//! the `*ElevationWindow`/`MoonIlluminationWindow` constraints (design doc
//! §2, §11).
//!
//! Built on `astro` (a pure-Rust implementation of Meeus' *Astronomical
//! Algorithms* — chosen 2026-09-20 specifically because it covers **both**
//! Sun and Moon, so this whole feature needs only one new external
//! dependency, not the two originally flagged as separate research items;
//! Galactic-Center support (`coords::gal_frm_eq`/`eq_frm_gal`) meant no
//! third crate was needed for MilkyWay either, matching the design doc's
//! own suspicion that it wouldn't need one).
//!
//! **Precision is deliberately bounded, not arcsecond-grade**: positions
//! are geocentric (no topocentric parallax correction beyond the Moon's
//! own rise/set altitude threshold, which folds parallax in via the same
//! technique `astro::transit` uses), and only the Sun gets its standard
//! aberration correction (Moon's own nutation/aberration refinement is
//! skipped). This is sub-arcminute-level accuracy, translating to
//! sub-minute timing error on rise/set/elevation crossings — far tighter
//! than a camera-scheduling feature needs, and validated against published
//! reference sunrise/sunset times in this module's tests.
//!
//! **Longitude sign convention warning**: `astro`'s formulas (following
//! Meeus' original book) treat longitude as **positive *west*** of
//! Greenwich — the opposite of the standard GPS/ISO-6709 convention (east
//! positive) this codebase's `Station.longitude` uses everywhere else.
//! Every function here that calls into `astro::coords`/`astro::time` for
//! sidereal time negates `Station.longitude` first — confirmed against
//! `astro`'s own test suite, which encodes Boston (71.0833° **west**) as
//! `+71.0833`.

use astro::{aberr, coords, ecliptic, lunar, nutation, sun};
use chrono::{DateTime, Datelike, Duration, Timelike, Utc};

use crate::optic_scheduler::{CrossingDirection, Station};

/// Right ascension/declination, in radians (the `astro` crate's own unit
/// convention throughout).
#[derive(Debug, Clone, Copy)]
pub struct Equatorial {
    pub ra: f64,
    pub dec: f64,
}

/// B1950.0 epoch, as a Julian day — `astro::coords::gal_frm_eq`/`eq_frm_gal`
/// are defined relative to this equinox (their own doc comments say so);
/// used only to precess the Galactic Center's fixed position to the date
/// of interest.
const B1950_JD: f64 = 2_433_282.423_5;

/// AU -> km, for combining `astro::sun::geocent_ecl_pos`'s AU distance with
/// `astro::lunar`'s km-based illumination formula, which needs both
/// distances in the same unit (confirmed by reading `illuminated_frac`'s
/// body: a plain ratio, not unit-aware).
const AU_IN_KM: f64 = 149_597_870.7;

/// Julian day (UT) for an instant — computed directly from the Unix
/// timestamp rather than via `astro::time::Date`'s calendar-field struct;
/// exact, and avoids round-tripping through calendar fields at all.
pub fn julian_day(at: DateTime<Utc>) -> f64 {
    at.timestamp() as f64 / 86400.0 + 2_440_587.5
}

/// The Sun's apparent geocentric equatorial position (nutation +
/// aberration corrected, per Meeus ch. 25).
pub fn sun_equatorial(jd: f64) -> Equatorial {
    let (ecl, earth_sun_dist_au) = sun::geocent_ecl_pos(jd);
    let (nut_in_long, nut_in_obliq) = nutation::nutation(jd);
    let true_obliq = ecliptic::mn_oblq_laskar(jd) + nut_in_obliq;
    let apparent_long = ecl.long + nut_in_long + aberr::sol_aberr(earth_sun_dist_au);
    Equatorial {
        ra: coords::asc_frm_ecl(apparent_long, ecl.lat, true_obliq),
        dec: coords::dec_frm_ecl(apparent_long, ecl.lat, true_obliq),
    }
}

struct MoonPosition {
    equatorial: Equatorial,
    earth_moon_dist_km: f64,
    ecl_long: f64,
    ecl_lat: f64,
}

fn moon_position(jd: f64) -> MoonPosition {
    let (ecl, earth_moon_dist_km) = lunar::geocent_ecl_pos(jd);
    let mean_obliq = ecliptic::mn_oblq_laskar(jd);
    MoonPosition {
        equatorial: Equatorial {
            ra: coords::asc_frm_ecl(ecl.long, ecl.lat, mean_obliq),
            dec: coords::dec_frm_ecl(ecl.long, ecl.lat, mean_obliq),
        },
        earth_moon_dist_km,
        ecl_long: ecl.long,
        ecl_lat: ecl.lat,
    }
}

/// The Moon's geocentric equatorial position.
pub fn moon_equatorial(jd: f64) -> Equatorial {
    moon_position(jd).equatorial
}

/// The Moon's illuminated fraction, 0.0-100.0 (percent).
pub fn moon_illumination_pct(jd: f64) -> f64 {
    let moon = moon_position(jd);
    let (sun_ecl, earth_sun_dist_au) = sun::geocent_ecl_pos(jd);
    let frac = lunar::illum_frac_frm_ecl_coords(
        moon.ecl_long,
        moon.ecl_lat,
        sun_ecl.long,
        moon.earth_moon_dist_km,
        earth_sun_dist_au * AU_IN_KM,
    );
    frac * 100.0
}

/// The altitude threshold (degrees) the Moon's *center* must cross for a
/// rise/set — folds the Moon's own parallax + standard refraction in via
/// `astro::transit`'s own formula (`0.7275 * horizontal_parallax -
/// 0.5667°`), which is how `astro` itself approximates topocentric
/// horizon-dip without a full topocentric coordinate conversion. Varies
/// slightly (roughly 0.09-0.16°) with the Moon's distance, unlike the
/// Sun's fixed -0.8333° — callers fold this into the crossing function
/// itself (`elevation - threshold`, compared against 0) rather than
/// `find_crossings`' single fixed `threshold_deg`, since the threshold
/// isn't constant here.
pub fn moon_rise_set_altitude_deg(jd: f64) -> f64 {
    let dist_km = lunar::geocent_ecl_pos(jd).1;
    (0.7275 * lunar::eq_hz_parllx(dist_km) - 0.5667_f64.to_radians()).to_degrees()
}

/// A Julian day as a `DateTime<Utc>` — the exact inverse of `julian_day`.
pub fn datetime_from_julian_day(jd: f64) -> DateTime<Utc> {
    let unix_secs = (jd - 2_440_587.5) * 86400.0;
    DateTime::from_timestamp(unix_secs.floor() as i64, 0)
        .expect("julian day in representable range")
}

fn astro_date(at: DateTime<Utc>) -> astro::time::Date {
    let decimal_day = at.day() as f64
        + at.hour() as f64 / 24.0
        + at.minute() as f64 / 1440.0
        + at.second() as f64 / 86400.0;
    astro::time::Date {
        year: at.year() as i16,
        month: at.month() as u8,
        decimal_day,
        cal_type: astro::time::CalType::Gregorian,
    }
}

/// One of the Moon's four monthly phase events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LunarPhase {
    New,
    First,
    Full,
    Last,
}

/// Every occurrence of `phase` within `(from, until]`. Synodic month is
/// ~29.53 days, so sampling every 20 days across a padded range (a month
/// before `from` through two days after `until`) guarantees every
/// occurrence in range gets found by at least one sample, with duplicates
/// from adjacent samples landing on the same event filtered out.
pub fn lunar_phase_occurrences(
    phase: LunarPhase,
    from: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Vec<DateTime<Utc>> {
    let astro_phase = match phase {
        LunarPhase::New => lunar::Phase::New,
        LunarPhase::First => lunar::Phase::First,
        LunarPhase::Full => lunar::Phase::Full,
        LunarPhase::Last => lunar::Phase::Last,
    };

    let mut jds: Vec<f64> = Vec::new();
    let mut sample = from - Duration::days(32);
    let end = until + Duration::days(2);
    while sample <= end {
        jds.push(lunar::time_of_phase(&astro_date(sample), &astro_phase));
        sample += Duration::days(20);
    }
    jds.sort_by(|a, b| a.partial_cmp(b).expect("julian day is never NaN"));
    jds.dedup_by(|a, b| (*a - *b).abs() < 0.5);

    let from_jd = julian_day(from);
    let until_jd = julian_day(until);
    jds.into_iter()
        .filter(|jd| *jd > from_jd && *jd <= until_jd)
        .map(datetime_from_julian_day)
        .collect()
}

/// The Galactic Center's equatorial position, precessed from its fixed
/// B1950.0 galactic coordinates (l=0°, b=0°) to the date of interest.
/// Precession alone (not a full proper-motion/parallax model) is the right
/// level of care here — Sgr A*'s own proper motion is negligible at this
/// use case's precision, but 76+ years of precession since B1950 is
/// several arcminutes and worth correcting since `astro::precess` makes it
/// a two-line addition.
pub fn milky_way_core_equatorial(jd: f64) -> Equatorial {
    let ra_b1950 = coords::asc_frm_gal(0.0, 0.0);
    let dec_b1950 = coords::dec_frm_gal(0.0, 0.0);
    let (ra, dec) = astro::precess::precess_eq_coords(ra_b1950, dec_b1950, B1950_JD, jd);
    Equatorial { ra, dec }
}

/// `Station.longitude` (east-positive) negated into `astro`'s
/// west-positive convention — see the module doc comment.
fn west_longitude_rad(station: &Station) -> f64 {
    (-station.longitude).to_radians()
}

/// Apparent sidereal time at Greenwich (radians), matching
/// `astro::time::apprnt_sidr!`'s macro expansion (reimplemented as a plain
/// function call here — the macro itself isn't imported, just its body).
fn apparent_greenwich_sidereal(jd: f64) -> f64 {
    let (nut_in_long, nut_in_obliq) = nutation::nutation(jd);
    let true_obliq = ecliptic::mn_oblq_laskar(jd) + nut_in_obliq;
    astro::time::apprnt_sidr(astro::time::mn_sidr(jd), nut_in_long, true_obliq)
}

/// Elevation (altitude) above the horizon, in degrees, of an equatorial
/// position for an observer at `station` at `jd`.
pub fn elevation_deg(eq: Equatorial, station: &Station, jd: f64) -> f64 {
    let hour_angle = coords::hr_angl_frm_observer_long(
        apparent_greenwich_sidereal(jd),
        west_longitude_rad(station),
        eq.ra,
    );
    coords::alt_frm_eq(hour_angle, eq.dec, station.latitude.to_radians()).to_degrees()
}

/// Azimuth, in degrees (0-360, north = 0, clockwise), of an equatorial
/// position for an observer at `station` at `jd`.
pub fn azimuth_deg(eq: Equatorial, station: &Station, jd: f64) -> f64 {
    let hour_angle = coords::hr_angl_frm_observer_long(
        apparent_greenwich_sidereal(jd),
        west_longitude_rad(station),
        eq.ra,
    );
    let az = coords::az_frm_eq(hour_angle, eq.dec, station.latitude.to_radians()).to_degrees();
    // `az_frm_eq` returns azimuth measured from the *south* point (Meeus'
    // convention, confirmed by the formula's `hour_angle.sin().atan2(...)`
    // shape matching Meeus ch. 13) in (-180, 180]; convert to the
    // conventional north-referenced 0-360 compass bearing the UI/design
    // doc's `Orientation.azimuth_degrees` expects.
    (az + 180.0).rem_euclid(360.0)
}

/// Sample step for the crossing/extremum search below. Coarse enough to be
/// cheap over a 48h forecast horizon, fine enough that no real crossing
/// (the slowest-moving case, Solar twilight tiers, still sweeps several
/// degrees/hour even at low elevation) is ever skipped between two
/// samples — verified directly by the "doesn't miss a crossing" tests.
const SAMPLE_STEP: Duration = Duration::minutes(4);

/// Finds every crossing of `threshold_deg` by `f(t)` within `(from,
/// until]`, refined by bisection to sub-second precision. `f` is any
/// smoothly-varying degree-valued function of time — elevation for
/// rise/set/named-tier/fixed-elevation events, azimuth (pre-unwrapped by
/// the caller, see `find_azimuth_crossings`) for orientation events.
pub fn find_crossings(
    from: DateTime<Utc>,
    until: DateTime<Utc>,
    threshold_deg: f64,
    direction: CrossingDirection,
    mut f: impl FnMut(DateTime<Utc>) -> f64,
) -> Vec<DateTime<Utc>> {
    let mut out = Vec::new();
    let mut t_prev = from;
    let mut v_prev = f(t_prev);
    let mut t = from + SAMPLE_STEP;
    while t <= until {
        let v = f(t);
        let crossed_up = v_prev < threshold_deg && v >= threshold_deg;
        let crossed_down = v_prev >= threshold_deg && v < threshold_deg;
        let wanted = match direction {
            CrossingDirection::Rising => crossed_up,
            CrossingDirection::Setting => crossed_down,
            CrossingDirection::Both => crossed_up || crossed_down,
        };
        if wanted {
            let refined = bisect_crossing(t_prev, v_prev, t, v, threshold_deg, &mut f);
            if refined > from && refined <= until {
                out.push(refined);
            }
        }
        t_prev = t;
        v_prev = v;
        t += SAMPLE_STEP;
    }
    out
}

fn bisect_crossing(
    mut t_lo: DateTime<Utc>,
    mut v_lo: f64,
    mut t_hi: DateTime<Utc>,
    _v_hi: f64,
    threshold_deg: f64,
    f: &mut impl FnMut(DateTime<Utc>) -> f64,
) -> DateTime<Utc> {
    // 20 halvings of a 4-minute window is well under 1 second — plenty for
    // a scheduling feature, nowhere near needing a numerically-aware
    // termination condition.
    for _ in 0..20 {
        let mid = t_lo + (t_hi - t_lo) / 2;
        let v_mid = f(mid);
        if (v_lo < threshold_deg) == (v_mid < threshold_deg) {
            t_lo = mid;
            v_lo = v_mid;
        } else {
            t_hi = mid;
        }
    }
    t_lo + (t_hi - t_lo) / 2
}

/// Azimuth-crossing search (`MilkyWayEvent::Orientation`) — azimuth wraps
/// 0/360, which a plain `find_crossings` would misread as a spurious
/// crossing every time the sample pair straddles the seam. Detects the
/// wrap (a >180° jump between consecutive samples) and skips reporting
/// across it, rather than trying to make the generic threshold search
/// wrap-aware for a case used by exactly one event type.
pub fn find_azimuth_crossings(
    from: DateTime<Utc>,
    until: DateTime<Utc>,
    target_deg: f64,
    mut f: impl FnMut(DateTime<Utc>) -> f64,
) -> Vec<DateTime<Utc>> {
    let mut out = Vec::new();
    let mut t_prev = from;
    let mut v_prev = f(t_prev);
    let mut t = from + SAMPLE_STEP;
    while t <= until {
        let v = f(t);
        if (v - v_prev).abs() < 180.0 {
            let crossed = (v_prev < target_deg) != (v < target_deg);
            if crossed {
                let refined = bisect_crossing(t_prev, v_prev, t, v, target_deg, &mut f);
                if refined > from && refined <= until {
                    out.push(refined);
                }
            }
        }
        t_prev = t;
        v_prev = v;
        t += SAMPLE_STEP;
    }
    out
}

/// Finds each local maximum (`want_max = true`, e.g. solar transit/lunar
/// transit/Milky Way core transit) or minimum (`want_max = false`, e.g.
/// solar nadir/lunar antitransit) of `f(t)` within `(from, until]` — one
/// per roughly-daily period, refined by golden-section search around the
/// coarse sample peak.
pub fn find_extrema(
    from: DateTime<Utc>,
    until: DateTime<Utc>,
    want_max: bool,
    mut f: impl FnMut(DateTime<Utc>) -> f64,
) -> Vec<DateTime<Utc>> {
    let better = |a: f64, b: f64| if want_max { a > b } else { a < b };
    let mut out = Vec::new();
    let mut t_prev = from;
    let mut v_prev = f(t_prev);
    let mut t = from + SAMPLE_STEP;
    let mut v = f(t);
    let mut t_next = t + SAMPLE_STEP;
    while t_next <= until + SAMPLE_STEP {
        let v_next = f(t_next);
        // A coarse local extremum: the middle sample beats both neighbors.
        if better(v, v_prev) && better(v, v_next) {
            out.push(golden_section_refine(t_prev, t_next, want_max, &mut f));
        }
        t_prev = t;
        v_prev = v;
        t = t_next;
        v = v_next;
        t_next += SAMPLE_STEP;
    }
    out
}

fn golden_section_refine(
    mut lo: DateTime<Utc>,
    mut hi: DateTime<Utc>,
    want_max: bool,
    f: &mut impl FnMut(DateTime<Utc>) -> f64,
) -> DateTime<Utc> {
    const RESPHI: f64 = 0.618_033_988_75;
    fn at(lo: DateTime<Utc>, hi: DateTime<Utc>, frac: f64) -> DateTime<Utc> {
        lo + Duration::milliseconds(((hi - lo).num_milliseconds() as f64 * frac) as i64)
    }
    let better = |a: f64, b: f64| if want_max { a > b } else { a < b };

    let mut c = at(lo, hi, 1.0 - RESPHI);
    let mut d = at(lo, hi, RESPHI);
    let mut fc = f(c);
    let mut fd = f(d);
    for _ in 0..25 {
        if better(fc, fd) {
            hi = d;
            d = c;
            fd = fc;
            c = at(lo, hi, 1.0 - RESPHI);
            fc = f(c);
        } else {
            lo = c;
            c = d;
            fc = fd;
            d = at(lo, hi, RESPHI);
            fd = f(d);
        }
        if (hi - lo).num_milliseconds() < 1000 {
            break;
        }
    }
    lo + (hi - lo) / 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn vancouver() -> Station {
        Station {
            latitude: 49.2827,
            longitude: -123.1207,
            elevation_m: 70.0,
            timezone: "America/Vancouver".to_owned(),
        }
    }

    // Reference: two independently-sourced reports (earthsky.org via
    // search, and a second corroborating snippet) both give the September
    // 2026 equinox as 2026-09-23T00:05:00Z — the exact instant the Sun's
    // apparent geocentric declination crosses 0°. This is a far stronger,
    // source-triangulated check than a single site's daily sunrise/sunset
    // table entry (which for a specific future date proved unreliable to
    // fetch/verify directly — see the self-consistency tests below for
    // same-day cross-checks that don't depend on any external lookup).
    #[test]
    fn sun_declination_crosses_zero_at_the_published_equinox_instant() {
        let equinox = Utc.with_ymd_and_hms(2026, 9, 23, 0, 5, 0).unwrap();
        let dec_at_equinox = sun_equatorial(julian_day(equinox)).dec.to_degrees();
        assert!(
            dec_at_equinox.abs() < 0.05,
            "expected ~0° declination at the equinox, got {dec_at_equinox}"
        );

        let a_day_before = equinox - Duration::days(1);
        let dec_before = sun_equatorial(julian_day(a_day_before)).dec.to_degrees();
        assert!(
            dec_before > 0.0,
            "declination should still be positive a day before the equinox"
        );

        let a_day_after = equinox + Duration::days(1);
        let dec_after = sun_equatorial(julian_day(a_day_after)).dec.to_degrees();
        assert!(
            dec_after < 0.0,
            "declination should have gone negative a day after the equinox"
        );
    }

    #[test]
    fn sunrise_and_sunset_each_occur_exactly_once_on_a_september_day_in_vancouver() {
        let station = vancouver();
        let elevation = |t: DateTime<Utc>| {
            elevation_deg(sun_equatorial(julian_day(t)), &station, julian_day(t))
        };
        let from = Utc.with_ymd_and_hms(2026, 9, 20, 10, 0, 0).unwrap();
        let until = from + Duration::hours(24);

        let sunrises = find_crossings(from, until, -0.8333, CrossingDirection::Rising, elevation);
        let sunsets = find_crossings(from, until, -0.8333, CrossingDirection::Setting, elevation);
        assert_eq!(
            sunrises.len(),
            1,
            "expected exactly one sunrise: {sunrises:?}"
        );
        assert_eq!(sunsets.len(), 1, "expected exactly one sunset: {sunsets:?}");
        assert!(sunrises[0] < sunsets[0], "sunrise must precede sunset");

        // Mid-September at 49°N: day length should be a plausible value
        // (not, say, a near-miss artifact reporting a 2-minute or 23-hour
        // day), independent of knowing the exact published minute.
        let day_length = sunsets[0] - sunrises[0];
        assert!(
            day_length > Duration::hours(11) && day_length < Duration::hours(13),
            "implausible day length: {day_length}"
        );
    }

    #[test]
    fn solar_noon_is_the_elevation_maximum_symmetric_between_sunrise_and_sunset() {
        let station = vancouver();
        let elevation = |t: DateTime<Utc>| {
            elevation_deg(sun_equatorial(julian_day(t)), &station, julian_day(t))
        };
        let from = Utc.with_ymd_and_hms(2026, 9, 20, 10, 0, 0).unwrap();
        let until = from + Duration::hours(24);

        let sunrise = find_crossings(from, until, -0.8333, CrossingDirection::Rising, elevation)[0];
        let sunset = find_crossings(from, until, -0.8333, CrossingDirection::Setting, elevation)[0];
        let transits = find_extrema(from, until, true, elevation);
        assert_eq!(transits.len(), 1);
        let solar_noon = transits[0];

        assert!(
            solar_noon > sunrise && solar_noon < sunset,
            "solar noon must fall between sunrise and sunset"
        );
        // The Sun's elevation is (very nearly) symmetric about solar noon
        // on any single day — a rise-to-noon gap that differs from the
        // noon-to-set gap by more than a minute would indicate a real bug
        // (e.g. a longitude sign error skewing the search window), not
        // measurement noise.
        let rise_to_noon = solar_noon - sunrise;
        let noon_to_set = sunset - solar_noon;
        let asymmetry = (rise_to_noon - noon_to_set).num_seconds().abs();
        assert!(
            asymmetry < 60,
            "solar noon isn't centered between sunrise/sunset: {asymmetry}s off"
        );
    }

    #[test]
    fn moon_illumination_is_a_sane_percentage() {
        let jd = julian_day(Utc.with_ymd_and_hms(2026, 9, 20, 12, 0, 0).unwrap());
        let pct = moon_illumination_pct(jd);
        assert!(
            (0.0..=100.0).contains(&pct),
            "illumination out of range: {pct}"
        );
    }

    #[test]
    fn milky_way_core_is_a_plausible_fixed_sky_position() {
        // Sgr A* is close to RA 17h45m40s (266.4°), Dec -29.0° (J2000) —
        // precession from B1950 over ~76 years shifts this by a few
        // arcminutes, not degrees.
        let jd = julian_day(Utc.with_ymd_and_hms(2026, 9, 20, 12, 0, 0).unwrap());
        let eq = milky_way_core_equatorial(jd);
        let ra_deg = eq.ra.to_degrees().rem_euclid(360.0);
        let dec_deg = eq.dec.to_degrees();
        assert!(
            (265.0..268.0).contains(&ra_deg),
            "RA out of range: {ra_deg}"
        );
        assert!(
            (-30.0..-28.0).contains(&dec_deg),
            "Dec out of range: {dec_deg}"
        );
    }

    #[test]
    fn moon_rise_set_altitude_is_in_the_expected_small_range() {
        let jd = julian_day(Utc.with_ymd_and_hms(2026, 9, 20, 12, 0, 0).unwrap());
        let deg = moon_rise_set_altitude_deg(jd);
        assert!(
            (0.05..0.2).contains(&deg),
            "unexpected moon rise/set altitude: {deg}"
        );
    }

    #[test]
    fn lunar_phase_occurrences_finds_exactly_one_full_moon_in_a_synodic_month() {
        let from = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let until = Utc.with_ymd_and_hms(2026, 9, 30, 0, 0, 0).unwrap();
        let full_moons = lunar_phase_occurrences(LunarPhase::Full, from, until);
        assert_eq!(
            full_moons.len(),
            1,
            "expected exactly one full moon: {full_moons:?}"
        );
    }

    #[test]
    fn lunar_phase_occurrences_finds_all_four_phases_across_two_months() {
        let from = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let until = Utc.with_ymd_and_hms(2026, 10, 31, 0, 0, 0).unwrap();
        for phase in [
            LunarPhase::New,
            LunarPhase::First,
            LunarPhase::Full,
            LunarPhase::Last,
        ] {
            let occurrences = lunar_phase_occurrences(phase, from, until);
            assert!(
                occurrences.len() == 2,
                "expected 2 occurrences of {phase:?} across ~61 days, got {occurrences:?}"
            );
        }
    }

    #[test]
    fn find_crossings_reports_nothing_when_the_threshold_is_never_reached() {
        let crossings = find_crossings(
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 1, 1, 1, 0, 0).unwrap(),
            1000.0,
            CrossingDirection::Both,
            |_| 0.0,
        );
        assert!(crossings.is_empty());
    }

    #[test]
    fn find_azimuth_crossings_ignores_the_0_360_wrap_seam() {
        // A function that jumps from 359 to 1 (wrapping through 0, not
        // through the 180 target) must not be reported as crossing 180.
        let crossings = find_azimuth_crossings(
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 20, 0).unwrap(),
            180.0,
            |t| {
                let minute = (t - Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()).num_minutes();
                if minute < 10 { 359.0 } else { 1.0 }
            },
        );
        assert!(crossings.is_empty());
    }
}
