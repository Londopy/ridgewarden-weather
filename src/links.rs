//! Every RF link the app has data for: its length from the nodes' reported positions,
//! when it was first and last heard, and whether it is studied. A link that turns up once
//! the app has been watching for a day is new; new long ones are flagged, since long
//! paths near the coast are the ones the marine layer bends.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use ridgewarden_client::graph::edge;
use ridgewarden_lightning::geo::distance_km;
use rusqlite::Connection;

use crate::config::Config;
use crate::detect::Pair;
use crate::evidence;
use crate::series::{self, DAY, HOUR};

/// How long a link counts as new after it first turns up.
const NEW_FOR: i64 = 14 * DAY;

#[derive(Clone, Debug, PartialEq)]
pub struct Seen {
    pub pair: Pair,
    /// when either end first and last reported it, as far back as hourly medians go
    pub first: i64,
    pub last: i64,
    /// from the nodes' reported positions, for links studied or new
    pub km: Option<f64>,
    pub studied: bool,
    /// turned up in the last two weeks, at least a day after the app's first data
    pub new: bool,
}

impl Seen {
    pub fn long(&self, cfg: &Config) -> bool {
        self.km.is_some_and(|k| k >= cfg.long_km)
    }
}

/// Every RF link the app has data for, in name order.
pub fn seen(conn: &Connection, cfg: &Config, now: i64) -> Result<Vec<Seen>> {
    let mut span: BTreeMap<Pair, (i64, i64)> = BTreeMap::new();
    // the rollup leaves the last hour or so in raw samples only
    for ((node, nb), (lo, hi)) in series::spans(conn, now - 2 * HOUR)? {
        let s = span.entry(edge(&node, &nb)).or_insert((lo, hi));
        *s = (s.0.min(lo), s.1.max(hi));
    }
    let watching = span.values().map(|s| s.0).min().unwrap_or(now);
    let new = |first: i64| first >= watching + DAY && now - first <= NEW_FOR;
    // a big crawl hears many links nobody studies; their lengths aren't needed here
    let wanted: BTreeSet<&String> = span
        .iter()
        .filter(|((a, b), (first, _))| cfg.studies(a, b) || new(*first))
        .flat_map(|((a, b), _)| [a, b])
        .collect();
    let pos = evidence::positions(conn, wanted)?;
    Ok(span
        .into_iter()
        .map(|((a, b), (first, last))| Seen {
            km: match (pos.get(&a), pos.get(&b)) {
                (Some(x), Some(y)) => Some(distance_km(*x, *y)),
                _ => None,
            },
            studied: cfg.studies(&a, &b),
            new: new(first),
            pair: (a, b),
            first,
            last,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::series::{HOUR, MIN, SCHEMA};
    use ridgewarden_client::core::Projection;
    use ridgewarden_client::mesh::Mesh;
    use ridgewarden_client::mesh::testing::Fake;
    use rusqlite::params;
    use serde_json::json;

    const T0: i64 = 1_795_000_000_000 - 1_795_000_000_000 % DAY;

    #[test]
    fn a_link_that_turns_up_later_is_new_and_long_ones_stand_out() {
        let mut f = Fake::new(vec![Box::new(Mesh) as Box<dyn Projection>]);
        f.conn.execute_batch(SCHEMA).unwrap();
        for (node, lat) in [("TST-A", 35.0), ("TST-B", 35.1), ("TST-C", 35.3)] {
            f.add(
                "node.poll",
                json!({"node": node, "ok": true, "lat": lat, "lon": -120.0, "links": []}),
                T0,
            );
        }
        let hour = |node: &str, nb: &str, at: i64| {
            f.conn
                .execute(
                    "INSERT INTO link_hours (node, neighbor, hour, n, signal) VALUES (?1, ?2, ?3, 60, -60)",
                    params![node, nb, at],
                )
                .unwrap();
        };
        // a-b from the start; b-c appears ten days later, heard from both ends
        hour("tst-a", "tst-b", T0);
        hour("tst-b", "tst-a", T0 + 20 * DAY);
        hour("tst-c", "tst-b", T0 + 10 * DAY);
        hour("tst-b", "tst-c", T0 + 10 * DAY + HOUR);
        let mut cfg: Config = toml::from_str("links = [[\"tst-a\", \"tst-b\"]]").unwrap();
        cfg.validate().unwrap();
        let now = T0 + 20 * DAY;
        let s = seen(&f.conn, &cfg, now).unwrap();
        assert_eq!(s.len(), 2);
        let (ab, bc) = (&s[0], &s[1]);
        assert_eq!((ab.first, ab.last), (T0, T0 + 20 * DAY));
        assert!(!ab.new && ab.studied);
        assert_eq!(bc.pair, ("tst-b".to_string(), "tst-c".to_string()));
        assert!(bc.new && !bc.studied);
        // with only hourly medians left, first heard is the start of its first hour
        assert_eq!(bc.first, T0 + 10 * DAY);
        // b and c are 0.2 degrees of latitude apart: about 22 km, long at the default 20
        assert!((bc.km.unwrap() - 22.2).abs() < 0.2, "{:?}", bc.km);
        assert!(bc.long(&cfg) && !ab.long(&cfg));
        // two weeks on it is just another link, and one nobody studies: no length lookup
        let later = &seen(&f.conn, &cfg, T0 + 25 * DAY).unwrap()[1];
        assert!(!later.new && later.km.is_none());
        // while that hour's raw samples are kept, it is the first sample's time
        let first = T0 + 10 * DAY + 25 * MIN;
        f.add(
            "node.poll",
            json!({"node": "TST-C", "ok": true, "lat": 35.3, "lon": -120.0, "links": [
                {"neighbor": "TST-B", "type": "RF", "signal": -70.0, "noise": -95.0}]}),
            first,
        );
        assert_eq!(seen(&f.conn, &cfg, now).unwrap()[1].first, first);
        // one heard half an hour ago is there before its first hour is rolled up
        f.add(
            "node.poll",
            json!({"node": "TST-C", "ok": true, "lat": 35.3, "lon": -120.0, "links": [
                {"neighbor": "TST-A", "type": "RF", "signal": -71.0, "noise": -95.0}]}),
            now - HOUR / 2,
        );
        let s = seen(&f.conn, &cfg, now).unwrap();
        let ac = &s[1];
        assert_eq!(ac.pair, ("tst-a".to_string(), "tst-c".to_string()));
        assert_eq!((ac.first, ac.last), (now - HOUR / 2, now - HOUR / 2));
        assert!(ac.new && ac.long(&cfg));
    }
}
