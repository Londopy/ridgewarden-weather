//! Link levels over time, with the daily cycle taken out.
//!
//! The level studied is the received signal (RSL, dBm) each node reports for each RF
//! neighbor; where only SNR and noise are reported, their sum. Every finished hour is
//! rolled up into `link_hours` as medians. The baseline for a day is, for each hour of
//! day (UTC), the median of that hour's medians over the days before it; the residual is
//! each sample minus the baseline for its hour. Temperature drift of the radios, sun on
//! the dish and the daily refractivity cycle all repeat daily and mostly cancel out;
//! weather that doesn't repeat stays in the residual.

use std::collections::BTreeMap;

use anyhow::Result;
use ridgewarden_client::util::median;
use rusqlite::{Connection, params};

use crate::config::BaselineCfg;

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

/// Hourly medians of every link end in [from, to).
pub fn hours(conn: &Connection, from: i64, to: i64) -> Result<BTreeMap<End, Vec<Hour>>> {
    let mut st = conn.prepare(
        "SELECT node, neighbor, hour, n, signal, snr, noise FROM link_hours
         WHERE hour >= ?1 AND hour < ?2 ORDER BY node, neighbor, hour",
    )?;
    let mut out: BTreeMap<End, Vec<Hour>> = BTreeMap::new();
    for r in st.query_map(params![from, to], |r| {
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

pub fn day_of(ts: i64) -> i64 {
    ts - ts.rem_euclid(DAY)
}

fn hod(ts: i64) -> usize {
    (ts.rem_euclid(DAY) / HOUR) as usize
}

/// The baseline for the day starting at `day`: per UTC hour of day, the median of that
/// hour's medians over the `cfg.days` days before, if at least `cfg.min_days` have one.
pub fn baseline(hours: &[Hour], day: i64, cfg: &BaselineCfg) -> [Option<f64>; 24] {
    let from = day - cfg.days as i64 * DAY;
    let mut by: [Vec<f64>; 24] = Default::default();
    for h in hours {
        if let (true, Some(v)) = (h.hour >= from && h.hour < day, h.signal) {
            by[hod(h.hour)].push(v);
        }
    }
    by.map(|mut v| {
        if v.len() >= cfg.min_days as usize {
            median(&mut v)
        } else {
            None
        }
    })
}

/// Days of hourly data a link end has in the baseline window before `day`.
pub fn baseline_days(hours: &[Hour], day: i64, cfg: &BaselineCfg) -> usize {
    let from = day - cfg.days as i64 * DAY;
    let mut days: Vec<i64> = hours
        .iter()
        .filter(|h| h.hour >= from && h.hour < day && h.signal.is_some())
        .map(|h| day_of(h.hour))
        .collect();
    days.dedup();
    days.len()
}

/// Each sample's time, level and the baseline for its day and hour; samples without a
/// baseline are left out.
pub fn with_baseline(
    samples: &[Sample],
    hours: &[Hour],
    cfg: &BaselineCfg,
) -> Vec<(i64, f64, f64)> {
    let mut day = i64::MIN;
    let mut base = [None; 24];
    let mut out = Vec::with_capacity(samples.len());
    for s in samples {
        if day_of(s.ts) != day {
            day = day_of(s.ts);
            base = baseline(hours, day, cfg);
        }
        if let Some(b) = base[hod(s.ts)] {
            out.push((s.ts, s.signal, b));
        }
    }
    out
}

/// Each sample minus its baseline.
pub fn residuals(samples: &[Sample], hours: &[Hour], cfg: &BaselineCfg) -> Vec<(i64, f64)> {
    with_baseline(samples, hours, cfg)
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
        BaselineCfg {
            days: 21,
            min_days: 7,
            bin_min: 10,
        }
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
        let r = residuals(&samples, &hours, &cfg());
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
        let hours = hours_of(6, cycle);
        let day = T0 + 6 * DAY;
        assert!(baseline(&hours, day, &cfg()).iter().all(Option::is_none));
        assert_eq!(baseline_days(&hours, day, &cfg()), 6);
        let hours = hours_of(7, cycle);
        assert!(
            baseline(&hours, T0 + 7 * DAY, &cfg())
                .iter()
                .all(Option::is_some)
        );
        // only the days before count, never the day itself
        let b = baseline(&hours, T0 + 3 * DAY, &cfg());
        assert!(b.iter().all(Option::is_none));
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
}
