//! The evidence behind a candidate, gathered once for the events page and for the
//! `weather.candidate` events: rain at the sites' stations and lightning near them for
//! drops, the nearest soundings for ducting, and for every kind the strongest gust the
//! stations near its sites reported: wind moving a dish or mount swings a link's level
//! too, and that isn't weather in the path.

use std::collections::BTreeMap;

use anyhow::Result;
use ridgewarden_lightning::geo::LatLon;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

use crate::config::Config;
use crate::events::Candidate;
use crate::rain::{self, Verdict};
use crate::series::HOUR;
use crate::sounding::{self, Sounding};
use crate::strikes::{self, Near};

pub struct Evidence {
    /// a ducting candidate, checked against soundings rather than rain
    pub duct: bool,
    /// the weather stations checked for rain (drops only)
    pub stations: Vec<String>,
    /// None for ducting, or when no station is set for the sites
    pub rain: Option<Verdict>,
    /// None when none of the sites has a known position
    pub strikes: Option<Near>,
    /// each station's sounding nearest the ducting peak
    pub soundings: Vec<Sounding>,
    /// the report with the strongest gust near the sites, from an hour before to an
    /// hour after
    pub gust: Option<rain::Report>,
}

/// The latest reported position of each of `nodes` that has one.
pub fn positions<'a>(
    conn: &Connection,
    nodes: impl IntoIterator<Item = &'a String>,
) -> Result<BTreeMap<String, LatLon>> {
    let mut st = conn.prepare(
        "SELECT lat, lon FROM polls WHERE node = ?1 AND lat IS NOT NULL AND lon IS NOT NULL
         ORDER BY ts DESC LIMIT 1",
    )?;
    let mut out = BTreeMap::new();
    for n in nodes {
        let p = st
            .query_row([n], |r| Ok(LatLon::new(r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some(p) = p.filter(LatLon::valid) {
            out.insert(n.clone(), p);
        }
    }
    Ok(out)
}

/// The window a drop is checked in: from `rain_before_h` before it to an hour after it
/// ends (or six hours on, while it hasn't).
fn window(cfg: &Config, start: i64, end: Option<i64>) -> (i64, i64) {
    (
        start - cfg.wet.rain_before_h as i64 * HOUR,
        end.unwrap_or(start + 6 * HOUR) + HOUR,
    )
}

pub fn gather(conn: &Connection, cfg: &Config, c: &Candidate) -> Result<Evidence> {
    if let Candidate::Duct(d) = c {
        let mut nodes: Vec<String> = d
            .links
            .iter()
            .flat_map(|(a, b)| [a.clone(), b.clone()])
            .collect();
        nodes.sort();
        nodes.dedup();
        let near = cfg.stations_near(&nodes);
        let to = d.end.unwrap_or(d.last) + HOUR;
        return Ok(Evidence {
            duct: true,
            stations: vec![],
            rain: None,
            strikes: None,
            soundings: sounding::nearest(conn, d.peak_at)?,
            gust: rain::strongest_gust(conn, &near, d.start - HOUR, to)?,
        });
    }
    let sites = c.sites();
    let (from, to) = window(cfg, c.start(), c.end());
    let stations = cfg.stations_near(&sites);
    let rain = if stations.is_empty() {
        None
    } else {
        Some(rain::verdict(&rain::reports(conn, &stations, from, to)?))
    };
    let places: Vec<LatLon> = positions(conn, &sites)?.into_values().collect();
    let strikes = if places.is_empty() {
        None
    } else {
        Some(strikes::near(conn, &places, from, to, cfg.wet.strike_km)?)
    };
    let gust = rain::strongest_gust(
        conn,
        &stations,
        c.start() - HOUR,
        c.end().unwrap_or(c.start() + 6 * HOUR) + HOUR,
    )?;
    Ok(Evidence {
        duct: false,
        stations,
        rain,
        strikes,
        soundings: vec![],
        gust,
    })
}

pub fn round(x: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (x * k).round() / k
}

impl Evidence {
    /// Plain sentences for the page.
    /// Whether a gust near the sites was strong enough that wind may have moved a dish.
    pub fn windy(&self, cfg: &Config) -> bool {
        self.gust
            .as_ref()
            .and_then(|g| g.gust_mph)
            .is_some_and(|g| g >= cfg.gust_mph)
    }

    pub fn lines(&self, cfg: &Config) -> Vec<String> {
        let mut out = vec![];
        if let Some(g) = &self.gust {
            let mph = g.gust_mph.unwrap_or(0.0);
            out.push(if self.windy(cfg) {
                format!(
                    "Gusts to {mph:.0} mph at {} ({}): wind may have moved a dish or mount, so \
                     this may not be weather in the path.",
                    g.station,
                    ridgewarden_client::util::short_time(g.ts)
                )
            } else {
                format!(
                    "Wind no concern: gusts to {mph:.0} mph at most at {}, under {:.0} mph.",
                    g.station, cfg.gust_mph
                )
            });
        }
        if let Some(r) = &self.rain {
            out.push(format!(
                "{} Checked: {}.",
                r.text(),
                self.stations.join(", ")
            ));
        } else if self.duct && self.soundings.is_empty() {
            out.push("No sounding within 12 hours.".into());
        } else if !self.duct {
            out.push(
                "No weather stations are set for these sites, so it can't be checked against rain."
                    .into(),
            );
        }
        if let Some(n) = &self.strikes {
            out.push(match n.nearest_km {
                Some(km) => format!(
                    "{} lightning strike{} within {:.0} km, the nearest {km:.0} km away: \
                     a storm was overhead or close.",
                    n.count,
                    if n.count == 1 { "" } else { "s" },
                    cfg.wet.strike_km
                ),
                None => format!(
                    "No lightning strikes within {:.0} km (only counts where a lightning \
                     network covers the area).",
                    cfg.wet.strike_km
                ),
            });
        }
        for s in &self.soundings {
            out.push(format!(
                "Sounding {} at {}: {}.",
                s.station,
                ridgewarden_client::util::short_time(s.ts),
                s.summary(cfg.sounding_height())
            ));
        }
        out
    }

    /// The evidence as the `evidence` object of a published candidate, minus the
    /// candidate's own numbers.
    pub fn json(&self, cfg: &Config) -> Value {
        let rain = self.rain.as_ref().map(|v| match v {
            Verdict::Rain(r) => json!({
                "verdict": "rain", "station": r.station, "source": r.source,
                "at_ms": r.ts, "mm_1h": r.rain_1h_mm.map(|m| round(m, 2)), "text": r.text,
            }),
            Verdict::Dry(n) => json!({"verdict": "dry", "reports": n}),
            Verdict::NoData => json!({"verdict": "none"}),
        });
        let strikes = self
            .strikes
            .as_ref()
            .map(|n| json!({"count": n.count, "nearest_km": n.nearest_km.map(|k| round(k, 1)), "radius_km": cfg.wet.strike_km}));
        let h = cfg.sounding_height();
        let soundings: Vec<Value> = self
            .soundings
            .iter()
            .map(|s| {
                let inv = s.inversion(h);
                let g = s.steepest(h);
                json!({
                    "station": s.station, "at_ms": s.ts,
                    "inversion_base_m": inv.map(|i| i.base_m),
                    "inversion_top_m": inv.map(|i| i.top_m),
                    "inversion_c": inv.map(|i| i.strength_c),
                    "gradient_n_per_km": g.map(|g| round(g.n_per_km, 0)),
                    "gradient_lo_m": g.map(|g| g.lo_m),
                    "gradient_hi_m": g.map(|g| g.hi_m),
                })
            })
            .collect();
        let wind = self.gust.as_ref().map(|g| {
            json!({"station": g.station, "at_ms": g.ts, "gust_mph": g.gust_mph,
                   "windy": self.windy(cfg)})
        });
        json!({"rain_stations": self.stations, "rain": rain, "strikes": strikes,
               "soundings": soundings, "wind": wind})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{Drop, Wet};

    const T0: i64 = 1_795_000_000_000;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        ridgewarden_client::core::init_db(&c, crate::PROJECTIONS).unwrap();
        c.execute_batch(rain::SCHEMA).unwrap();
        c.execute_batch(sounding::SCHEMA).unwrap();
        c
    }

    fn wet() -> Candidate {
        Candidate::Wet(Wet {
            site: "tst-hub".into(),
            start: T0,
            end: Some(T0 + 2 * HOUR),
            drops: vec![Drop {
                a: "tst-hub".into(),
                b: "tst-n1".into(),
                start: T0,
                end: Some(T0 + 2 * HOUR),
                last_low: T0 + HOUR,
                depth: 3.0,
            }],
            site_links: 2,
            others: 3,
            others_dropping: 0,
        })
    }

    fn gust(c: &Connection, mph: f64, ts: i64) {
        rain::store(
            c,
            &rain::Report {
                source: "aprs".into(),
                station: "CW0001".into(),
                ts,
                rain_1h_mm: Some(0.0),
                gust_mph: Some(mph),
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn strong_gusts_at_the_site_flag_the_candidate() {
        let cfg: Config =
            toml::from_str("[aprs]\n[[site]]\nnode = \"tst-hub\"\naprs = [\"CW0001\"]").unwrap();
        let c = db();
        gust(&c, 12.0, T0 + 30 * 60_000);
        let ev = gather(&c, &cfg, &wet()).unwrap();
        assert!(!ev.windy(&cfg));
        assert_eq!(ev.json(&cfg)["wind"]["windy"], false);
        // a 38 mph gust during the drop; one the day before doesn't count
        gust(&c, 38.0, T0 + HOUR);
        gust(&c, 60.0, T0 - 24 * HOUR);
        let ev = gather(&c, &cfg, &wet()).unwrap();
        assert!(ev.windy(&cfg));
        assert_eq!(ev.json(&cfg)["wind"]["gust_mph"], 38.0);
        assert!(ev.lines(&cfg).iter().any(|l| l.contains("Gusts to 38 mph")));
    }
}
