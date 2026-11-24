//! Radar rain at each site's own position: the NWS Multi-Radar Multi-Sensor (MRMS)
//! radar-only estimate of the last hour's rain on a 1 km grid, from NOAA's image service.
//! Gauges sit in towns and valleys while backbone sites are on peaks; radar is the one
//! source that says what fell on the peak itself.
//!
//! `GET {url}/getSamples` with `geometry={"points": [[lon, lat], ...], "spatialReference":
//! {"wkid": 4326}}`, `geometryType=esriGeometryMultipoint`, a mosaic rule picking the
//! newest raster named `product`, `returnFirstValueOnly=true`,
//! `outFields=name,idp_validendtime` and `f=json` answers
//! `{"samples": [{"locationId": i, "value": "0.100000000", "attributes": {"name":
//! "conus_QPE_01H", "idp_validendtime": <unix ms>}}, ...]}`: inches of rain in the hour
//! ending at `idp_validendtime` at point i, in steps of 0.1 in, -3 where no radar covers
//! the point, and no sample at all off the grid. A refusal is `{"error": {"code",
//! "message"}}`, sometimes with status 200. The service keeps only the latest hour and
//! NOAA posts it some minutes after the hour, so a late or failed hour is asked for again
//! until 50 past, as is one with no studied site heard yet (on a fresh start); an hour the
//! app misses altogether is gone. Readings are stored as rain
//! reports from source "mrms", with the site's node name as the station. A reading of
//! 0.1 in or more is a rain hour; a 0 can still be drizzle, so it isn't a dry one.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ridgewarden_client::util::now_ms;
use ridgewarden_lightning::geo::LatLon;
use serde_json::{Value, json};

use crate::App;
use crate::config::RadarCfg;
use crate::evidence;
use crate::links;
use crate::rain::{self, Report};
use crate::series::{DAY, HOUR, MIN};

pub const SOURCE: &str = "mrms";

/// Points per request: they go in the URL, which servers cap at a few kB.
const BATCH: usize = 50;

/// Rain reports from a getSamples answer for points at `nodes`, in order, keeping only
/// samples from `product`, and the newest raster time among them. A time past `now` is
/// taken as `now`.
pub fn parse(
    v: &Value,
    nodes: &[String],
    product: &str,
    now: i64,
) -> Result<(Vec<Report>, Option<i64>)> {
    if let Some(e) = v.get("error") {
        bail!(
            "the radar service refused the request: {}",
            e.get("message")
                .and_then(Value::as_str)
                .unwrap_or("no reason given")
        );
    }
    let Some(samples) = v.get("samples").and_then(Value::as_array) else {
        bail!("the radar service's answer has no samples");
    };
    let mut out = vec![];
    let mut newest = None;
    for s in samples {
        let attr = |k: &str| s.get("attributes").and_then(|a| a.get(k));
        let Some(ts) = attr("idp_validendtime").and_then(Value::as_i64) else {
            continue;
        };
        if attr("name").and_then(Value::as_str) != Some(product) {
            continue;
        }
        let ts = ts.min(now);
        newest = newest.max(Some(ts));
        let node = s
            .get("locationId")
            .and_then(Value::as_u64)
            .and_then(|i| nodes.get(i as usize));
        let inches = s
            .get("value")
            .and_then(|x| x.as_f64().or_else(|| x.as_str()?.trim().parse().ok()))
            // negative is the grid's mark for no radar coverage
            .filter(|x| (0.0..20.0).contains(x));
        let (Some(node), Some(inches)) = (node, inches) else {
            continue;
        };
        out.push(Report {
            source: SOURCE.into(),
            station: node.clone(),
            ts,
            rain_1h_mm: Some(inches * 25.4),
            ..Default::default()
        });
    }
    Ok((out, newest))
}

/// The newest raster of `product`.
fn mosaic(product: &str) -> String {
    json!({
        "mosaicMethod": "esriMosaicAttribute",
        "where": format!("name = '{product}'"),
        "sortField": "idp_validendtime",
        "sortValue": "2100-01-01",
        "ascending": true,
    })
    .to_string()
}

/// The ends of every studied link heard in the last two days, where the node reports its
/// position.
fn places(app: &App, now: i64) -> Result<Vec<(String, LatLon)>> {
    let db = app.db();
    let nodes: BTreeSet<String> = links::seen(&db, &app.cfg, now)?
        .into_iter()
        .filter(|l| l.studied && now - l.last <= 2 * DAY)
        .flat_map(|l| [l.pair.0, l.pair.1])
        .collect();
    Ok(evidence::positions(&db, &nodes)?.into_iter().collect())
}

async fn fetch(
    http: &reqwest::Client,
    cfg: &RadarCfg,
    places: &[(String, LatLon)],
    now: i64,
) -> Result<(Vec<Report>, Option<i64>)> {
    let points: Vec<[f64; 2]> = places.iter().map(|(_, p)| [p.lon, p.lat]).collect();
    let geometry = json!({"points": points, "spatialReference": {"wkid": 4326}}).to_string();
    let r = http
        .get(format!("{}/getSamples", cfg.url.trim_end_matches('/')))
        .query(&[
            ("geometry", geometry.as_str()),
            ("geometryType", "esriGeometryMultipoint"),
            ("mosaicRule", mosaic(&cfg.product).as_str()),
            ("returnFirstValueOnly", "true"),
            ("outFields", "name,idp_validendtime"),
            ("f", "json"),
        ])
        .send()
        .await
        // the query holds every site's position: too long to show with the error
        .map_err(reqwest::Error::without_url)
        .with_context(|| crate::host(&cfg.url))?;
    if !r.status().is_success() {
        bail!("{} answered {}", crate::host(&cfg.url), r.status());
    }
    let v: Value = serde_json::from_slice(&crate::body(r, 1_000_000).await?)?;
    let nodes: Vec<String> = places.iter().map(|(n, _)| n.clone()).collect();
    parse(&v, &nodes, &cfg.product, now)
}

/// The readings at every place, asked for in batches, and the newest raster time seen.
async fn sample(
    http: &reqwest::Client,
    cfg: &RadarCfg,
    places: &[(String, LatLon)],
    now: i64,
) -> Result<(Vec<Report>, Option<i64>)> {
    let mut out = vec![];
    let mut newest = None;
    for batch in places.chunks(BATCH) {
        let (r, n) = fetch(http, cfg, batch, now).await?;
        out.extend(r);
        newest = newest.max(n);
    }
    Ok((out, newest))
}

#[derive(Debug, PartialEq)]
enum Round {
    Done,
    /// worth asking again within the hour: its raster isn't out yet, or no site is heard yet
    Again,
    Failed,
}

/// `Again` when the newest raster is from before the hour that just ended.
fn outcome(newest: Option<i64>, now: i64) -> Round {
    if newest.is_some_and(|t| t < now - now.rem_euclid(HOUR)) {
        Round::Again
    } else {
        Round::Done
    }
}

/// When to ask next: ten past the next hour, or sooner while there's reason to ask again
/// (every 4 minutes) or the service fails (every 15), up to 50 past.
fn next_try(now: i64, r: Round) -> i64 {
    let hour = now - now.rem_euclid(HOUR);
    let next = hour + HOUR + 10 * MIN;
    let gap = match r {
        Round::Done => return next,
        Round::Again => 4 * MIN,
        Round::Failed => 15 * MIN,
    };
    if now + gap <= hour + 50 * MIN {
        now + gap
    } else {
        next
    }
}

async fn round(app: &App, cfg: &RadarCfg) -> Result<Round> {
    let now = now_ms();
    let places = places(app, now)?;
    if places.is_empty() {
        // no studied site heard with a position yet, as on a fresh start: no request needed
        return Ok(Round::Again);
    }
    let (reports, newest) = sample(&app.http, cfg, &places, now).await?;
    for r in &reports {
        rain::store(&app.db(), r)?;
    }
    Ok(outcome(newest, now))
}

/// Fetch the radar rain at every studied site ten minutes past each hour, forever.
pub async fn run(app: Arc<App>) {
    let Some(cfg) = app.cfg.radar.clone() else {
        return;
    };
    // the sites come from the feed
    app.caught_up().await;
    loop {
        let r = match round(&app, &cfg).await {
            Ok(r) => {
                app.ok("radar");
                r
            }
            Err(e) => {
                app.failed("radar", &format!("radar (MRMS): {e:#}"));
                Round::Failed
            }
        };
        let now = now_ms();
        let wait = (next_try(now, r) - now).max(0) as u64;
        tokio::time::sleep(Duration::from_millis(wait)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    /// The end of an hour, and ten minutes later.
    const END: i64 = 1_795_161_600_000;
    const NOW: i64 = END + 10 * MIN;

    #[test]
    fn reads_rain_at_each_point_and_skips_no_coverage() {
        // shaped like what the service returned on 2026-11-20; values changed
        let v: Value =
            serde_json::from_str(include_str!("../tests/fixtures/mrms-samples.json")).unwrap();
        let nodes = ["tst-a", "tst-b", "tst-c", "tst-d"].map(String::from);
        let (r, newest) = parse(&v, &nodes, "conus_QPE_01H", NOW).unwrap();
        // tst-c has no radar coverage, and tst-d's sample is from another product
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(newest, Some(END));
        assert_eq!((r[0].station.as_str(), r[0].ts), ("tst-a", END));
        assert_eq!(r[0].rain_1h_mm, Some(0.0));
        // under 0.1 in can still be drizzle: not a dry hour
        assert_eq!(r[0].wet(), None);
        assert_eq!(r[1].station, "tst-b");
        assert!((r[1].rain_1h_mm.unwrap() - 2.54).abs() < 1e-9);
        assert_eq!(r[1].wet(), Some(true));
        assert!(
            rain::verdict(&r)
                .text()
                .starts_with("Radar (MRMS) shows 2.5 mm")
        );
        // a raster time ahead of the clock is taken as now
        let (r, newest) = parse(&v, &nodes, "conus_QPE_01H", END - MIN).unwrap();
        assert_eq!((r[0].ts, newest), (END - MIN, Some(END - MIN)));
        let refused = json!({"error": {"code": 400, "message": "Invalid or missing input parameters.", "details": []}});
        let e = parse(&refused, &nodes, "conus_QPE_01H", NOW).unwrap_err();
        assert!(e.to_string().contains("Invalid or missing"), "{e}");
    }

    #[test]
    fn the_mosaic_rule_picks_the_newest_raster_of_the_product() {
        let m: Value = serde_json::from_str(&mosaic("conus_QPE_01H")).unwrap();
        assert_eq!(m["where"], "name = 'conus_QPE_01H'");
        assert_eq!(m["sortField"], "idp_validendtime");
        assert_eq!(m["ascending"], true);
    }

    #[test]
    fn a_late_or_failed_hour_is_asked_for_again_until_fifty_past() {
        let at = |m: i64| END + m * MIN;
        let next_hour = END + HOUR + 10 * MIN;
        assert_eq!(outcome(Some(END), at(10)), Round::Done);
        assert_eq!(next_try(at(10), Round::Done), next_hour);
        // the newest raster is still the hour before
        assert_eq!(outcome(Some(END - HOUR), at(10)), Round::Again);
        assert_eq!(next_try(at(10), Round::Again), at(14));
        assert_eq!(next_try(at(46), Round::Again), at(50));
        assert_eq!(next_try(at(47), Round::Again), next_hour);
        assert_eq!(next_try(at(10), Round::Failed), at(25));
        assert_eq!(next_try(at(40), Round::Failed), next_hour);
        // no samples at all says nothing about the hour
        assert_eq!(outcome(None, at(10)), Round::Done);
    }

    /// A stand-in for the image service answering each getSamples with `answer(points)`,
    /// and the number of points each request carried.
    async fn stub(answer: fn(usize) -> Value) -> (String, Arc<Mutex<Vec<usize>>>) {
        use axum::extract::Query;
        let asked: Arc<Mutex<Vec<usize>>> = Arc::default();
        let log = asked.clone();
        let app = axum::Router::new().route(
            "/getSamples",
            axum::routing::get(move |Query(q): Query<HashMap<String, String>>| {
                let log = log.clone();
                async move {
                    let g: Value = serde_json::from_str(&q["geometry"]).unwrap();
                    let n = g["points"].as_array().unwrap().len();
                    log.lock().unwrap().push(n);
                    axum::Json(answer(n))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, asked)
    }

    fn tenth_of_an_inch(n: usize, end: i64) -> Value {
        let samples: Vec<Value> = (0..n)
            .map(|i| {
                json!({"locationId": i, "value": "0.100000000",
                       "attributes": {"name": "conus_QPE_01H", "idp_validendtime": end}})
            })
            .collect();
        json!({ "samples": samples })
    }

    #[tokio::test]
    async fn asks_in_batches_and_tells_a_late_hour_and_errors_apart() {
        let http = reqwest::Client::new();
        let places: Vec<(String, LatLon)> = (0..60)
            .map(|i| {
                (
                    format!("tst-{i:02}"),
                    LatLon::new(35.0 + i as f64 / 100.0, -120.6),
                )
            })
            .collect();
        let cfg = |url: String| RadarCfg {
            url,
            product: "conus_QPE_01H".into(),
        };
        let (url, asked) = stub(|n| tenth_of_an_inch(n, END)).await;
        let (r, newest) = sample(&http, &cfg(url), &places, NOW).await.unwrap();
        assert_eq!(*asked.lock().unwrap(), [50, 10]);
        assert_eq!(r.len(), 60);
        assert_eq!((r[59].station.as_str(), r[59].ts), ("tst-59", END));
        assert_eq!(r[59].wet(), Some(true));
        assert_eq!(outcome(newest, NOW), Round::Done);
        let (url, _) = stub(|n| tenth_of_an_inch(n, END - HOUR)).await;
        let (_, newest) = sample(&http, &cfg(url), &places, NOW).await.unwrap();
        assert_eq!(outcome(newest, NOW), Round::Again);
        let (url, _) = stub(
            |_| json!({"error": {"code": 400, "message": "Invalid or missing input parameters."}}),
        )
        .await;
        let e = sample(&http, &cfg(url.clone()), &places, NOW)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("Invalid or missing"), "{e}");
        // a configured path can hold a key; the pages and /api/overview show the error
        let e = sample(&http, &cfg(format!("{url}/key-abc123")), &places, NOW)
            .await
            .unwrap_err();
        assert_eq!(e.to_string(), "127.0.0.1 answered 404 Not Found");
        // nothing listening: the error doesn't carry the URL and every site's position
        let gone = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let e = sample(
            &http,
            &cfg(format!("http://{gone}/key-abc123")),
            &places,
            NOW,
        )
        .await
        .unwrap_err();
        let e = format!("{e:#}");
        assert!(e.starts_with("127.0.0.1: "), "{e}");
        assert!(
            !e.contains("key-abc123") && !e.contains("geometry") && !e.contains("120.6"),
            "{e}"
        );
    }
}
