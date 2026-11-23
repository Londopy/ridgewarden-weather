//! Link levels over time, with the daily cycle taken out.
//!
//! The level studied is the received signal (RSL, dBm) each node reports for each RF
//! neighbor; where only SNR and noise are reported, their sum. Every finished hour is
//! rolled up into `link_hours` as medians. The baseline for a day is, for each hour of
//! day (UTC), the median of that hour's medians over the days before it, leaving out the
//! hours it rained at either end and the drying time after (`History::wet`), so it is the
//! link's dry level; the residual is each sample minus the baseline for its hour.
//! Temperature drift of the radios, sun on the dish and the daily refractivity cycle all
//! repeat daily and mostly cancel out; weather that doesn't repeat stays in the residual.
//! The window rolls, so the baseline follows the season.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;
use ridgewarden_client::util::median;
use rusqlite::{Connection, params};

use crate::config::{BaselineCfg, Config};
use crate::rain;

pub const MIN: i64 = 60_000;
pub const HOUR: i64 = 60 * MIN;
pub const DAY: i64 = 24 * HOUR;
const HOURS_KEEP: i64 = 400 * DAY;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS link_hours (
    node      TEXT NOT NULL,
    neighbor  TEXT NOT NULL,
    hour      INTEGER NOT NULL,
    n         INTEGER NOT NULL,
    signal    REAL,
    snr       REAL,
    noise     REAL,
    PRIMARY KEY (node, neighbor, hour)
);";

/// One link end: the signal `.0` hears from `.1`.
pub type End = (String, String);

#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub ts: i64,
    pub signal: f64,
    pub snr: Option<f64>,
    pub noise: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hour {
    pub hour: i64,
    pub n: usize,
    pub signal: Option<f64>,
    pub snr: Option<f64>,
    pub noise: Option<f64>,
}

/// RF samples with a usable level; anything outside -130..0 dBm is a broken report.
const USABLE: &str = "link_type = 'RF' AND coalesce(signal, snr + noise) BETWEEN -130 AND 0";

fn sample_sql(more: &str) -> String {
    format!(
        "SELECT node, neighbor, ts, coalesce(signal, snr + noise), snr, noise FROM link_samples
         WHERE {USABLE} AND ts >= ?1 AND ts < ?2 {more}"
    )
}

fn collect(
    st: &mut rusqlite::Statement,
    p: impl rusqlite::Params,
) -> Result<BTreeMap<End, Vec<Sample>>> {
    let mut out: BTreeMap<End, Vec<Sample>> = BTreeMap::new();
    let rows = st.query_map(p, |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            Sample {
                ts: r.get(2)?,
                signal: r.get(3)?,
                snr: r.get(4)?,
                noise: r.get(5)?,
            },
        ))
    })?;
    for row in rows {
        let (end, s) = row?;
        out.entry(end).or_default().push(s);
    }
    for v in out.values_mut() {
        v.sort_by_key(|s| s.ts);
    }
    Ok(out)
}

/// Every RF link end's samples in [from, to), in time order.
pub fn all_samples(conn: &Connection, from: i64, to: i64) -> Result<BTreeMap<End, Vec<Sample>>> {
    collect(&mut conn.prepare(&sample_sql(""))?, params![from, to])
}

/// One link end's samples in [from, to).
pub fn samples(conn: &Connection, end: &End, from: i64, to: i64) -> Result<Vec<Sample>> {
    let sql = sample_sql("AND node = ?3 AND neighbor = ?4");
    Ok(
        collect(&mut conn.prepare(&sql)?, params![from, to, end.0, end.1])?
            .remove(end)
            .unwrap_or_default(),
    )
}

fn summarize(hour: i64, s: &[Sample]) -> Hour {
    let col = |f: fn(&Sample) -> Option<f64>| -> Option<f64> {
        median(&mut s.iter().filter_map(f).collect::<Vec<_>>())
    };
    Hour {
        hour,
        n: s.len(),
        signal: col(|x| Some(x.signal)),
        snr: col(|x| x.snr),
        noise: col(|x| x.noise),
    }
}

/// Roll finished hours up into `link_hours`, and drop raw samples past `keep_days`.
/// Returns the oldest hour rolled, so late samples can be scanned again.
/// An hour is rolled again when the raw table holds more samples for it than were
/// rolled, so a site whose polls arrive late (cut off for a while) still makes it in.
pub fn rollup(conn: &Connection, now: i64, keep_days: u32) -> Result<Option<i64>> {
    let keep = keep_days as i64 * DAY;
    let edge = now - now.rem_euclid(HOUR);
    let stale: Vec<i64> = conn
        .prepare(&format!(
            "SELECT r.hour FROM
               (SELECT ts - (ts % 3600000) AS hour, count(*) AS n FROM link_samples
                WHERE {USABLE} AND ts >= ?1 AND ts < ?2 GROUP BY 1) r
             LEFT JOIN (SELECT hour, sum(n) AS n FROM link_hours GROUP BY hour) h
               ON h.hour = r.hour
             WHERE h.n IS NULL OR r.n > h.n"
        ))?
        .query_map(params![now - keep + HOUR, edge], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let tx = conn.unchecked_transaction()?;
    for &hour in &stale {
        tx.execute("DELETE FROM link_hours WHERE hour = ?1", params![hour])?;
        for ((node, nb), s) in all_samples(conn, hour, hour + HOUR)? {
            let h = summarize(hour, &s);
            tx.execute(
                "INSERT INTO link_hours (node, neighbor, hour, n, signal, snr, noise)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![node, nb, hour, h.n as i64, h.signal, h.snr, h.noise],
            )?;
        }
    }
    tx.commit()?;
    conn.execute(
        "DELETE FROM link_samples WHERE ts < ?1",
        params![now - keep],
    )?;
    conn.execute("DELETE FROM polls WHERE ts < ?1", params![now - keep])?;
    conn.execute(
        "DELETE FROM link_hours WHERE hour < ?1",
        params![now - HOURS_KEEP],
    )?;
    Ok(stale.into_iter().min())
}

fn hour_sql(more: &str) -> String {
    format!(
        "SELECT node, neighbor, hour, n, signal, snr, noise FROM link_hours
         WHERE hour >= ?1 AND hour < ?2 {more} ORDER BY node, neighbor, hour"
    )
}

fn collect_hours(
    st: &mut rusqlite::Statement,
    p: impl rusqlite::Params,
) -> Result<BTreeMap<End, Vec<Hour>>> {
    let mut out: BTreeMap<End, Vec<Hour>> = BTreeMap::new();
    for r in st.query_map(p, |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            Hour {
                hour: r.get(2)?,
                n: r.get::<_, i64>(3)? as usize,
                signal: r.get(4)?,
                snr: r.get(5)?,
                noise: r.get(6)?,
            },
        ))
    })? {
        let (k, h) = r?;
        out.entry(k).or_default().push(h);
    }
    Ok(out)
}

/// Hourly medians of every link end in [from, to).
pub fn hours(conn: &Connection, from: i64, to: i64) -> Result<BTreeMap<End, Vec<Hour>>> {
    collect_hours(&mut conn.prepare(&hour_sql(""))?, params![from, to])
}

/// One link end's hourly medians in [from, to).
pub fn end_hours(conn: &Connection, end: &End, from: i64, to: i64) -> Result<Vec<Hour>> {
    let sql = hour_sql("AND node = ?3 AND neighbor = ?4");
    Ok(
        collect_hours(&mut conn.prepare(&sql)?, params![from, to, end.0, end.1])?
            .remove(end)
            .unwrap_or_default(),
    )
}

/// The link ends with hourly medians in [from, to).
pub fn ends(conn: &Connection, from: i64, to: i64) -> Result<Vec<End>> {
    let mut st = conn.prepare(
        "SELECT DISTINCT node, neighbor FROM link_hours WHERE hour >= ?1 AND hour < ?2
         ORDER BY node, neighbor",
    )?;
    let rows = st.query_map(params![from, to], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// When each RF link end was first and last heard: from the hourly medians, and from the
/// raw samples since `raw_from` for the hours not rolled up yet. The first time is the
/// first sample's while that hour's raw samples are kept, and the hour's start after.
pub fn spans(conn: &Connection, raw_from: i64) -> Result<BTreeMap<End, (i64, i64)>> {
    let hourly = "SELECT node, neighbor, min(hour), max(hour) FROM link_hours WHERE hour >= ?1
                  GROUP BY node, neighbor";
    let raw = format!(
        "SELECT node, neighbor, min(ts), max(ts) FROM link_samples WHERE {USABLE} AND ts >= ?1
         GROUP BY node, neighbor"
    );
    let mut out: BTreeMap<End, (i64, i64)> = BTreeMap::new();
    for (sql, from) in [(hourly, i64::MIN), (raw.as_str(), raw_from)] {
        let mut st = conn.prepare(sql)?;
        let mut rows = st.query(params![from])?;
        while let Some(r) = rows.next()? {
            let s = out
                .entry((r.get(0)?, r.get(1)?))
                .or_insert((i64::MAX, i64::MIN));
            *s = (s.0.min(r.get(2)?), s.1.max(r.get(3)?));
        }
    }
    let mut first = conn.prepare(&format!(
        "SELECT min(ts) FROM link_samples
         WHERE node = ?1 AND neighbor = ?2 AND ts >= ?3 AND ts < ?4 AND {USABLE}"
    ))?;
    for ((node, nb), s) in out.iter_mut() {
        let t: Option<i64> = first.query_row(params![node, nb, s.0, s.0 + HOUR], |r| r.get(0))?;
        s.0 = t.unwrap_or(s.0);
    }
    Ok(out)
}

/// One link end's hourly medians, in time order, and its wet hours: rain reported at
/// either end of the link and the drying time after, which its baseline leaves out.
#[derive(Clone, Debug, Default)]
pub struct History {
    pub hours: Vec<Hour>,
    pub wet: BTreeSet<i64>,
}

/// Each of `ends`' wet hours in [from, to): rain reported at either end of the link, and
/// the `dry_after_h` hours after while the antennas dry. An end with no rain source at
/// either end has none.
pub fn wet<'a>(
    conn: &Connection,
    cfg: &Config,
    ends: impl IntoIterator<Item = &'a End>,
    from: i64,
    to: i64,
) -> Result<BTreeMap<End, BTreeSet<i64>>> {
    // a station serves every link at its site, so its reports are read once
    let mut by_station: HashMap<String, BTreeSet<i64>> = HashMap::new();
    let mut out = BTreeMap::new();
    for end in ends {
        let mut wet: BTreeSet<i64> = BTreeSet::new();
        for s in cfg.stations_near(&[end.0.clone(), end.1.clone()]) {
            let w = match by_station.entry(s) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => {
                    let after = cfg.baseline.dry_after_h;
                    let w = rain::wet_hours(conn, std::slice::from_ref(e.key()), from, to, after)?;
                    e.insert(w)
                }
            };
            wet.extend(w.iter());
        }
        out.insert(end.clone(), wet);
    }
    Ok(out)
}

/// Every link end's history in [from, to).
pub fn history(
    conn: &Connection,
    cfg: &Config,
    from: i64,
    to: i64,
) -> Result<BTreeMap<End, History>> {
    let hours = hours(conn, from, to)?;
    let mut wet = wet(conn, cfg, hours.keys(), from, to)?;
    Ok(hours
        .into_iter()
        .map(|(end, hours)| {
            let wet = wet.remove(&end).unwrap_or_default();
            (end, History { hours, wet })
        })
        .collect())
}

/// The history of each of `ends` in [from, to), in the same order.
pub fn history_of(
    conn: &Connection,
    cfg: &Config,
    ends: &[End],
    from: i64,
    to: i64,
) -> Result<Vec<History>> {
    let mut wet = wet(conn, cfg, ends, from, to)?;
    ends.iter()
        .map(|e| {
            Ok(History {
                hours: end_hours(conn, e, from, to)?,
                wet: wet.remove(e).unwrap_or_default(),
            })
        })
        .collect()
}

pub fn day_of(ts: i64) -> i64 {
    ts - ts.rem_euclid(DAY)
}

fn hod(ts: i64) -> usize {
    (ts.rem_euclid(DAY) / HOUR) as usize
}

/// The hours in the `cfg.days` days before `day`.
fn window<'a>(hours: &'a [Hour], day: i64, cfg: &BaselineCfg) -> &'a [Hour] {
    let lo = hours.partition_point(|h| h.hour < day - cfg.days as i64 * DAY);
    let hi = hours.partition_point(|h| h.hour < day);
    &hours[lo..hi.max(lo)]
}

/// The baseline for the day starting at `day`: per UTC hour of day, the median of that
/// hour's medians over the `cfg.days` days before, leaving out the wet hours while at
/// least `cfg.min_days` dry ones are left, and taking every hour when they aren't (an
/// hour of day it rains at most days); none with fewer than `cfg.min_days` in all.
pub fn baseline(h: &History, day: i64, cfg: &BaselineCfg) -> [Option<f64>; 24] {
    let mut dry: [Vec<f64>; 24] = Default::default();
    let mut all: [Vec<f64>; 24] = Default::default();
    for x in window(&h.hours, day, cfg) {
        if let Some(v) = x.signal {
            all[hod(x.hour)].push(v);
            if !h.wet.contains(&x.hour) {
                dry[hod(x.hour)].push(v);
            }
        }
    }
    let min = cfg.min_days as usize;
    std::array::from_fn(|i| {
        if dry[i].len() >= min {
            median(&mut dry[i])
        } else if all[i].len() >= min {
            median(&mut all[i])
        } else {
            None
        }
    })
}

/// Days of hourly data a link end has in the baseline window before `day`, at the hour of
/// day with the fewest: the baseline is whole once this reaches `cfg.min_days`.
pub fn baseline_days(h: &History, day: i64, cfg: &BaselineCfg) -> usize {
    let mut n = [0usize; 24];
    for x in window(&h.hours, day, cfg) {
        if x.signal.is_some() {
            n[hod(x.hour)] += 1;
        }
    }
    n.into_iter().min().unwrap_or(0)
}

/// Each sample's time, level and the baseline for its day and hour; samples without a
/// baseline are left out.
pub fn with_baseline(samples: &[Sample], h: &History, cfg: &BaselineCfg) -> Vec<(i64, f64, f64)> {
    let mut day = i64::MIN;
    let mut base = [None; 24];
    let mut out = Vec::with_capacity(samples.len());
    for s in samples {
        if day_of(s.ts) != day {
            day = day_of(s.ts);
            base = baseline(h, day, cfg);
        }
        if let Some(b) = base[hod(s.ts)] {
            out.push((s.ts, s.signal, b));
        }
    }
    out
}

/// Each sample minus its baseline.
pub fn residuals(samples: &[Sample], h: &History, cfg: &BaselineCfg) -> Vec<(i64, f64)> {
    with_baseline(samples, h, cfg)
        .into_iter()
        .map(|(t, v, b)| (t, v - b))
        .collect()
}

/// Medians over bins `width` long, keyed by the bin's start, in time order.
pub fn bin(points: &[(i64, f64)], width: i64) -> Vec<(i64, f64)> {
    let mut out = vec![];
    let mut cur: Option<i64> = None;
    let mut vals: Vec<f64> = vec![];
    for &(t, v) in points {
        let b = t - t.rem_euclid(width);
        if cur.is_some_and(|c| c != b) {
            if let (Some(c), Some(m)) = (cur, median(&mut vals)) {
                out.push((c, m));
            }
            vals.clear();
        }
        cur = Some(b);
        vals.push(v);
    }
    if let (Some(c), Some(m)) = (cur, median(&mut vals)) {
        out.push((c, m));
    }
    out
}

/// The typical time between samples, from the most recent ones.
pub fn interval(samples: &[Sample]) -> Option<i64> {
    let tail = &samples[samples.len().saturating_sub(60)..];
    let mut gaps: Vec<f64> = tail
        .windows(2)
        .map(|w| (w[1].ts - w[0].ts) as f64)
        .filter(|g| *g > 0.0)
        .collect();
    median(&mut gaps).map(|g| g as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ridgewarden_client::core::Projection;
    use ridgewarden_client::mesh::Mesh;
    use ridgewarden_client::mesh::testing::Fake;
    use serde_json::json;

    const T0: i64 = 1_795_000_000_000 - 1_795_000_000_000 % DAY;

    fn cfg() -> BaselineCfg {
        BaselineCfg::default()
    }

    /// A daily cycle of +-3 dB around -60 dBm, peaking at 12 UTC.
    fn cycle(ts: i64) -> f64 {
        let h = (ts.rem_euclid(DAY)) as f64 / HOUR as f64;
        -60.0 + 3.0 * ((h - 6.0) / 24.0 * std::f64::consts::TAU).sin()
    }

    fn hours_of(days: i64, f: impl Fn(i64) -> f64) -> Vec<Hour> {
        (0..days * 24)
            .map(|i| {
                let t = T0 + i * HOUR;
                Hour {
                    hour: t,
                    n: 60,
                    signal: Some(f(t + 30 * MIN)),
                    snr: None,
                    noise: None,
                }
            })
            .collect()
    }

    fn hist(hours: Vec<Hour>) -> History {
        History {
            hours,
            wet: BTreeSet::new(),
        }
    }

    #[test]
    fn the_daily_cycle_comes_out_and_weather_stays_in() {
        let hours = hours_of(21, cycle);
        let day = T0 + 21 * DAY;
        // a 3 dB dip from 14:00 to 16:00 on top of the usual cycle
        let samples: Vec<Sample> = (0..24 * 60)
            .map(|m| {
                let ts = day + m * MIN + 30 * 1000;
                let dip = if (14 * 60..16 * 60).contains(&m) {
                    3.0
                } else {
                    0.0
                };
                Sample {
                    ts,
                    signal: cycle(ts) - dip,
                    snr: None,
                    noise: None,
                }
            })
            .collect();
        let r = residuals(&samples, &hist(hours), &cfg());
        assert_eq!(r.len(), samples.len());
        let at = |m: i64| r[m as usize].1;
        // the hourly median is taken at mid-hour, so the rest of the hour is off by a bit
        assert!(at(3 * 60 + 30).abs() < 0.1, "{}", at(3 * 60 + 30));
        assert!(at(9 * 60).abs() < 0.5, "{}", at(9 * 60));
        assert!((at(15 * 60 + 30) + 3.0).abs() < 0.1, "{}", at(15 * 60 + 30));
        // the raw level swings 4 dB between 09 and 21 UTC; the residual doesn't
        let raw_gap = (samples[9 * 60].signal - samples[21 * 60].signal).abs();
        assert!(raw_gap > 4.0, "{raw_gap}");
        assert!(at(21 * 60).abs() < 0.5, "{}", at(21 * 60));
    }

    #[test]
    fn no_baseline_until_enough_days() {
        let h = hist(hours_of(6, cycle));
        let day = T0 + 6 * DAY;
        assert!(baseline(&h, day, &cfg()).iter().all(Option::is_none));
        assert_eq!(baseline_days(&h, day, &cfg()), 6);
        let h = hist(hours_of(7, cycle));
        assert!(
            baseline(&h, T0 + 7 * DAY, &cfg())
                .iter()
                .all(Option::is_some)
        );
        // only the days before count, never the day itself
        let b = baseline(&h, T0 + 3 * DAY, &cfg());
        assert!(b.iter().all(Option::is_none));
    }

    #[test]
    fn an_hour_it_rains_most_days_falls_back_to_every_day() {
        let day = T0 + 16 * DAY;
        let at_14 = |t: i64, days: i64| hod(t) == 14 && t < T0 + days * DAY;
        // 3 dB down at 14:00 on the days it rained then
        for (rainy, want) in [(9, -60.0), (10, -63.0)] {
            let hours = hours_of(16, |t| if at_14(t, rainy) { -63.0 } else { -60.0 });
            let wet = hours
                .iter()
                .map(|h| h.hour)
                .filter(|t| at_14(*t, rainy))
                .collect();
            let b = baseline(&History { hours, wet }, day, &cfg());
            // seven dry days make a dry baseline; with six, every day counts
            assert_eq!(b[14], Some(want), "{rainy} rainy days");
            assert_eq!(b[13], Some(-60.0));
        }
    }

    #[test]
    fn the_baseline_is_ready_once_every_hour_of_day_is() {
        let mut hours = hours_of(10, cycle);
        // the node was off at 03:00 on four of the days
        hours.retain(|h| !(hod(h.hour) == 3 && h.hour < T0 + 4 * DAY));
        let h = hist(hours);
        let day = T0 + 10 * DAY;
        assert_eq!(baseline_days(&h, day, &cfg()), 6);
        let b = baseline(&h, day, &cfg());
        assert!(b[3].is_none() && b[4].is_some());
    }

    #[test]
    fn bins_take_medians() {
        let pts = vec![(0, 1.0), (MIN, 9.0), (2 * MIN, 2.0), (10 * MIN, 5.0)];
        assert_eq!(bin(&pts, 10 * MIN), vec![(0, 2.0), (10 * MIN, 5.0)]);
    }

    #[test]
    fn rollup_reads_rf_links_from_polls_and_rolls_late_samples_again() {
        let mut f = Fake::new(vec![Box::new(Mesh) as Box<dyn Projection>]);
        f.conn.execute_batch(SCHEMA).unwrap();
        let poll = |signal: f64| {
            json!({"node": "TST-A", "ok": true, "links": [
                {"neighbor": "TST-B", "type": "RF", "signal": signal, "noise": -95.0, "snr": signal + 95.0},
                {"neighbor": "TST-C", "type": "DTD"},
                {"neighbor": "TST-D", "type": "RF", "snr": 20.0, "noise": -90.0},
                {"neighbor": "TST-E", "type": "RF", "signal": 40.0},
            ]})
        };
        let h = T0 + 5 * HOUR;
        f.add("node.poll", poll(-60.0), h + MIN);
        f.add("node.poll", poll(-64.0), h + 2 * MIN);
        f.add("node.poll", poll(-62.0), h + 3 * MIN);
        assert_eq!(rollup(&f.conn, h + 2 * HOUR, 30).unwrap(), Some(h));
        let hs = hours(&f.conn, h, h + HOUR).unwrap();
        let ab = &hs[&("tst-a".to_string(), "tst-b".to_string())][0];
        assert_eq!((ab.n, ab.signal, ab.noise), (3, Some(-62.0), Some(-95.0)));
        // SNR plus noise stands in for a missing signal; a positive dBm is junk
        assert_eq!(
            hs[&("tst-a".to_string(), "tst-d".to_string())][0].signal,
            Some(-70.0)
        );
        assert!(!hs.contains_key(&("tst-a".to_string(), "tst-e".to_string())));
        assert!(!hs.contains_key(&("tst-a".to_string(), "tst-c".to_string())));
        // nothing new: nothing rolled
        assert_eq!(rollup(&f.conn, h + 2 * HOUR, 30).unwrap(), None);
        // a poll from that hour that arrived late
        f.add("node.poll", poll(-50.0), h + 4 * MIN);
        assert_eq!(rollup(&f.conn, h + 3 * HOUR, 30).unwrap(), Some(h));
        let hs = hours(&f.conn, h, h + HOUR).unwrap();
        assert_eq!(hs[&("tst-a".to_string(), "tst-b".to_string())][0].n, 4);
        // raw samples past keep_days go, the hourly medians stay
        rollup(&f.conn, h + 40 * DAY, 30).unwrap();
        let raw: i64 = f
            .conn
            .query_row("SELECT count(*) FROM link_samples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, 0);
        assert_eq!(hours(&f.conn, h, h + HOUR).unwrap().len(), 2);
    }

    #[test]
    fn rain_at_either_end_and_the_drying_after_stay_out_of_the_baseline() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(rain::SCHEMA).unwrap();
        let mut cfg: Config =
            toml::from_str("[aprs]\n[radar]\n[[site]]\nnode = \"tst-hub\"\naprs = [\"CW0001\"]")
                .unwrap();
        cfg.validate().unwrap();
        let end = |a: &str, b: &str| (a.to_string(), b.to_string());
        let ends = [
            end("tst-hub", "tst-n1"),
            end("tst-n1", "tst-hub"),
            end("tst-n2", "tst-n3"),
        ];
        let rained = |source: &str, station: &str, ts: i64, mm: f64| {
            let r = rain::Report {
                source: source.into(),
                station: station.into(),
                ts,
                rain_1h_mm: Some(mm),
                ..Default::default()
            };
            rain::store(&c, &r).unwrap();
        };
        // the hub's gauge: rain in the hour to 03:00; radar: 0.1 in on tst-n3 to 10:00
        rained("aprs", "CW0001", T0 + 3 * HOUR, 1.0);
        rained(crate::radar::SOURCE, "tst-n3", T0 + 10 * HOUR, 2.54);
        let w = wet(&c, &cfg, &ends, T0, T0 + DAY).unwrap();
        let hours = |a: &str, b: &str| -> Vec<i64> {
            w[&end(a, b)].iter().map(|h| (h - T0) / HOUR).collect()
        };
        // the rain hour and the three after, in both directions of the hub's link
        assert_eq!(hours("tst-hub", "tst-n1"), [2, 3, 4, 5]);
        assert_eq!(hours("tst-n1", "tst-hub"), [2, 3, 4, 5]);
        assert_eq!(hours("tst-n2", "tst-n3"), [9, 10, 11, 12]);
    }
}
