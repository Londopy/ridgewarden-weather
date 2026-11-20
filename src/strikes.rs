//! Lightning strikes from ridgewarden-lightning's `lightning.strikes` events, as
//! supporting evidence for wet-antenna and widespread-drop candidates: a storm overhead
//! is rain nearby even where no gauge reports it.
//!
//! The payload is `{"strikes": [{"id", "t_us", "lat", "lon", "major_km", "minor_km",
//! "az_deg", "rms_us", "ambiguous", "sites": [[site, t_us, resid_us], ...]}, ...]}`,
//! read with ridgewarden-lightning's own parser, which checks every field.

use anyhow::Result;
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_lightning::geo::{LatLon, distance_km};
use ridgewarden_lightning::store::{MAX_STRIKES, parse_strike};
use rusqlite::{Connection, Transaction, params};
use serde_json::Value;

pub struct Strikes;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS strikes (
    id         TEXT PRIMARY KEY,
    ts         INTEGER NOT NULL,
    lat        REAL NOT NULL,
    lon        REAL NOT NULL,
    major_km   REAL NOT NULL,
    ambiguous  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS strikes_ts ON strikes(ts);";

impl Projection for Strikes {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute("DELETE FROM strikes", [])?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.kind != "lightning.strikes" {
            return Ok(());
        }
        for k in e
            .payload
            .get("strikes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(MAX_STRIKES)
        {
            if let Some(st) = parse_strike(k, ts) {
                tx.execute(
                    "INSERT OR IGNORE INTO strikes (id, ts, lat, lon, major_km, ambiguous)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        st.id,
                        st.t_us / 1000,
                        st.pos.lat,
                        st.pos.lon,
                        st.major_km,
                        st.ambiguous
                    ],
                )?;
            }
        }
        Ok(())
    }
}

/// Drop strikes older than the rest of the weather data is kept.
pub fn prune(conn: &Connection, now: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM strikes WHERE ts < ?1",
        params![now - 400 * 86_400_000],
    )?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Near {
    pub count: usize,
    pub nearest_km: Option<f64>,
}

/// Strikes in [from, to) within `km` of any of `places`. Ambiguous strikes (two possible
/// positions) are left out.
pub fn near(conn: &Connection, places: &[LatLon], from: i64, to: i64, km: f64) -> Result<Near> {
    let mut st =
        conn.prepare("SELECT lat, lon FROM strikes WHERE ts >= ?1 AND ts < ?2 AND ambiguous = 0")?;
    let mut out = Near {
        count: 0,
        nearest_km: None,
    };
    for r in st.query_map(params![from, to], |r| Ok((r.get(0)?, r.get(1)?)))? {
        let (lat, lon) = r?;
        let p = LatLon::new(lat, lon);
        let d = places
            .iter()
            .map(|s| distance_km(*s, p))
            .min_by(f64::total_cmp);
        if let Some(d) = d.filter(|d| *d <= km) {
            out.count += 1;
            out.nearest_km = Some(out.nearest_km.map_or(d, |n: f64| n.min(d)));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ridgewarden_client::mesh::testing::Fake;
    use serde_json::json;

    const T: i64 = 1_795_000_000_000;

    fn strike(id: &str, t_ms: i64, lat: f64, ambiguous: bool) -> Value {
        json!({"id": id, "t_us": t_ms * 1000, "lat": lat, "lon": -120.6, "major_km": 2.0,
               "minor_km": 1.0, "az_deg": 10.0, "rms_us": null, "ambiguous": ambiguous,
               "sites": [["tst-a", t_ms * 1000, 0.1], ["tst-b", t_ms * 1000, 0.2], ["tst-c", t_ms * 1000, -0.1]]})
    }

    #[test]
    fn strikes_from_lightning_events_count_near_a_site() {
        let mut f = Fake::new(vec![Box::new(Strikes) as Box<dyn Projection>]);
        f.add(
            "lightning.strikes",
            json!({"velocity_factor": 0.9922, "strikes": [
                strike("aa01", T, 35.40, false),
                strike("aa02", T + 60_000, 35.70, false),
                strike("aa03", T + 120_000, 35.33, true),
                strike("zz", T, 35.33, false),
                {"id": "aa04", "t_us": "soon"},
            ]}),
            T + 300_000,
        );
        let site = [LatLon::new(35.33, -120.6)];
        let n = near(&f.conn, &site, T - 3_600_000, T + 3_600_000, 30.0).unwrap();
        // aa02 is 41 km away, aa03 is ambiguous, "zz" and aa04 don't parse
        assert_eq!(n.count, 1);
        assert!((n.nearest_km.unwrap() - 7.8).abs() < 0.2, "{n:?}");
        let later = near(&f.conn, &site, T + 3_600_000, T + 7_200_000, 30.0).unwrap();
        assert_eq!(later.count, 0);
        assert_eq!(later.nearest_km, None);
    }
}
