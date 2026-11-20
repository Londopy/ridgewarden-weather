//! Candidates and rain readings published to the mesh through the core, so other apps
//! can use them without reaching this app.
//!
//! `weather.candidate`, a main-log event per candidate. The payload is the candidate
//! exactly as `/api/events` lists it, plus `v`, `id`, `summary`, `withdrawn` and
//! `evidence`:
//!
//! ```text
//! wet:   {"kind": "wet", "site", "start", "end", "drops": [{"a", "b", "start", "end",
//!         "last_low", "depth"}], "site_links", "others", "others_dropping", ...}
//! front: {"kind": "front", "start", "end", "drops": [...], "sites": [...], "with_data", ...}
//! duct:  {"kind": "duct", "start", "end", "last", "links": [["a", "b"]], "peak_at",
//!         "peak_departing", "peak_with_data", "peak_up", "peak_down", "max_abs", ...}
//! all:   "v": 1, "id": "16 hex digits", "summary": "one sentence", "withdrawn": bool,
//!        "evidence": {"rain_stations": [...], "rain": {...} | null,
//!                     "strikes": {...} | null, "soundings": [...]}
//! ```
//!
//! Times are unix milliseconds; `end` is null while a candidate is still going; `depth`
//! and `max_abs` are dB. A candidate is published when first found and again under the
//! same id when it ends. If a later scan no longer finds it (it turned out to be part of
//! a widespread drop, say), it is published once more with `withdrawn: true` and `end`
//! set to that time. Readers keep the latest event per id.
//!
//! `weather.rain`, telemetry events with the rain readings fetched from APRS-IS and NWS:
//! `{"v": 1, "readings": [{"station", "source": "aprs" | "nws", "at_ms", "rain_1h_mm",
//! "rain_24h_mm", "text", "wind_dir_deg", "wind_mph", "gust_mph"}]}`, at most 200 readings per event, each reading sent once
//! (again if a later fetch changes it). `rain_1h_mm` and `rain_24h_mm` are null when the
//! report didn't carry them; `text` is the NWS observer's description.

use std::collections::HashSet;

use anyhow::Result;
use ridgewarden_client::util::short_time;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use crate::App;
use crate::config::Config;
use crate::events::{self, Candidate};
use crate::evidence;
use crate::rain;
use crate::series::{DAY, HOUR};

pub const KIND: &str = "weather.candidate";
pub const RAIN_KIND: &str = "weather.rain";

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS published (
    id        TEXT PRIMARY KEY,
    kind      TEXT NOT NULL,
    key       TEXT NOT NULL,
    start     INTEGER NOT NULL,
    end       INTEGER,
    withdrawn INTEGER NOT NULL,
    payload   TEXT NOT NULL,
    at        INTEGER NOT NULL
);";

/// Only candidates this recent are published; older ones are history, not news.
const RECENT: i64 = 14 * DAY;
/// Events published per round, so a first run doesn't flood the log.
const PER_ROUND: usize = 20;
/// Rain readings per `weather.rain` event.
const READINGS: usize = 200;
/// Older readings (an imported log, publishing just switched on) aren't news.
const RAIN_RECENT: i64 = 2 * DAY;
/// The scan may move a candidate's start by a bin or two between rounds.
const SAME_START: i64 = HOUR;

/// What makes two scans' candidates the same one, besides kind and start: the site for a
/// wet antenna; nothing for the others, whose links and sites can grow as they go on.
fn key(c: &Candidate) -> String {
    match c {
        Candidate::Wet(w) => w.site.clone(),
        _ => String::new(),
    }
}

/// 64-bit FNV-1a of the candidate's kind, key and start: stable, and plenty to tell one
/// station's candidates apart. Not a security measure; the core signs the event.
fn new_id(c: &Candidate) -> String {
    let k = format!("{}|{}|{}", c.kind(), key(c), c.start());
    let h = k.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{h:016x}")
}

/// A candidate as last published.
struct Row {
    id: String,
    kind: String,
    key: String,
    start: i64,
    end: Option<i64>,
    withdrawn: bool,
    payload: String,
}

fn rows(conn: &Connection, since: i64) -> Result<Vec<Row>> {
    let mut st = conn.prepare(
        "SELECT id, kind, key, start, end, withdrawn, payload FROM published WHERE start >= ?1",
    )?;
    let rows = st.query_map([since], |r| {
        Ok(Row {
            id: r.get(0)?,
            kind: r.get(1)?,
            key: r.get(2)?,
            start: r.get(3)?,
            end: r.get(4)?,
            withdrawn: r.get(5)?,
            payload: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn summary(cfg: &Config, c: &Candidate) -> String {
    match c {
        Candidate::Wet(w) => format!(
            "Wet antenna? {} of {}'s {} links dropped at {}.",
            w.drops.len(),
            cfg.label(&w.site),
            w.site_links,
            short_time(w.start)
        ),
        Candidate::Front(f) => format!(
            "Widespread drop? {} of {} links at {} sites dropped at {}.",
            f.drops.len(),
            f.with_data,
            f.sites.len(),
            short_time(f.start)
        ),
        Candidate::Duct(d) => format!(
            "Ducting or fading? {} of {} links {:.0} dB or more from their usual level at {}.",
            d.peak_departing,
            d.peak_with_data,
            cfg.duct.departure_db,
            short_time(d.peak_at)
        ),
    }
}

/// The event payload for a candidate: the candidate itself plus the publishing fields.
pub fn payload(conn: &Connection, cfg: &Config, c: &Candidate, id: &str) -> Result<Value> {
    let mut p = serde_json::to_value(c)?;
    let ev = evidence::gather(conn, cfg, c)?.json(cfg);
    if let Value::Object(m) = &mut p {
        m.insert("v".into(), json!(1));
        m.insert("id".into(), json!(id));
        m.insert("summary".into(), json!(summary(cfg, c)));
        m.insert("withdrawn".into(), json!(false));
        m.insert("evidence".into(), ev);
    }
    Ok(p)
}

/// One `weather.candidate` event to log, and what to remember about it.
pub struct Item {
    pub id: String,
    pub kind: String,
    pub key: String,
    pub start: i64,
    pub end: Option<i64>,
    pub withdrawn: bool,
    pub payload: Value,
}

/// What to publish now: new candidates, published ones that have since ended or come
/// back after a withdrawal, and published ones the latest scan no longer finds.
pub fn due(conn: &Connection, cfg: &Config, now: i64) -> Result<Vec<Item>> {
    let current = events::list(conn, now - RECENT, now + 1, None)?;
    let published = rows(conn, now - RECENT - 7 * DAY)?;
    // each published row stands for one candidate at most
    let mut claimed: HashSet<&str> = HashSet::new();
    let mut out = vec![];
    for c in current.iter().rev() {
        let row = published
            .iter()
            .filter(|r| {
                r.kind == c.kind()
                    && r.key == key(c)
                    && (r.start - c.start()).abs() <= SAME_START
                    && !claimed.contains(r.id.as_str())
            })
            .min_by_key(|r| (r.start - c.start()).abs());
        let id = match row {
            None => new_id(c),
            Some(r) => {
                claimed.insert(&r.id);
                if r.withdrawn || (r.end.is_none() && c.end().is_some()) {
                    r.id.clone()
                } else {
                    continue;
                }
            }
        };
        out.push(Item {
            payload: payload(conn, cfg, c, &id)?,
            id,
            kind: c.kind().into(),
            key: key(c),
            start: c.start(),
            end: c.end(),
            withdrawn: false,
        });
        if out.len() >= PER_ROUND {
            // the rest next round; nothing can be called gone until all were matched
            return Ok(out);
        }
    }
    for r in &published {
        if r.withdrawn || r.start < now - RECENT || claimed.contains(r.id.as_str()) {
            continue;
        }
        let mut payload: Value = serde_json::from_str(&r.payload)?;
        payload["withdrawn"] = json!(true);
        payload["end"] = json!(now);
        out.push(Item {
            id: r.id.clone(),
            kind: r.kind.clone(),
            key: r.key.clone(),
            start: r.start,
            end: Some(now),
            withdrawn: true,
            payload,
        });
        if out.len() >= PER_ROUND {
            break;
        }
    }
    Ok(out)
}

pub fn record(conn: &Connection, i: &Item, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO published (id, kind, key, start, end, withdrawn, payload, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET end = excluded.end, withdrawn = excluded.withdrawn,
             payload = excluded.payload, at = excluded.at",
        params![
            i.id,
            i.kind,
            i.key,
            i.start,
            i.end,
            i.withdrawn,
            i.payload.to_string(),
            now
        ],
    )?;
    Ok(())
}

/// The next `weather.rain` payload and the readings in it, if any are unsent.
pub fn rain_due(conn: &Connection, now: i64) -> Result<Option<(Vec<rain::Report>, Value)>> {
    let r = rain::unsent(conn, now - RAIN_RECENT, READINGS)?;
    if r.is_empty() {
        return Ok(None);
    }
    let readings: Vec<Value> = r
        .iter()
        .map(|x| {
            json!({"station": x.station, "source": x.source, "at_ms": x.ts,
                   "rain_1h_mm": x.rain_1h_mm.map(|m| evidence::round(m, 2)),
                   "rain_24h_mm": x.rain_24h_mm.map(|m| evidence::round(m, 2)),
                   "text": x.text, "wind_dir_deg": x.wind_dir_deg, "wind_mph": x.wind_mph,
                   "gust_mph": x.gust_mph})
        })
        .collect();
    Ok(Some((r, json!({"v": 1, "readings": readings}))))
}

/// Publish what is due through the core. Returns how many events were logged.
pub async fn round(app: &App) -> Result<usize> {
    let cfg = app.cfg.clone();
    let now = ridgewarden_client::util::now_ms();
    let items = tokio::task::spawn_blocking(move || {
        let conn = crate::open(&cfg)?;
        due(&conn, &cfg, now)
    })
    .await??;
    let mut n = 0;
    // one event the core refuses mustn't hold up the rest; report the first refusal
    let mut refused = None;
    for i in items {
        match app.core.append(KIND, i.payload.clone()).await {
            Ok(_) => {
                record(&app.db(), &i, now)?;
                n += 1;
            }
            Err(e) => {
                refused.get_or_insert(e.context(format!("candidate {}", i.id)));
            }
        }
    }
    for _ in 0..5 {
        let Some((readings, p)) = rain_due(&app.db(), now)? else {
            break;
        };
        if let Err(e) = app.core.append_telemetry(RAIN_KIND, p).await {
            refused.get_or_insert(e.context("rain readings"));
            break;
        }
        rain::mark_sent(&app.db(), &readings)?;
        n += 1;
    }
    match refused {
        Some(e) => Err(e),
        None => Ok(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{Drop, Duct, Wet};

    const T0: i64 = 1_795_000_000_000;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        ridgewarden_client::core::init_db(&c, crate::PROJECTIONS).unwrap();
        for s in [
            crate::series::SCHEMA,
            crate::rain::SCHEMA,
            crate::sounding::SCHEMA,
            events::SCHEMA,
            SCHEMA,
        ] {
            c.execute_batch(s).unwrap();
        }
        c
    }

    fn wet(start: i64, end: Option<i64>) -> Candidate {
        Candidate::Wet(Wet {
            site: "tst-hub".into(),
            start,
            end,
            drops: vec![Drop {
                a: "tst-hub".into(),
                b: "tst-n1".into(),
                start,
                end,
                last_low: start,
                depth: 3.04,
            }],
            site_links: 2,
            others: 3,
            others_dropping: 0,
        })
    }

    fn duct(start: i64, end: Option<i64>, links: usize) -> Candidate {
        Candidate::Duct(Duct {
            start,
            end,
            last: end.unwrap_or(start + HOUR),
            links: (0..links)
                .map(|i| ("tst-a".to_string(), format!("tst-n{i}")))
                .collect(),
            peak_at: start,
            peak_departing: links,
            peak_with_data: 6,
            peak_up: 1,
            peak_down: links - 1,
            max_abs: 6.0,
        })
    }

    fn store(c: &Connection, cands: &[Candidate]) {
        c.execute("DELETE FROM events", []).unwrap();
        for cand in cands {
            c.execute(
                "INSERT INTO events (kind, start, end, detail) VALUES (?1, ?2, ?3, ?4)",
                params![
                    cand.kind(),
                    cand.start(),
                    cand.end(),
                    serde_json::to_string(cand).unwrap()
                ],
            )
            .unwrap();
        }
    }

    fn publish(c: &Connection, cfg: &Config, now: i64) -> Vec<Value> {
        let items = due(c, cfg, now).unwrap();
        for i in &items {
            record(c, i, now).unwrap();
        }
        items.into_iter().map(|i| i.payload).collect()
    }

    fn cfg() -> Config {
        toml::from_str("[aprs]\n[[site]]\nnode = \"tst-hub\"\naprs = [\"CW0001\"]").unwrap()
    }

    #[test]
    fn the_payload_is_the_candidate_as_the_api_lists_it() {
        let c = db();
        store(&c, &[wet(T0, None)]);
        let p = &publish(&c, &cfg(), T0 + HOUR)[0];
        assert_eq!(p["kind"], "wet");
        assert_eq!(p["site"], "tst-hub");
        assert_eq!(p["start"], T0);
        assert_eq!(p["end"], Value::Null);
        assert_eq!(p["drops"][0]["a"], "tst-hub");
        assert_eq!(p["drops"][0]["b"], "tst-n1");
        assert_eq!(p["withdrawn"], false);
        assert_eq!(p["evidence"]["rain"]["verdict"], "none");
        assert_eq!(p["evidence"]["rain_stations"], json!(["CW0001"]));
        assert_eq!(p["id"].as_str().unwrap().len(), 16);
        // the same fields as /api/events, plus the publishing ones
        let api = serde_json::to_value(wet(T0, None)).unwrap();
        for (k, v) in api.as_object().unwrap() {
            assert_eq!(&p[k], v, "{k}");
        }
    }

    #[test]
    fn published_once_when_found_and_once_when_it_ends() {
        let c = db();
        let now = T0 + HOUR;
        store(&c, &[wet(T0, None)]);
        let first = publish(&c, &cfg(), now);
        let id = first[0]["id"].clone();
        assert!(publish(&c, &cfg(), now).is_empty(), "nothing new");
        // the next scan moved the start one bin and saw it end: same id, now with an end
        store(&c, &[wet(T0 + 600_000, Some(T0 + 2 * HOUR))]);
        let ended = publish(&c, &cfg(), now + 2 * HOUR);
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0]["id"], id);
        assert_eq!(ended[0]["end"], T0 + 2 * HOUR);
        assert!(publish(&c, &cfg(), now + 3 * HOUR).is_empty());
        // a different event later the same day is a new id
        store(&c, &[wet(T0 + 6 * HOUR, Some(T0 + 7 * HOUR))]);
        let later = publish(&c, &cfg(), now + 8 * HOUR);
        assert_ne!(later[0]["id"], id);
    }

    #[test]
    fn a_growing_duct_keeps_its_id_and_ends_once() {
        let c = db();
        store(&c, &[duct(T0, None, 3)]);
        let first = publish(&c, &cfg(), T0 + HOUR);
        assert_eq!(first[0]["end"], Value::Null);
        // more links join as it goes on; still the same candidate
        store(&c, &[duct(T0, None, 5)]);
        assert!(publish(&c, &cfg(), T0 + 2 * HOUR).is_empty());
        store(&c, &[duct(T0, Some(T0 + 4 * HOUR), 5)]);
        let ended = publish(&c, &cfg(), T0 + 5 * HOUR);
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0]["id"], first[0]["id"]);
        assert_eq!(ended[0]["links"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn a_candidate_the_next_scan_drops_is_withdrawn() {
        let c = db();
        store(&c, &[wet(T0, None)]);
        let first = publish(&c, &cfg(), T0 + HOUR);
        // it turned out to be part of something else
        store(&c, &[]);
        let gone = publish(&c, &cfg(), T0 + 2 * HOUR);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0]["id"], first[0]["id"]);
        assert_eq!(gone[0]["withdrawn"], true);
        assert_eq!(gone[0]["end"], T0 + 2 * HOUR);
        assert!(publish(&c, &cfg(), T0 + 3 * HOUR).is_empty());
    }

    #[test]
    fn two_candidates_close_together_are_two_and_a_return_is_announced() {
        let c = db();
        store(&c, &[duct(T0, Some(T0 + 40 * 60_000), 3)]);
        let first = publish(&c, &cfg(), T0 + HOUR);
        // a second episode 50 minutes after the first
        store(
            &c,
            &[
                duct(T0, Some(T0 + 40 * 60_000), 3),
                duct(T0 + 50 * 60_000, Some(T0 + 5 * HOUR), 4),
            ],
        );
        let second = publish(&c, &cfg(), T0 + 6 * HOUR);
        assert_eq!(second.len(), 1);
        assert_ne!(second[0]["id"], first[0]["id"]);
        // withdrawn, then found again: announced again under its id
        store(&c, &[wet(T0 + DAY, None)]);
        let w = publish(&c, &cfg(), T0 + DAY + HOUR);
        store(&c, &[]);
        publish(&c, &cfg(), T0 + DAY + 2 * HOUR);
        store(&c, &[wet(T0 + DAY, Some(T0 + DAY + 3 * HOUR))]);
        let back: Vec<Value> = publish(&c, &cfg(), T0 + DAY + 4 * HOUR)
            .into_iter()
            .filter(|p| p["kind"] == "wet")
            .collect();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0]["id"], w[0]["id"]);
        assert_eq!(back[0]["withdrawn"], false);
        assert_eq!(back[0]["end"], T0 + DAY + 3 * HOUR);
    }

    #[test]
    fn rain_readings_go_out_in_batches_once() {
        let c = db();
        for k in 0..3 {
            rain::store(
                &c,
                &rain::Report {
                    source: "aprs".into(),
                    station: "CW0001".into(),
                    ts: T0 + k * HOUR,
                    rain_1h_mm: Some(0.254 * k as f64),
                    rain_24h_mm: Some(1.016),
                    text: None,
                    gust_mph: Some(31.0),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let (r, p) = rain_due(&c, T0 + 3 * HOUR).unwrap().unwrap();
        assert_eq!(p["v"], 1);
        assert_eq!(p["readings"].as_array().unwrap().len(), 3);
        assert_eq!(p["readings"][1]["rain_1h_mm"], 0.25);
        assert_eq!(p["readings"][2]["at_ms"], T0 + 2 * HOUR);
        assert_eq!(p["readings"][0]["source"], "aprs");
        assert_eq!(p["readings"][0]["gust_mph"], 31.0);
        rain::mark_sent(&c, &r).unwrap();
        assert!(rain_due(&c, T0 + 3 * HOUR).unwrap().is_none());
        // and nothing from a log days old
        assert!(rain_due(&c, T0 + 5 * DAY).unwrap().is_none());
    }
}
