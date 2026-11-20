//! Upper-air soundings, for the inversion that caps the marine layer and for refractivity
//! layers that can duct 5 GHz signals.
//!
//! Source: the University of Wyoming's sounding archive, text list form:
//! `GET https://weather.uwyo.edu/wsgi/sounding?datetime=2026-11-18 12:00:00&id=72393&type=TEXT:LIST`.
//! The page has a line "Observations for Station 72393 at 12 UTC 18 Nov 2026" and a
//! fixed-width table, seven characters per column:
//! `PRES HGHT TEMP DWPT RELH MIXR DRCT SPED THTA THTE THTV` (hPa, m above sea level, C,
//! C, ...), blank where a value is missing. The older cgi-bin pages have the same table
//! (wind in knots) under "72393 VBG Vandenberg Observations at 12Z 18 Nov 2026"; both are
//! read. Only pressure, height, temperature and dew point are used.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ridgewarden_client::util::{ms_to_rfc3339, now_ms};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::App;
use crate::config::SoundingCfg;
use crate::series::{DAY, HOUR};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS soundings (
    station  TEXT NOT NULL,
    ts       INTEGER NOT NULL,
    levels   TEXT NOT NULL,
    PRIMARY KEY (station, ts)
);";

/// A gradient steeper than this traps a horizontal ray: a duct.
pub const TRAPPING_N_PER_KM: f64 = -157.0;
/// Steeper than this bends rays more than usual (super-refraction).
pub const SUPER_N_PER_KM: f64 = -79.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Level {
    /// hPa
    pub p: f64,
    /// m above sea level
    pub h: f64,
    /// C
    pub t: f64,
    /// dew point, C
    pub td: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sounding {
    pub station: String,
    pub ts: i64,
    /// from the ground up
    pub levels: Vec<Level>,
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// "12 UTC 18 Nov 2026" or "12Z 18 Nov 2026" to unix ms.
fn obs_time(words: &[&str]) -> Option<i64> {
    let (hour, rest) = match words.first()?.strip_suffix('Z') {
        Some(h) => (h, &words[1..]),
        None if words.get(1) == Some(&"UTC") => (words[0], &words[2..]),
        None => return None,
    };
    let hour: i64 = hour.parse().ok().filter(|h| (0..24).contains(h))?;
    let day: u8 = rest.first()?.parse().ok()?;
    let mon = rest.get(1)?;
    let month = MONTHS.iter().position(|m| m == mon)? as u8 + 1;
    let year: i32 = rest.get(2)?.parse().ok()?;
    let date =
        time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()?;
    Some(date.midnight().assume_utc().unix_timestamp() * 1000 + hour * HOUR)
}

fn field(line: &str, i: usize) -> Option<f64> {
    line.get(i * 7..((i + 1) * 7).min(line.len()))?
        .trim()
        .parse()
        .ok()
}

/// Read a sounding text list page.
pub fn parse(page: &str) -> Result<Sounding> {
    let text = strip_tags(page);
    let head = text
        .lines()
        .find(|l| l.contains("Observations") && l.contains(" at "))
        .context("no \"Observations ... at\" line: not a sounding page")?;
    let station = head
        .split_whitespace()
        .find(|w| w.len() == 5 && w.bytes().all(|b| b.is_ascii_digit()))
        .context("no station number in the heading")?
        .to_string();
    let when: Vec<&str> = head
        .split(" at ")
        .nth(1)
        .unwrap_or("")
        .split_whitespace()
        .collect();
    let ts = obs_time(&when).with_context(|| format!("can't read the time in {head:?}"))?;
    let mut levels: Vec<Level> = vec![];
    for line in text.lines() {
        let (Some(p), Some(h), Some(t)) = (field(line, 0), field(line, 1), field(line, 2)) else {
            continue;
        };
        // heights must climb; anything else is a stray line, not the table
        if levels.last().is_some_and(|l| h <= l.h) || !(0.0..1100.0).contains(&p) {
            continue;
        }
        levels.push(Level {
            p,
            h,
            t,
            td: field(line, 3),
        });
    }
    if levels.len() < 3 {
        bail!("no sounding table for {station} at {}", ms_to_rfc3339(ts));
    }
    Ok(Sounding {
        station,
        ts,
        levels,
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Inversion {
    /// heights above the ground
    pub base_m: f64,
    pub top_m: f64,
    /// how much warmer the top is than the base, C
    pub strength_c: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gradient {
    pub lo_m: f64,
    pub hi_m: f64,
    /// refractivity change per km of height
    pub n_per_km: f64,
}

/// Radio refractivity N from pressure, temperature and dew point (ITU-R P.453).
pub fn refractivity(l: &Level) -> Option<f64> {
    let td = l.td?;
    let e = 6.112 * (17.67 * td / (td + 243.5)).exp();
    let t = l.t + 273.15;
    Some(77.6 / t * (l.p + 4810.0 * e / t))
}

impl Sounding {
    fn low(&self, max_m: f64) -> &[Level] {
        let ground = self.levels[0].h;
        let n = self
            .levels
            .iter()
            .take_while(|l| l.h - ground <= max_m)
            .count();
        &self.levels[..n]
    }

    /// The lowest layer within `max_m` of the ground where temperature rises with
    /// height by at least 1 C: the inversion over the marine layer.
    pub fn inversion(&self, max_m: f64) -> Option<Inversion> {
        let ls = self.low(max_m);
        let ground = self.levels[0].h;
        let mut i = 0;
        while i + 1 < ls.len() {
            if ls[i + 1].t <= ls[i].t {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j + 1 < ls.len() && ls[j + 1].t >= ls[j].t {
                j += 1;
            }
            if ls[j].t - ls[i].t >= 1.0 {
                return Some(Inversion {
                    base_m: ls[i].h - ground,
                    top_m: ls[j].h - ground,
                    strength_c: ls[j].t - ls[i].t,
                });
            }
            i = j;
        }
        None
    }

    /// The layer within `max_m` of the ground where refractivity falls fastest with height.
    pub fn steepest(&self, max_m: f64) -> Option<Gradient> {
        let ground = self.levels[0].h;
        self.low(max_m)
            .windows(2)
            .filter_map(|w| {
                let dn = refractivity(&w[1])? - refractivity(&w[0])?;
                Some(Gradient {
                    lo_m: w[0].h - ground,
                    hi_m: w[1].h - ground,
                    n_per_km: dn / (w[1].h - w[0].h) * 1000.0,
                })
            })
            .min_by(|a, b| a.n_per_km.total_cmp(&b.n_per_km))
    }

    /// One plain sentence about the low atmosphere.
    pub fn summary(&self, max_m: f64) -> String {
        let inv = match self.inversion(max_m) {
            Some(i) => format!(
                "inversion from {:.0} to {:.0} m above ground, {:.1} C warmer at the top",
                i.base_m, i.top_m, i.strength_c
            ),
            None => format!("no inversion in the lowest {max_m:.0} m"),
        };
        let grad = match self.steepest(max_m) {
            Some(g) => format!(
                "; steepest refractivity gradient {:.0} N/km at {:.0}-{:.0} m{}",
                g.n_per_km,
                g.lo_m,
                g.hi_m,
                if g.n_per_km < TRAPPING_N_PER_KM {
                    " (a trapping layer: ducting possible)"
                } else if g.n_per_km < SUPER_N_PER_KM {
                    " (super-refractive)"
                } else {
                    ""
                }
            ),
            None => String::new(),
        };
        format!("{inv}{grad}")
    }
}

pub fn store(conn: &Connection, s: &Sounding) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO soundings (station, ts, levels) VALUES (?1, ?2, ?3)",
        params![s.station, s.ts, serde_json::to_string(&s.levels)?],
    )?;
    Ok(())
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<(String, i64, String)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
}

fn decode((station, ts, levels): (String, i64, String)) -> Result<Sounding> {
    Ok(Sounding {
        station,
        ts,
        levels: serde_json::from_str(&levels)?,
    })
}

/// Soundings in [from, to), oldest first.
pub fn between(conn: &Connection, from: i64, to: i64) -> Result<Vec<Sounding>> {
    let mut st = conn.prepare(
        "SELECT station, ts, levels FROM soundings WHERE ts >= ?1 AND ts < ?2 ORDER BY ts",
    )?;
    let rows: Vec<_> = st
        .query_map(params![from, to], row)?
        .collect::<Result<_, _>>()?;
    rows.into_iter().map(decode).collect()
}

/// Each station's sounding closest to `ts`, if within 12 hours.
pub fn nearest(conn: &Connection, ts: i64) -> Result<Vec<Sounding>> {
    let mut out: Vec<Sounding> = vec![];
    for s in between(conn, ts - 12 * HOUR, ts + 12 * HOUR + 1)? {
        match out.iter_mut().find(|o| o.station == s.station) {
            Some(o) if (s.ts - ts).abs() < (o.ts - ts).abs() => *o = s,
            Some(_) => {}
            None => out.push(s),
        }
    }
    Ok(out)
}

pub fn latest(conn: &Connection) -> Result<Vec<Sounding>> {
    let mut st = conn.prepare(
        "SELECT station, max(ts), levels FROM soundings GROUP BY station ORDER BY station",
    )?;
    let rows: Vec<_> = st.query_map([], row)?.collect::<Result<_, _>>()?;
    rows.into_iter().map(decode).collect()
}

async fn fetch(app: &App, cfg: &SoundingCfg, station: &str, ts: i64) -> Result<Sounding> {
    let t = ms_to_rfc3339(ts);
    let when = format!("{} {}", &t[..10], &t[11..19]);
    let r = app
        .http
        .get(&cfg.url)
        .query(&[
            ("datetime", when.as_str()),
            ("id", station),
            ("type", "TEXT:LIST"),
        ])
        .send()
        .await?;
    if !r.status().is_success() {
        bail!("{} answered {}", cfg.url, r.status());
    }
    let s = parse(&String::from_utf8_lossy(&crate::body(r, 2_000_000).await?))?;
    if s.station != station || s.ts != ts {
        bail!(
            "asked for {station} at {t}, got {} at {}",
            s.station,
            ms_to_rfc3339(s.ts)
        );
    }
    Ok(s)
}

/// Soundings are launched at 00 and 12 UTC and posted an hour or two later; not every
/// station flies every time. Each time is tried 2 h, 6 h and 24 h after launch, then
/// given up, so a station that skipped a launch costs three requests.
const TRIES_AFTER: [i64; 3] = [2 * HOUR, 6 * HOUR, 24 * HOUR];

fn due(tried: &HashMap<(String, i64), usize>, station: &str, ts: i64, now: i64) -> bool {
    let n = tried.get(&(station.to_string(), ts)).copied().unwrap_or(0);
    TRIES_AFTER.get(n).is_some_and(|after| now - ts >= *after)
}

async fn round(app: &App, cfg: &SoundingCfg, tried: &Mutex<HashMap<(String, i64), usize>>) {
    let now = now_ms();
    let last = now - now.rem_euclid(12 * HOUR);
    for station in &cfg.stations {
        for ts in [last, last - 12 * HOUR, last - DAY, last - 36 * HOUR] {
            let have: bool = app
                .db()
                .query_row(
                    "SELECT count(*) > 0 FROM soundings WHERE station = ?1 AND ts = ?2",
                    params![station, ts],
                    |r| r.get(0),
                )
                .unwrap_or(false);
            if have || !due(&tried.lock().unwrap(), station, ts, now) {
                continue;
            }
            *tried
                .lock()
                .unwrap()
                .entry((station.clone(), ts))
                .or_default() += 1;
            let key = format!("sounding {station}");
            match fetch(app, cfg, station, ts).await {
                Ok(s) => match store(&app.db(), &s) {
                    Ok(()) => app.ok(&key),
                    Err(e) => app.failed(&key, &format!("storing sounding {station}: {e:#}")),
                },
                Err(e) => app.failed(&key, &format!("sounding {station}: {e:#}")),
            }
        }
    }
    tried
        .lock()
        .unwrap()
        .retain(|(_, ts), _| now - ts < 3 * DAY);
}

/// Fetch new soundings every hour, forever.
pub async fn run(app: Arc<App>) {
    let Some(cfg) = app.cfg.soundings.clone() else {
        return;
    };
    let tried = Mutex::new(HashMap::new());
    loop {
        round(&app, &cfg, &tried).await;
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ridgewarden_client::util::rfc3339_to_ms;

    fn vbg() -> Sounding {
        parse(include_str!("../tests/fixtures/uwyo-72393.html")).unwrap()
    }

    #[test]
    fn reads_the_wyoming_text_list() {
        let s = vbg();
        assert_eq!(s.station, "72393");
        assert_eq!(s.ts, rfc3339_to_ms("2026-11-18T12:00:00Z").unwrap());
        assert_eq!(s.levels.len(), 9);
        assert_eq!(
            s.levels[0],
            Level {
                p: 1007.0,
                h: 121.0,
                t: 8.0,
                td: Some(5.8)
            }
        );
        // the top level has no wind; the columns we use are still there
        assert_eq!(s.levels[8].h, 35869.0);
        // the old cgi-bin heading reads too
        let old = include_str!("../tests/fixtures/uwyo-72393.html").replace(
            "Observations for Station 72393 at 12 UTC 18 Nov 2026",
            "72393 VBG Vandenberg Observations at 12Z 18 Nov 2026",
        );
        assert_eq!(parse(&old).unwrap().ts, s.ts);
        assert!(parse("<html>Can't get 72393 VBG Observations at 00Z 19 Nov 2026</html>").is_err());
        assert!(parse("<html>nothing</html>").is_err());
    }

    #[test]
    fn finds_the_inversion_and_the_trapping_layer() {
        let s = vbg();
        let inv = s.inversion(3000.0).unwrap();
        assert_eq!((inv.base_m, inv.top_m), (0.0, 111.0));
        assert!((inv.strength_c - 4.6).abs() < 1e-9);
        let g = s.steepest(3000.0).unwrap();
        assert_eq!((g.lo_m, g.hi_m), (0.0, 35.0));
        // 8.0 C / 5.8 C at 1007 hPa up to 11.0 C / 4.0 C at 1000 hPa: about -308 N/km
        assert!((g.n_per_km + 308.0).abs() < 3.0, "{}", g.n_per_km);
        assert!(s.summary(3000.0).contains("trapping"));
    }

    #[test]
    fn refractivity_matches_the_standard_atmosphere() {
        // ITU-R P.453 gives about 315 N-units at sea level in a mid-latitude atmosphere
        let n = refractivity(&Level {
            p: 1013.25,
            h: 0.0,
            t: 15.0,
            td: Some(8.0),
        })
        .unwrap();
        assert!((300.0..330.0).contains(&n), "{n}");
    }

    #[test]
    fn nearest_gives_each_station_its_closest_sounding() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        let s = vbg();
        let at = |station: &str, ts: i64| Sounding {
            station: station.into(),
            ts,
            levels: s.levels.clone(),
        };
        for x in [
            at("72493", s.ts),
            at("72393", s.ts - 12 * HOUR),
            at("72393", s.ts),
            at("72393", s.ts + DAY),
        ] {
            store(&c, &x).unwrap();
        }
        let mut n = nearest(&c, s.ts + HOUR).unwrap();
        n.sort_by(|a, b| a.station.cmp(&b.station));
        let got: Vec<(&str, i64)> = n.iter().map(|x| (x.station.as_str(), x.ts)).collect();
        assert_eq!(got, vec![("72393", s.ts), ("72493", s.ts)]);
        assert!(nearest(&c, s.ts + 3 * DAY).unwrap().is_empty());
    }

    #[test]
    fn missing_soundings_are_retried_three_times_then_dropped() {
        let mut tried = HashMap::new();
        let ts = 1_795_000_000_000 - 1_795_000_000_000 % (12 * HOUR);
        assert!(!due(&tried, "72393", ts, ts + HOUR), "not posted yet");
        assert!(due(&tried, "72393", ts, ts + 2 * HOUR));
        tried.insert(("72393".to_string(), ts), 1);
        assert!(!due(&tried, "72393", ts, ts + 3 * HOUR));
        assert!(due(&tried, "72393", ts, ts + 6 * HOUR));
        tried.insert(("72393".to_string(), ts), 3);
        assert!(!due(&tried, "72393", ts, ts + 30 * HOUR));
    }
}
