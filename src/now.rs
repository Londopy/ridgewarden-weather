//! The live marine-layer indicator: what the latest residuals and the latest sounding
//! say about the low atmosphere right now, with the evidence and an honest confidence.
//!
//! From the links: in the newest complete residual bin (ended no more than 30 minutes
//! ago) how many links with data are `departure_db` or more from their usual level, and
//! whether the last three bins (at least half an hour) all met the ducting rule. Many
//! links down together with none up is called a widespread drop, not ducting. From the sounding: the inversion capping the
//! marine layer and the steepest refractivity gradient below `max_height_m`. Confidence
//! drops when few links report, when the sounding is old or missing, and when the two
//! disagree, or when a station near the mesh reported strong gusts in the last hour
//! (wind moving dishes swings links too). It is an indication, never a measurement.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use ridgewarden_client::util::{human_duration, short_time};
use rusqlite::Connection;
use serde::Serialize;

use crate::config::Config;
use crate::detect::{self, Series};
use crate::events;
use crate::series::{HOUR, MIN};
use crate::sounding::{self, Inversion, Sounding, TRAPPING_N_PER_KM};

/// Residuals older than this say nothing about now.
const FRESH: i64 = 30 * MIN;
/// A sounding older than this is weak evidence; older than a day, none.
const SOUNDING_FRESH: i64 = 12 * HOUR;
const SOUNDING_USABLE: i64 = 24 * HOUR;
/// An inversion this low caps a marine layer the backbone can reach.
const MARINE_TOP_M: f64 = 1000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Unknown,
    Calm,
    Some,
    Disturbed,
    Sustained,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    None,
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, Serialize)]
pub struct Now {
    pub state: State,
    pub confidence: Confidence,
    /// the verdict in one sentence
    pub verdict: String,
    /// what it rests on, and what lowered the confidence
    pub evidence: Vec<String>,
    pub bin_ms: Option<i64>,
    pub with_data: usize,
    pub departing: usize,
    pub up: usize,
    pub down: usize,
    pub sounding_station: Option<String>,
    pub sounding_ms: Option<i64>,
    pub inversion_base_m: Option<f64>,
    pub inversion_top_m: Option<f64>,
    pub inversion_c: Option<f64>,
    pub gradient_n_per_km: Option<f64>,
}

struct Bin {
    t: i64,
    with_data: usize,
    departing: usize,
    up: usize,
    /// the node at one end of every departing link, if there is one
    shared: Option<String>,
}

fn bins(series: &Series, departure_db: f64) -> Vec<Bin> {
    let mut by_t: BTreeMap<i64, (Bin, Option<BTreeSet<&String>>)> = BTreeMap::new();
    for (pair, pts) in series {
        for &(t, v) in pts {
            let (b, common) = by_t.entry(t).or_insert((
                Bin {
                    t,
                    with_data: 0,
                    departing: 0,
                    up: 0,
                    shared: None,
                },
                None,
            ));
            b.with_data += 1;
            if v.abs() >= departure_db {
                b.departing += 1;
                b.up += usize::from(v > 0.0);
                let ends = BTreeSet::from([&pair.0, &pair.1]);
                *common = Some(match common.take() {
                    None => ends,
                    Some(c) => c.intersection(&ends).copied().collect(),
                });
            }
        }
    }
    by_t.into_values()
        .map(|(mut b, common)| {
            b.shared = common
                .filter(|c| c.len() == 1 && b.departing > 1)
                .and_then(|c| c.into_iter().next().cloned());
            b
        })
        .collect()
}

/// Judge the present from residual bins (each link's, binned) and the latest sounding.
pub fn assess(
    series: &Series,
    sounding: Option<&Sounding>,
    gust: Option<&crate::rain::Report>,
    cfg: &Config,
    now: i64,
) -> Now {
    let d = &cfg.duct;
    let bins = bins(series, d.departure_db);
    let meets = |b: &Bin| {
        b.with_data >= d.min_with_data
            && b.departing >= d.min_links
            && b.departing as f64 >= d.min_frac * b.with_data as f64
    };
    let mut ev = vec![];
    let mut conf = Confidence::High;
    let bin = cfg.baseline.bin_min as i64 * MIN;
    // the bin still filling has only some of its links yet
    let bins: Vec<Bin> = bins.into_iter().filter(|b| b.t + bin <= now).collect();
    let fresh = FRESH.max(2 * bin);
    let latest = bins.last().filter(|b| now - (b.t + bin) <= fresh);
    let state = match latest {
        None => {
            ev.push(
                "No residuals in the last 30 minutes: links without a baseline yet, or no fresh samples."
                    .into(),
            );
            State::Unknown
        }
        Some(b) => {
            ev.push(format!(
                "At {}, {} of {} links with data were {:.0} dB or more from their usual level \
                 ({} above, {} below).",
                short_time(b.t),
                b.departing,
                b.with_data,
                d.departure_db,
                b.up,
                b.departing - b.up
            ));
            if b.with_data < d.min_with_data {
                conf = conf.min(Confidence::Low);
                ev.push(format!(
                    "Only {} links reporting; {} are needed to say much about the mesh.",
                    b.with_data, d.min_with_data
                ));
            }
            let span = FRESH.max(3 * bin);
            let recent: Vec<&Bin> = bins.iter().filter(|x| b.t - x.t < span).collect();
            if meets(b) && b.up == 0 {
                State::Down
            } else if recent.len() >= 3 && recent.iter().all(|x| meets(x)) {
                State::Sustained
            } else if meets(b) {
                State::Disturbed
            } else if b.departing > 0 {
                State::Some
            } else {
                State::Calm
            }
        }
    };
    let snd = sounding.filter(|s| now - s.ts <= SOUNDING_USABLE);
    let h = cfg.sounding_height();
    let inv: Option<Inversion> = snd.and_then(|s| s.inversion(h));
    let grad = snd.and_then(|s| s.steepest(h));
    let marine = inv.filter(|i| i.top_m <= MARINE_TOP_M);
    let trapping = grad.is_some_and(|g| g.n_per_km < TRAPPING_N_PER_KM);
    match snd {
        None => {
            conf = conf.min(Confidence::Medium);
            ev.push("No sounding in the last 24 hours.".into());
        }
        Some(s) => {
            let age = now - s.ts;
            ev.push(format!(
                "Sounding {} at {} ({} ago): {}.",
                s.station,
                short_time(s.ts),
                human_duration(age),
                s.summary(h)
            ));
            if age > SOUNDING_FRESH {
                conf = conf.min(Confidence::Medium);
                ev.push("The sounding is over 12 hours old; the layer may have moved.".into());
            }
        }
    }
    let disturbed = matches!(state, State::Disturbed | State::Sustained | State::Down);
    if disturbed && snd.is_some() && marine.is_none() && !trapping {
        conf = conf.min(Confidence::Medium);
        ev.push(
            "The sounding shows no low inversion or trapping layer, so something other than \
             the marine layer may be moving the links."
                .into(),
        );
    }
    if state == State::Calm && trapping {
        ev.push("The sounding had a trapping layer, but the links aren't showing it now.".into());
    }
    if let Some((g, station)) = gust.and_then(|r| Some((r.gust_mph?, &r.station)))
        && g >= cfg.gust_mph
    {
        if state != State::Calm {
            conf = conf.min(Confidence::Medium);
        }
        ev.push(format!(
            "Gusts to {g:.0} mph at {station} in the last hour: some of the swing may be dishes moving in the wind."
        ));
    }
    if state == State::Unknown {
        conf = Confidence::None;
    }
    let layer = match marine {
        Some(i) => format!(
            "The last sounding put a marine-layer inversion at {:.0} to {:.0} m above ground.",
            i.base_m, i.top_m
        ),
        None if snd.is_some() => "The last sounding showed no low inversion.".into(),
        None => String::new(),
    };
    let shared = bins.last().and_then(|b| b.shared.clone());
    let links = match (state, shared) {
        (State::Unknown, _) => "Can't tell from the links right now.".to_string(),
        (State::Calm, _) => "Links are at their usual levels: no sign of ducting or fading.".into(),
        (State::Some, _) => {
            "A few links are off their usual level, not enough to call it ducting.".into()
        }
        (State::Disturbed, _) => {
            "Many links are far from their usual level: ducting or fading now.".into()
        }
        (State::Down, Some(site)) => format!(
            "The links at {} are all down together and none up: more like water on that \
             site's antennas than ducting.",
            cfg.label(&site)
        ),
        (State::Down, None) => "Many links are down together and none up: more like rain on \
                                several sites, or something hitting every link, than ducting."
            .into(),
        (State::Sustained, _) => "Many links have been far from their usual level for half an \
                                  hour: ducting or fading under way."
            .into(),
    };
    let latest = bins.last().filter(|_| state != State::Unknown);
    Now {
        state,
        confidence: conf,
        verdict: format!("{links} {layer}").trim().to_string(),
        evidence: ev,
        bin_ms: latest.map(|b| b.t),
        with_data: latest.map_or(0, |b| b.with_data),
        departing: latest.map_or(0, |b| b.departing),
        up: latest.map_or(0, |b| b.up),
        down: latest.map_or(0, |b| b.departing - b.up),
        sounding_station: snd.map(|s| s.station.clone()),
        sounding_ms: snd.map(|s| s.ts),
        inversion_base_m: inv.map(|i| i.base_m),
        inversion_top_m: inv.map(|i| i.top_m),
        inversion_c: inv.map(|i| i.strength_c),
        gradient_n_per_km: grad.map(|g| g.n_per_km),
    }
}

/// The indicator from the database.
pub fn current(conn: &Connection, cfg: &Config, now: i64) -> Result<Now> {
    let links = detect::link_series(&events::residual_ends(conn, cfg, now - 2 * HOUR, now + 1)?);
    let latest = sounding::latest(conn)?.into_iter().max_by_key(|s| s.ts);
    let gust = crate::rain::strongest_gust(conn, &cfg.aprs_stations(), now - HOUR, now + 1)?;
    Ok(assess(&links, latest.as_ref(), gust.as_ref(), cfg, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_795_000_000_000 - 1_795_000_000_000 % (10 * MIN);
    /// when the bin starting at NOW is complete
    const DONE: i64 = NOW + 10 * MIN;

    fn cfg() -> Config {
        toml::from_str("").unwrap()
    }

    /// `n` links over the last hour; `off` of them `db` away from usual in the last `mins`.
    fn mesh(n: usize, off: usize, db: f64, mins: i64) -> Series {
        (0..n)
            .map(|i| {
                let pts = (0..6)
                    .map(|k| {
                        let t = NOW - (5 - k) * 10 * MIN;
                        let v = if i < off && NOW - t < mins * MIN {
                            if i % 2 == 0 { db } else { -db }
                        } else {
                            0.2
                        };
                        (t, v)
                    })
                    .collect();
                (("a".to_string(), format!("n{i}")), pts)
            })
            .collect()
    }

    fn sounding(age: i64, inverted: bool) -> Sounding {
        let mut s =
            crate::sounding::parse(include_str!("../tests/fixtures/uwyo-72393.html")).unwrap();
        s.ts = NOW - age;
        if !inverted {
            // a plain lapse rate: cooling with height everywhere, dry aloft
            for l in s.levels.iter_mut() {
                l.t = 15.0 - 0.0065 * (l.h - 121.0);
                l.td = Some(5.0 - 0.002 * (l.h - 121.0));
            }
        }
        s
    }

    #[test]
    fn calm_links_and_a_fresh_sounding_are_a_confident_calm() {
        let n = assess(
            &mesh(6, 0, 0.0, 0),
            Some(&sounding(2 * HOUR, true)),
            None,
            &cfg(),
            DONE,
        );
        assert_eq!((n.state, n.confidence), (State::Calm, Confidence::High));
        assert!(
            n.verdict.contains("marine-layer inversion at 0 to 111 m"),
            "{}",
            n.verdict
        );
        // the fixture sounding has a trapping layer the calm links don't show
        assert!(n.evidence.iter().any(|e| e.contains("trapping layer")));
    }

    #[test]
    fn half_an_hour_of_departures_is_sustained_and_agrees_with_the_inversion() {
        let n = assess(
            &mesh(6, 4, 6.0, 40),
            Some(&sounding(HOUR, true)),
            None,
            &cfg(),
            DONE,
        );
        assert_eq!(
            (n.state, n.confidence),
            (State::Sustained, Confidence::High)
        );
        assert_eq!((n.departing, n.up, n.down, n.with_data), (4, 2, 2, 6));
        let just = assess(
            &mesh(6, 4, 6.0, 5),
            Some(&sounding(HOUR, true)),
            None,
            &cfg(),
            DONE,
        );
        assert_eq!(just.state, State::Disturbed);
        let one = assess(
            &mesh(6, 1, 6.0, 40),
            Some(&sounding(HOUR, true)),
            None,
            &cfg(),
            DONE,
        );
        assert_eq!(one.state, State::Some);
    }

    #[test]
    fn the_bin_still_filling_is_left_out_and_all_down_is_not_ducting() {
        // seconds into a new bin, only two links have reported in it
        let mut s = mesh(6, 4, 6.0, 50);
        for (i, pts) in s.values_mut().enumerate() {
            if i < 2 {
                pts.push((DONE, 0.1));
            }
        }
        let n = assess(&s, Some(&sounding(HOUR, true)), None, &cfg(), DONE + 30_000);
        assert_eq!((n.state, n.with_data), (State::Sustained, 6));
        // four links all down, none up, at four different sites
        let down: Series = mesh(6, 4, 6.0, 40)
            .into_iter()
            .enumerate()
            .map(|(i, (_, pts))| {
                let pts = pts.into_iter().map(|(t, v)| (t, -v.abs())).collect();
                ((format!("x{i}"), format!("n{i}")), pts)
            })
            .collect();
        let n = assess(&down, Some(&sounding(HOUR, true)), None, &cfg(), DONE);
        assert_eq!(n.state, State::Down);
        assert!(n.verdict.contains("rain on several sites"), "{}", n.verdict);
        // the same, but every down link touches one site
        let hub: Series = down
            .into_iter()
            .enumerate()
            .map(|(i, (_, pts))| (("hub".to_string(), format!("n{i}")), pts))
            .collect();
        let n = assess(&hub, Some(&sounding(HOUR, true)), None, &cfg(), DONE);
        assert!(n.verdict.contains("links at hub"), "{}", n.verdict);
    }

    #[test]
    fn strong_gusts_lower_the_confidence_in_a_swing() {
        let gust = crate::rain::Report {
            station: "CW0001".into(),
            gust_mph: Some(41.0),
            ..Default::default()
        };
        let calm_air = assess(
            &mesh(6, 4, 6.0, 40),
            Some(&sounding(HOUR, true)),
            None,
            &cfg(),
            DONE,
        );
        assert_eq!(calm_air.confidence, Confidence::High);
        let windy = assess(
            &mesh(6, 4, 6.0, 40),
            Some(&sounding(HOUR, true)),
            Some(&gust),
            &cfg(),
            DONE,
        );
        assert_eq!(windy.confidence, Confidence::Medium);
        assert!(windy.evidence.iter().any(|e| e.contains("41 mph")));
        let breeze = crate::rain::Report {
            gust_mph: Some(12.0),
            ..gust
        };
        let n = assess(
            &mesh(6, 4, 6.0, 40),
            Some(&sounding(HOUR, true)),
            Some(&breeze),
            &cfg(),
            DONE,
        );
        assert_eq!(n.confidence, Confidence::High);
    }

    #[test]
    fn confidence_falls_with_old_soundings_disagreement_and_few_links() {
        let no_inv = assess(
            &mesh(6, 4, 6.0, 40),
            Some(&sounding(HOUR, false)),
            None,
            &cfg(),
            DONE,
        );
        assert_eq!(no_inv.confidence, Confidence::Medium);
        let old = assess(
            &mesh(6, 0, 0.0, 0),
            Some(&sounding(18 * HOUR, true)),
            None,
            &cfg(),
            DONE,
        );
        assert_eq!(old.confidence, Confidence::Medium);
        let none = assess(
            &mesh(6, 0, 0.0, 0),
            Some(&sounding(30 * HOUR, true)),
            None,
            &cfg(),
            DONE,
        );
        assert_eq!(none.sounding_station, None);
        assert_eq!(none.confidence, Confidence::Medium);
        let few = assess(
            &mesh(3, 3, 6.0, 40),
            Some(&sounding(HOUR, true)),
            None,
            &cfg(),
            DONE,
        );
        assert_eq!(few.confidence, Confidence::Low);
        assert_eq!(
            few.state,
            State::Some,
            "3 links can't make the ducting rule"
        );
        let stale = assess(&mesh(6, 0, 0.0, 0), None, None, &cfg(), DONE + HOUR);
        assert_eq!(
            (stale.state, stale.confidence),
            (State::Unknown, Confidence::None)
        );
    }
}
