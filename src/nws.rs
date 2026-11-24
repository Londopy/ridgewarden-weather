//! Rain from NWS surface observations, from api.weather.gov.
//!
//! `GET /stations/{id}/observations?start=<RFC 3339>` returns a GeoJSON FeatureCollection.
//! Each feature's `properties` has `timestamp` ("2026-11-19T20:56:00+00:00"),
//! `stationId`, `textDescription` ("Light Rain"), and, on the hourly METAR reports,
//! `precipitationLastHour` as `{"unitCode": "wmoUnit:mm", "value": 0, ...}`. The value
//! is null when not measured and the field is missing from the 5-minute reports. The API
//! refuses requests without a User-Agent naming the app and a contact.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ridgewarden_client::util::{ms_to_rfc3339, now_ms, rfc3339_to_ms};
use serde_json::Value;

use crate::App;
use crate::config::NwsCfg;
use crate::rain::{Report, store};
use crate::series::{DAY, HOUR};

/// Rain reports in an observations response. Observations that say nothing about rain
/// (no gauge figure and no description) are left out.
pub fn parse(v: &Value, station: &str) -> Vec<Report> {
    let Some(features) = v.get("features").and_then(Value::as_array) else {
        return vec![];
    };
    features
        .iter()
        .filter_map(|f| {
            let p = f.get("properties")?;
            let ts = rfc3339_to_ms(p.get("timestamp")?.as_str()?)?;
            let mm = p.get("precipitationLastHour").and_then(|q| {
                let v = q.get("value")?.as_f64()?;
                match q.get("unitCode")?.as_str()? {
                    "wmoUnit:mm" => Some(v),
                    "wmoUnit:m" => Some(v * 1000.0),
                    _ => None,
                }
            });
            let text = p
                .get("textDescription")
                .and_then(Value::as_str)
                .map(|t| t.chars().take(80).collect::<String>())
                .filter(|t| !t.trim().is_empty());
            if mm.is_none() && text.is_none() {
                return None;
            }
            Some(Report {
                source: "nws".into(),
                station: station.to_string(),
                ts,
                rain_1h_mm: mm.filter(|m| (0.0..1000.0).contains(m)),
                rain_24h_mm: None,
                text,
                ..Default::default()
            })
        })
        .collect()
}

async fn fetch(app: &App, cfg: &NwsCfg, station: &str, from: i64) -> Result<Vec<Report>> {
    let r = app
        .http
        .get(format!(
            "{}/stations/{station}/observations",
            cfg.url.trim_end_matches('/')
        ))
        .query(&[("start", ms_to_rfc3339(from))])
        .header(reqwest::header::USER_AGENT, &cfg.user_agent)
        .header(reqwest::header::ACCEPT, "application/geo+json")
        .send()
        .await
        .map_err(reqwest::Error::without_url)
        .with_context(|| crate::host(&cfg.url))?;
    if !r.status().is_success() {
        bail!("{} answered {}", crate::host(&cfg.url), r.status());
    }
    let v: Value = serde_json::from_slice(&crate::body(r, 4_000_000).await?)?;
    Ok(parse(&v, station))
}

/// From the last report on file, or three days back for a new station.
fn since(app: &App, station: &str, now: i64) -> Result<i64> {
    let last: Option<i64> = app.db().query_row(
        "SELECT max(ts) FROM rain WHERE source = 'nws' AND station = ?1",
        [station],
        |r| r.get(0),
    )?;
    Ok(last.map_or(now - 3 * DAY, |t| (t + 1000).max(now - 3 * DAY)))
}

async fn round(app: &App, cfg: &NwsCfg) {
    for station in app.cfg.nws_stations() {
        let key = format!("nws {station}");
        let res = async {
            let from = since(app, &station, now_ms())?.min(now_ms() - HOUR);
            let reports = fetch(app, cfg, &station, from).await?;
            let db = app.db();
            for r in &reports {
                store(&db, r)?;
            }
            anyhow::Ok(())
        }
        .await;
        match res {
            Ok(()) => app.ok(&key),
            Err(e) => app.failed(&key, &format!("NWS {station}: {e:#}")),
        }
    }
}

/// Fetch the sites' NWS observations every `interval_min`, forever.
pub async fn run(app: Arc<App>) {
    let Some(cfg) = app.cfg.nws.clone() else {
        return;
    };
    if app.cfg.nws_stations().is_empty() {
        return;
    }
    loop {
        round(&app, &cfg).await;
        tokio::time::sleep(Duration::from_secs(cfg.interval_min as u64 * 60)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_rain_and_descriptions_and_skips_empty_observations() {
        // shaped like what api.weather.gov returned for KSBP on 2026-11-19; some values changed
        let v: Value =
            serde_json::from_str(include_str!("../tests/fixtures/nws-ksbp.json")).unwrap();
        let r = parse(&v, "KSBP");
        assert_eq!(r.len(), 4, "{r:?}");
        let hourly = r
            .iter()
            .find(|x| x.ts == rfc3339_to_ms("2026-11-19T20:56:00+00:00").unwrap())
            .unwrap();
        assert_eq!(hourly.rain_1h_mm, Some(0.0));
        assert_eq!(hourly.wet(), Some(false));
        let raining = r
            .iter()
            .find(|x| x.ts == rfc3339_to_ms("2026-11-20T00:20:00+00:00").unwrap())
            .unwrap();
        assert_eq!(raining.rain_1h_mm, None);
        assert_eq!(raining.wet(), Some(true));
        assert!(r.iter().any(|x| x.rain_1h_mm == Some(0.8)));
        assert!(parse(&serde_json::json!({"title": "error"}), "KSBP").is_empty());
    }
}
