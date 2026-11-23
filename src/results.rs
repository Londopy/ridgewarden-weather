//! The spec's success measure, "a statistically clear effect on at least one link class,
//! or a well-documented negative result", computed from the stored history.
//!
//! Each test works on hourly residuals: an hour's median level minus the dry baseline
//! for that hour, both ends of a link averaged. Two groups of hours are compared by the
//! difference of their means with a 95% interval (Welch's, with Student's t on the
//! Welch-Satterthwaite degrees of freedom); the effect is clear when the interval leaves
//! out zero. The tests over all links take one value per hour, the mean over the links,
//! since every link sees the same weather that hour. Hours next to each other still
//! aren't independent, so the interval is narrower than it should be, and with many
//! links one of them will look clear by chance now and then; treat "clear" as worth a
//! closer look, not as proof.
//!
//! - Rain: hours a station near either end reported rain against hours its reports were
//!   dry (not counting the drying time after rain), per link and over all links together.
//!   Radar adds rain hours only: its zero isn't dry, so the dry hours come from gauges.
//! - Night: the size of departures (|residual|) at night (20-06 local) against the day
//!   (08-18 local), over all links; ducting and fading along the coast favour the night.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use ridgewarden_client::graph::edge;
use rusqlite::Connection;

use crate::config::Config;
use crate::detect::Pair;
use crate::rain;
use crate::series::{self, DAY, HOUR, History, day_of};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Comparison {
    pub n_a: usize,
    pub mean_a: f64,
    pub n_b: usize,
    pub mean_b: f64,
    /// mean_a - mean_b
    pub diff: f64,
    /// half-width of the 95% interval around `diff`
    pub ci95: f64,
}

impl Comparison {
    pub fn clear(&self) -> bool {
        self.diff.abs() > self.ci95
    }
}

fn mean_var(v: &[f64]) -> (f64, f64) {
    let m = v.iter().sum::<f64>() / v.len() as f64;
    let var = v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (v.len() - 1) as f64;
    (m, var)
}

/// Group a against group b; None with fewer than three values in either.
/// Student's t at 97.5%, for a two-sided 95% interval. Fractional degrees of freedom
/// round down, which widens the interval a little.
fn t975(df: f64) -> f64 {
    const T: [f64; 30] = [
        12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160,
        2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056,
        2.052, 2.048, 2.045, 2.042,
    ];
    if df < 31.0 {
        return T[(df.floor().max(1.0) as usize) - 1];
    }
    // Cornish-Fisher expansion around the normal quantile; good to 0.001 from 30 up
    let z: f64 = 1.959_964;
    z + (z.powi(3) + z) / (4.0 * df)
        + (5.0 * z.powi(5) + 16.0 * z.powi(3) + 3.0 * z) / (96.0 * df * df)
}

pub fn compare(a: &[f64], b: &[f64]) -> Option<Comparison> {
    if a.len() < 3 || b.len() < 3 {
        return None;
    }
    let ((ma, va), (mb, vb)) = (mean_var(a), mean_var(b));
    let (qa, qb) = (va / a.len() as f64, vb / b.len() as f64);
    let se = (qa + qb).sqrt();
    let df = (qa + qb).powi(2) / (qa * qa / (a.len() - 1) as f64 + qb * qb / (b.len() - 1) as f64);
    Some(Comparison {
        n_a: a.len(),
        mean_a: ma,
        n_b: b.len(),
        mean_b: mb,
        diff: ma - mb,
        ci95: if se > 0.0 { t975(df) * se } else { 0.0 },
    })
}

/// Hourly residuals of one link end, keyed by hour, against its baseline.
fn hourly(h: &History, cfg: &Config) -> BTreeMap<i64, f64> {
    let mut out = BTreeMap::new();
    let mut day = i64::MIN;
    let mut base = [None; 24];
    for x in &h.hours {
        if day_of(x.hour) != day {
            day = day_of(x.hour);
            base = series::baseline(h, day, &cfg.baseline);
        }
        let hod = ((x.hour - day) / HOUR) as usize;
        if let (Some(v), Some(b)) = (x.signal, base[hod]) {
            out.insert(x.hour, v - b);
        }
    }
    out
}

/// Hourly residuals per link, both ends averaged, for hours in [from, to).
fn links(
    conn: &Connection,
    cfg: &Config,
    from: i64,
    to: i64,
) -> Result<BTreeMap<Pair, BTreeMap<i64, f64>>> {
    let span = from - cfg.baseline.days as i64 * DAY;
    let mut ends = series::ends(conn, span, to)?;
    ends.retain(|e| cfg.studies(&e.0, &e.1));
    let mut wet = series::wet(conn, cfg, &ends, span, to)?;
    let mut acc: BTreeMap<Pair, BTreeMap<i64, (f64, u32)>> = BTreeMap::new();
    // one end at a time: a year of every link's hours is a lot to hold on a Pi
    for end in &ends {
        let h = History {
            hours: series::end_hours(conn, end, span, to)?,
            wet: wet.remove(end).unwrap_or_default(),
        };
        let m = acc.entry(edge(&end.0, &end.1)).or_default();
        for (t, r) in hourly(&h, cfg).range(from..to) {
            let e = m.entry(*t).or_insert((0.0, 0));
            e.0 += r;
            e.1 += 1;
        }
    }
    Ok(acc
        .into_iter()
        .map(|(p, m)| {
            (
                p,
                m.into_iter().map(|(t, (s, n))| (t, s / n as f64)).collect(),
            )
        })
        .filter(|(_, m): &(Pair, BTreeMap<i64, f64>)| !m.is_empty())
        .collect())
}

pub struct RainResult {
    pub pair: Pair,
    pub stations: Vec<String>,
    /// wet hours minus dry hours, in dB; None without enough of both
    pub cmp: Option<Comparison>,
}

pub struct Results {
    pub from: i64,
    pub to: i64,
    pub links: usize,
    pub rain: Vec<RainResult>,
    pub rain_all: Option<Comparison>,
    /// night |residual| minus day |residual|, in dB
    pub night: Option<Comparison>,
    /// mean |residual| by local hour of day, and how many link-hours behind it
    pub by_hour: Vec<(usize, f64, usize)>,
}

/// Sums and counts per hour, for one value per hour over several links.
type Hourly = BTreeMap<i64, (f64, u32)>;

fn add(acc: &mut Hourly, rs: &BTreeMap<i64, f64>, hour: i64) {
    if let Some(r) = rs.get(&hour) {
        let e = acc.entry(hour).or_insert((0.0, 0));
        e.0 += r;
        e.1 += 1;
    }
}

fn means(acc: &Hourly) -> Vec<(i64, f64)> {
    acc.iter().map(|(t, (s, n))| (*t, s / *n as f64)).collect()
}

fn values(acc: &Hourly) -> Vec<f64> {
    means(acc).into_iter().map(|x| x.1).collect()
}

pub fn compute(conn: &Connection, cfg: &Config, from: i64, to: i64) -> Result<Results> {
    let per_link = links(conn, cfg, from, to)?;
    let mut rain = vec![];
    let (mut wet_all, mut dry_all) = (Hourly::new(), Hourly::new());
    for (pair, rs) in &per_link {
        let stations = cfg.stations_near(&[pair.0.clone(), pair.1.clone()]);
        if stations.is_empty() {
            continue;
        }
        let (wet, dry) = rain::wet_and_dry(conn, &stations, from, to, cfg.baseline.dry_after_h)?;
        let pick = |hs: &BTreeSet<i64>| -> Vec<f64> {
            hs.iter().filter_map(|h| rs.get(h).copied()).collect()
        };
        let (w, d) = (pick(&wet), pick(&dry));
        for h in &wet {
            add(&mut wet_all, rs, *h);
        }
        for h in &dry {
            add(&mut dry_all, rs, *h);
        }
        rain.push(RainResult {
            pair: pair.clone(),
            stations,
            cmp: compare(&w, &d),
        });
    }
    let off = cfg.utc_offset_h as i64 * HOUR;
    let mut by_hour = vec![(0.0, 0usize); 24];
    let mut size = Hourly::new();
    for rs in per_link.values() {
        for (t, r) in rs {
            let h = ((t + off).rem_euclid(DAY) / HOUR) as usize;
            by_hour[h].0 += r.abs();
            by_hour[h].1 += 1;
            let e = size.entry(*t).or_insert((0.0, 0));
            e.0 += r.abs();
            e.1 += 1;
        }
    }
    let local = |t: i64| ((t + off).rem_euclid(DAY) / HOUR) as usize;
    let size = means(&size);
    let night: Vec<f64> = size
        .iter()
        .filter(|(t, _)| !(6..20).contains(&local(*t)))
        .map(|x| x.1)
        .collect();
    let day: Vec<f64> = size
        .iter()
        .filter(|(t, _)| (8..18).contains(&local(*t)))
        .map(|x| x.1)
        .collect();
    Ok(Results {
        from,
        to,
        links: per_link.len(),
        rain,
        rain_all: compare(&values(&wet_all), &values(&dry_all)),
        night: compare(&night, &day),
        by_hour: by_hour
            .into_iter()
            .enumerate()
            .map(|(h, (s, n))| (h, if n > 0 { s / n as f64 } else { 0.0 }, n))
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clear_difference_and_a_muddy_one() {
        let a = [-3.1, -2.9, -3.0, -3.2, -2.8];
        let b = [0.1, -0.1, 0.0, 0.2, -0.2];
        let c = compare(&a, &b).unwrap();
        assert!((c.diff + 3.0).abs() < 1e-9);
        assert!(c.clear());
        let noisy = [-4.0, 3.0, -2.0, 4.0, -1.5];
        assert!(!compare(&noisy, &b).unwrap().clear());
        // three against three, a 1 dB shift: t with 4 degrees of freedom, not 1.96
        let c = compare(&[1.0, 2.0, 3.0], &[2.0, 3.0, 4.0]).unwrap();
        assert!(
            (c.ci95 - 2.776 * (2.0f64 / 3.0).sqrt()).abs() < 1e-3,
            "{c:?}"
        );
        assert!(!c.clear());
        assert!((t975(1000.0) - 1.9623).abs() < 1e-3);
        assert!((t975(40.0) - 2.0211).abs() < 1e-3);
        assert!(compare(&a[..2], &b).is_none());
    }

    #[test]
    fn rain_hours_and_night_hours_come_out_of_the_history() {
        use crate::rain::Report;
        use rusqlite::params;
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(series::SCHEMA).unwrap();
        conn.execute_batch(rain::SCHEMA).unwrap();
        let mut cfg: Config = toml::from_str(
            "utc_offset_h = -8\n[aprs]\n[[site]]\nnode = \"tst-a\"\naprs = [\"CW0001\"]",
        )
        .unwrap();
        cfg.validate().unwrap();
        let t0 = 1_795_000_000_000 - 1_795_000_000_000 % DAY;
        let days = 40;
        for k in 0..days * 24 {
            let hour = t0 + k * HOUR;
            let local = ((hour - 8 * HOUR).rem_euclid(DAY) / HOUR) as usize;
            // rain every fifth day from 10 to 14 local costs 3 dB, and 2 dB while the
            // antennas dry after; nights swing 4 dB either way from one night to the next
            let rain_day = (k / 24) % 5 == 4 && k / 24 >= 25;
            let raining = rain_day && (10..14).contains(&local);
            let drying = rain_day && (14..16).contains(&local);
            let wobble = match ((6..20).contains(&local), (k / 24) % 2) {
                (true, _) => 0.0,
                (false, 0) => 4.0,
                (false, _) => -4.0,
            };
            let down = match (raining, drying) {
                (true, _) => 3.0,
                (_, true) => 2.0,
                _ => 0.0,
            };
            let v = -60.0 + wobble - down;
            for (n, nb) in [("tst-a", "tst-b"), ("tst-b", "tst-a")] {
                conn.execute(
                    "INSERT INTO link_hours (node, neighbor, hour, n, signal) VALUES (?1, ?2, ?3, 60, ?4)",
                    params![n, nb, hour, v],
                )
                .unwrap();
            }
            if k / 24 >= 25 && (8..16).contains(&local) {
                rain::store(
                    &conn,
                    &Report {
                        source: "aprs".into(),
                        station: "CW0001".into(),
                        ts: hour + HOUR - 60_000,
                        rain_1h_mm: Some(if raining { 2.0 } else { 0.0 }),
                        rain_24h_mm: None,
                        text: None,
                        ..Default::default()
                    },
                )
                .unwrap();
            }
        }
        let r = compute(&conn, &cfg, t0 + 25 * DAY, t0 + days * DAY).unwrap();
        assert_eq!(r.links, 1);
        let c = r.rain_all.unwrap();
        assert!(c.clear(), "{c:?}");
        assert!((c.diff + 3.0).abs() < 0.6, "{c:?}");
        assert_eq!(c.n_a, 3 * 4, "three rain days of four hours");
        // 15 days of reports from 08 to 16 local, less the rain and the two drying hours
        assert_eq!(c.n_b, 15 * 8 - 3 * 4 - 3 * 2, "{c:?}");
        let n = r.night.unwrap();
        assert!(n.clear() && n.diff > 3.0, "{n:?}");
        assert!(r.by_hour[2].2 > 0 && r.by_hour[2].1 > r.by_hour[12].1);
    }
}
