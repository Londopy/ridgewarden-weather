//! The analysis round: roll up hours, take residuals, run the detectors, and keep the
//! candidates in `events` so they outlive the raw samples.
//!
//! Each round looks at the last three days (further back if rounds were missed, or if
//! samples for an older hour arrived late). The first day of that window is only context
//! for the pre-drop level and the baseline; candidates starting before it are frozen as
//! found by earlier rounds.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::detect::{self, Duct, Front, Wet};
use crate::series::{self, DAY, End, HOUR, MIN};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS events (
    id     INTEGER PRIMARY KEY,
    kind   TEXT NOT NULL,
    start  INTEGER NOT NULL,
    end    INTEGER,
    site   TEXT,
    detail TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_start ON events(start);
CREATE TABLE IF NOT EXISTS scan (
    id  INTEGER PRIMARY KEY CHECK (id = 1),
    at  INTEGER NOT NULL
);";

const SCAN: i64 = 3 * DAY;
/// Long scans go in windows this long, keeping the candidates that start in the middle;
/// the day on each side is context for the level before a drop and for its recovery.
const WINDOW: i64 = 4 * DAY;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Candidate {
    Wet(Wet),
    Duct(Duct),
    Front(Front),
}

impl Candidate {
    pub fn start(&self) -> i64 {
        match self {
            Candidate::Wet(w) => w.start,
            Candidate::Duct(d) => d.start,
            Candidate::Front(f) => f.start,
        }
    }

    pub fn end(&self) -> Option<i64> {
        match self {
            Candidate::Wet(w) => w.end,
            Candidate::Duct(d) => d.end,
            Candidate::Front(f) => f.end,
        }
    }

    /// Whether the candidate involves the link between `a` and `b`.
    pub fn involves(&self, a: &str, b: &str) -> bool {
        match self {
            Candidate::Wet(w) => w.drops.iter().any(|d| d.a == a && d.b == b),
            Candidate::Duct(d) => d.links.iter().any(|(x, y)| x == a && y == b),
            Candidate::Front(f) => f.drops.iter().any(|d| d.a == a && d.b == b),
        }
    }

    /// "wet", "duct" or "front", as stored and published.
    pub fn kind(&self) -> &'static str {
        match self {
            Candidate::Wet(_) => "wet",
            Candidate::Duct(_) => "duct",
            Candidate::Front(_) => "front",
        }
    }

    /// The sites it is about: the wet site, or every site with a dropping link.
    pub fn sites(&self) -> Vec<String> {
        match self {
            Candidate::Wet(w) => vec![w.site.clone()],
            Candidate::Duct(_) => vec![],
            Candidate::Front(f) => f.sites.clone(),
        }
    }

    /// The links involved, each as its two node names in order.
    pub fn links(&self) -> Vec<detect::Pair> {
        match self {
            Candidate::Wet(Wet { drops, .. }) | Candidate::Front(Front { drops, .. }) => {
                drops.iter().map(|d| (d.a.clone(), d.b.clone())).collect()
            }
            Candidate::Duct(d) => d.links.clone(),
        }
    }
}

/// Binned residuals of every studied link end in [from, to).
pub fn residual_ends(
    conn: &Connection,
    cfg: &Config,
    from: i64,
    to: i64,
) -> Result<BTreeMap<End, Vec<(i64, f64)>>> {
    let hours = series::hours(conn, from - cfg.baseline.days as i64 * DAY, to)?;
    let bin = cfg.baseline.bin_min as i64 * MIN;
    let mut out = BTreeMap::new();
    for (end, s) in series::all_samples(conn, from, to)? {
        if !cfg.studies(&end.0, &end.1) {
            continue;
        }
        let h = hours.get(&end).map_or(&[][..], Vec::as_slice);
        let r = series::residuals(&s, h, &cfg.baseline);
        if !r.is_empty() {
            out.insert(end, series::bin(&r, bin));
        }
    }
    Ok(out)
}

/// Every candidate in [from, to).
pub fn detect(conn: &Connection, cfg: &Config, from: i64, to: i64) -> Result<Vec<Candidate>> {
    let links = detect::link_series(&residual_ends(conn, cfg, from, to)?);
    let drops: Vec<detect::Drop> = links
        .iter()
        .flat_map(|(p, pts)| detect::drops(p, pts, &cfg.wet))
        .collect();
    let (wet, fronts) = detect::site_drops(&links, &drops, &cfg.wet);
    let mut out: Vec<Candidate> = wet.into_iter().map(Candidate::Wet).collect();
    out.extend(fronts.into_iter().map(Candidate::Front));
    let bin = cfg.baseline.bin_min as i64 * MIN;
    let ducts = detect::ducting(&links, bin, to, &cfg.duct);
    out.extend(not_drops(ducts, &out).into_iter().map(Candidate::Duct));
    out.sort_by_key(Candidate::start);
    Ok(out)
}

/// Ducting episodes that aren't just the drops already listed: an episode with nothing
/// above its baseline, overlapping a wet antenna or a widespread drop, is that drop seen
/// again, not a duct.
fn not_drops(ducts: Vec<Duct>, drops: &[Candidate]) -> Vec<Duct> {
    ducts
        .into_iter()
        .filter(|d| {
            d.peak_up > 0
                || !drops.iter().any(|c| {
                    c.start() < d.end.unwrap_or(i64::MAX) && c.end().is_none_or(|e| e > d.start)
                })
        })
        .collect()
}

/// Candidates starting at or after `keep_from`, from data since `from`, a window at a time
/// so a long first scan doesn't load months of samples at once.
fn scan(
    conn: &Connection,
    cfg: &Config,
    from: i64,
    keep_from: i64,
    now: i64,
) -> Result<Vec<Candidate>> {
    let mut out = vec![];
    let mut s = from;
    loop {
        let e = (s + WINDOW).min(now);
        let lo = if s == from { keep_from } else { s + DAY };
        let hi = if e == now { i64::MAX } else { e - DAY };
        out.extend(
            detect(conn, cfg, s, e)?
                .into_iter()
                .filter(|c| (lo..hi).contains(&c.start())),
        );
        if e == now {
            return Ok(out);
        }
        s = e - 2 * DAY;
    }
}

/// Give an open candidate an end: where its drops were last seen low, or the last
/// departing bin of a ducting episode. Drops get one once they can't still be going
/// (older than `max_recover_h`, their links silent since); anything does when `force`,
/// as it is frozen.
fn close(c: &mut Candidate, force: bool, cfg: &Config, now: i64) {
    let (start, end, drops) = match c {
        Candidate::Wet(w) => (w.start, &mut w.end, &w.drops),
        Candidate::Front(f) => (f.start, &mut f.end, &f.drops),
        Candidate::Duct(d) => {
            if force && d.end.is_none() {
                d.end = Some(d.last);
            }
            return;
        }
    };
    if end.is_none() && (force || now - start > cfg.wet.max_recover_h as i64 * HOUR) {
        *end = drops.iter().map(|d| d.end.unwrap_or(d.last_low)).max();
    }
}

/// Close the stored candidates that are about to be frozen while still open.
fn close_frozen(conn: &Connection, cfg: &Config, before: i64, now: i64) -> Result<()> {
    let open: Vec<(i64, String)> = conn
        .prepare("SELECT id, detail FROM events WHERE end IS NULL AND start < ?1")?
        .query_map(params![before], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (id, detail) in open {
        let mut c: Candidate =
            serde_json::from_str(&detail).context("reading a stored candidate")?;
        close(&mut c, true, cfg, now);
        conn.execute(
            "UPDATE events SET end = ?1, detail = ?2 WHERE id = ?3",
            params![c.end(), serde_json::to_string(&c)?, id],
        )?;
    }
    Ok(())
}

/// One analysis round. Returns how many candidates are on file for the scanned window.
pub fn round(conn: &Connection, cfg: &Config, now: i64) -> Result<usize> {
    let rolled = series::rollup(conn, now, cfg.keep_days)?;
    crate::rain::prune(conn, now)?;
    crate::strikes::prune(conn, now)?;
    let last: Option<i64> = conn
        .query_row("SELECT at FROM scan WHERE id = 1", [], |r| r.get(0))
        .optional()?;
    let (from, frozen_before) = match last {
        Some(at) => {
            let from = (now - SCAN).min(at - SCAN);
            match rolled {
                // samples for an hour already frozen arrived late (a core still
                // catching up, a stalled sync): look again from there
                Some(h) if h < from + DAY => (h - DAY, h),
                _ => (from, from + DAY),
            }
        }
        None => {
            let first: Option<i64> =
                conn.query_row("SELECT min(ts) FROM link_samples", [], |r| r.get(0))?;
            let from = first.unwrap_or(now);
            (from, from)
        }
    };
    let mut found = scan(conn, cfg, from, frozen_before, now)?;
    for c in &mut found {
        close(c, false, cfg, now);
    }
    close_frozen(conn, cfg, frozen_before, now)?;
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM events WHERE start >= ?1",
        params![frozen_before],
    )?;
    let mut n = 0;
    for c in &found {
        let site = match c {
            Candidate::Wet(w) => Some(w.site.as_str()),
            _ => None,
        };
        tx.execute(
            "INSERT INTO events (kind, start, end, site, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                c.kind(),
                c.start(),
                c.end(),
                site,
                serde_json::to_string(c)?
            ],
        )?;
        n += 1;
    }
    tx.execute(
        "INSERT INTO scan (id, at) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET at = excluded.at",
        params![now],
    )?;
    tx.commit()?;
    Ok(n)
}

/// Candidates on file overlapping [from, to), newest first; `kind` "wet" or "duct" filters.
pub fn list(conn: &Connection, from: i64, to: i64, kind: Option<&str>) -> Result<Vec<Candidate>> {
    let mut st = conn.prepare(
        "SELECT detail FROM events WHERE coalesce(end, ?2) >= ?1 AND start < ?2
         AND (?3 IS NULL OR kind = ?3) ORDER BY start DESC LIMIT 500",
    )?;
    let rows: Vec<String> = st
        .query_map(params![from, to, kind], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    // a row this version can't read (written by another) is left out, not fatal
    Ok(rows
        .iter()
        .filter_map(|d| match serde_json::from_str(d) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("skipping a stored candidate this version can't read: {e}");
                None
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ridgewarden_client::core::Projection;
    use ridgewarden_client::mesh::Mesh;
    use ridgewarden_client::mesh::testing::Fake;
    use serde_json::json;

    const T0: i64 = 1_795_000_000_000 - 1_795_000_000_000 % DAY;

    fn fake() -> Fake {
        let f = Fake::new(vec![
            Box::new(Mesh) as Box<dyn Projection>,
            Box::new(crate::strikes::Strikes),
        ]);
        f.conn
            .execute_batch(&format!(
                "{}{}{}",
                series::SCHEMA,
                SCHEMA,
                crate::rain::SCHEMA
            ))
            .unwrap();
        f
    }

    fn cfg() -> Config {
        let mut c: Config = toml::from_str("").unwrap();
        c.validate().unwrap();
        c
    }

    /// Polls every 5 minutes from three sites around a hub, for `days` days. Each link
    /// has a daily cycle; from `wet_at` for three hours the hub's links lose up to 3 dB.
    fn history(f: &mut Fake, days: i64, wet_at: i64) {
        let spokes = ["tst-n1", "tst-n2", "tst-n3"];
        for k in 0..days * 288 {
            let ts = T0 + k * 5 * MIN;
            let h = (ts % DAY) as f64 / HOUR as f64;
            let cycle = 2.0 * (h / 24.0 * std::f64::consts::TAU).sin();
            let wet = if (wet_at..wet_at + 3 * HOUR).contains(&ts) {
                -3.0 * (1.0 - (ts - wet_at) as f64 / (3 * HOUR) as f64)
            } else {
                0.0
            };
            let jitter = if k % 2 == 0 { 0.5 } else { -0.5 };
            let level = |base: f64, w: f64| (base + cycle + w + jitter).round();
            let links: Vec<_> = spokes
                .iter()
                .map(|s| json!({"neighbor": s, "type": "RF", "signal": level(-60.0, wet), "noise": -95.0}))
                .collect();
            f.add(
                "node.poll",
                json!({"node": "tst-hub", "ok": true, "links": links}),
                ts,
            );
            for s in spokes {
                f.add(
                    "node.poll",
                    json!({"node": s, "ok": true, "links": [
                        {"neighbor": "tst-hub", "type": "RF", "signal": level(-62.0, wet), "noise": -95.0},
                        {"neighbor": if s == "tst-n3" { "tst-n1" } else { "tst-n3" }, "type": "RF",
                         "signal": level(-70.0, 0.0), "noise": -95.0},
                    ]}),
                    ts,
                );
            }
        }
    }

    #[test]
    fn a_round_finds_the_wet_hub_and_freezes_old_candidates() {
        let mut f = fake();
        let wet_at = T0 + 9 * DAY + 14 * HOUR + 3 * MIN;
        history(&mut f, 10, wet_at);
        let now = T0 + 10 * DAY;
        let c = cfg();
        assert_eq!(round(&f.conn, &c, now).unwrap(), 1);
        let found = list(&f.conn, 0, now, None).unwrap();
        let Candidate::Wet(w) = &found[0] else {
            panic!("{found:?}")
        };
        assert_eq!(w.site, "tst-hub");
        assert_eq!(w.drops.len(), 3);
        assert!((w.start - wet_at).abs() <= 10 * MIN, "{}", w.start - wet_at);
        assert!(
            w.drops.iter().all(|d| d.depth >= 2.0 && d.depth < 6.0),
            "{:?}",
            w.drops
        );
        assert!(found[0].involves("tst-hub", "tst-n2"));
        assert!(!found[0].involves("tst-n1", "tst-n3"));
        assert!(list(&f.conn, 0, now, Some("duct")).unwrap().is_empty());
        // later rounds keep it, even once the raw samples behind it are gone
        let later = now + 5 * DAY;
        round(&f.conn, &c, later).unwrap();
        f.conn.execute("DELETE FROM link_samples", []).unwrap();
        round(&f.conn, &c, later + HOUR).unwrap();
        assert_eq!(list(&f.conn, 0, later, Some("wet")).unwrap().len(), 1);
    }

    #[test]
    fn an_open_wet_candidate_closes_once_it_cant_still_be_going() {
        let drop = |end: Option<i64>, last_low: i64| detect::Drop {
            a: "tst-hub".into(),
            b: "tst-n1".into(),
            start: T0,
            end,
            last_low,
            depth: 3.0,
        };
        let mut c = Candidate::Wet(Wet {
            site: "tst-hub".into(),
            start: T0,
            end: None,
            drops: vec![
                drop(None, T0 + HOUR),
                drop(Some(T0 + 2 * HOUR), T0 + 90 * MIN),
            ],
            site_links: 2,
            others: 0,
            others_dropping: 0,
        });
        close(&mut c, false, &cfg(), T0 + 6 * HOUR);
        assert_eq!(c.end(), None, "could still be draining");
        let mut frozen = c.clone();
        close(&mut frozen, true, &cfg(), T0 + 6 * HOUR);
        assert_eq!(
            frozen.end(),
            Some(T0 + 2 * HOUR),
            "freezing closes it anyway"
        );
        close(&mut c, false, &cfg(), T0 + DAY);
        assert_eq!(c.end(), Some(T0 + 2 * HOUR));
    }

    #[test]
    fn history_that_arrives_after_the_first_round_is_still_scanned() {
        let mut f = fake();
        let wet_at = T0 + 9 * DAY + 14 * HOUR + 3 * MIN;
        let c = cfg();
        history(&mut f, 10, wet_at);
        // the core hands over only the last day first
        f.conn
            .execute_batch(
                "CREATE TABLE held AS SELECT * FROM link_samples WHERE 0;
                 INSERT INTO held SELECT * FROM link_samples;",
            )
            .unwrap();
        f.conn
            .execute("DELETE FROM link_samples WHERE ts < ?1", [T0 + 9 * DAY])
            .unwrap();
        let now = T0 + 10 * DAY;
        assert_eq!(
            round(&f.conn, &c, now).unwrap(),
            0,
            "no baseline from one day"
        );
        // the rest arrives
        f.conn
            .execute(
                "INSERT INTO link_samples SELECT * FROM held WHERE ts < ?1",
                [T0 + 9 * DAY],
            )
            .unwrap();
        round(&f.conn, &c, now + 10 * MIN).unwrap();
        assert_eq!(list(&f.conn, 0, now, Some("wet")).unwrap().len(), 1);
    }

    #[test]
    fn an_all_down_duct_over_a_drop_is_the_drop() {
        let duct = |up: usize| Duct {
            start: T0 + HOUR,
            end: Some(T0 + 3 * HOUR),
            last: T0 + 3 * HOUR,
            links: vec![],
            peak_at: T0 + HOUR,
            peak_departing: 3,
            peak_with_data: 6,
            peak_up: up,
            peak_down: 3 - up,
            max_abs: 5.5,
        };
        let wet = Candidate::Wet(Wet {
            site: "tst-hub".into(),
            start: T0,
            end: Some(T0 + 2 * HOUR),
            drops: vec![],
            site_links: 3,
            others: 3,
            others_dropping: 0,
        });
        assert!(not_drops(vec![duct(0)], std::slice::from_ref(&wet)).is_empty());
        assert_eq!(
            not_drops(vec![duct(1)], std::slice::from_ref(&wet)).len(),
            1
        );
        assert_eq!(not_drops(vec![duct(0)], &[]).len(), 1);
    }

    #[test]
    fn nothing_is_found_before_there_is_a_baseline() {
        let mut f = fake();
        history(&mut f, 5, T0 + 4 * DAY + 14 * HOUR);
        assert_eq!(round(&f.conn, &cfg(), T0 + 5 * DAY).unwrap(), 0);
    }
}
