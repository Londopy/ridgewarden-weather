//! The pages: an overview of the links and weather sources, a residual chart per link
//! with rain and sounding markers, the candidate events with their evidence, the methods,
//! and the OpenSense export.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use ridgewarden_client::graph::edge;
use ridgewarden_client::svg::{Chart, Line};
use ridgewarden_client::ui::{Color, Page, pill};
use ridgewarden_client::util::{esc, human_duration, ms_to_rfc3339, norm_node, now_ms, short_time};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::App;
use crate::config::Config;
use crate::detect::Pair;
use crate::events::{self, Candidate};
use crate::evidence;
use crate::links::{self, Seen};
use crate::now::{self, Now};
use crate::rain::{self, Report};
use crate::results;
use crate::series::{self, DAY, End, HOUR, MIN};
use crate::sounding::{self, Sounding};
use ridgewarden_lightning::geo::distance_km;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(overview))
        .route("/link", get(link_page))
        .route("/events", get(events_page))
        .route("/methods", get(methods_page))
        .route("/export", get(export_page))
        .route("/export.nc", get(export_nc))
        .route("/results", get(results_page))
        .route("/api/events", get(events_json))
        .route("/api/now", get(now_json))
        .route("/api/overview", get(overview_json))
        .route("/api/link", get(link_json))
        .route("/api/links.geojson", get(links_geojson))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app)
}

struct Fail(anyhow::Error);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        tracing::error!("request failed: {:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for Fail {
    fn from(e: E) -> Self {
        Fail(e.into())
    }
}

const NAV: &[(&str, &str)] = &[
    ("/", "Now"),
    ("/events", "Events"),
    ("/results", "Results"),
    ("/methods", "Methods"),
    ("/export", "Dataset"),
];

fn page(here: &str, active: &str, title: &str, body: &str) -> Html<String> {
    let p = Page {
        app: "ridgewarden-weather (experimental)",
        nav: NAV,
        active,
        who: here,
        refresh: active == "/",
    };
    Html(p.render(title, body))
}

/// Percent-encode a query value.
pub fn enc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            o.push(b as char);
        } else {
            let _ = write!(o, "%{b:02X}");
        }
    }
    o
}

/// "2026-11-18 19:40 UTC (11:40 local)"
fn when(cfg: &Config, ms: i64) -> String {
    let local = ms_to_rfc3339(ms + cfg.utc_offset_h as i64 * HOUR);
    format!("{} ({} local)", short_time(ms), &local[11..16])
}

fn link_href(a: &str, b: &str) -> String {
    format!("/link?a={}&amp;b={}", enc(a), enc(b))
}

fn link_name(cfg: &Config, a: &str, b: &str) -> String {
    format!("{} &harr; {}", esc(&cfg.label(a)), esc(&cfg.label(b)))
}

struct EndStatus {
    end: End,
    samples: usize,
    /// the newest sample's time
    last: i64,
    every: Option<i64>,
    baseline_days: usize,
    residual: Option<f64>,
}

type Histories = BTreeMap<End, series::History>;

/// Every link end's history behind the baselines of the last day: what `end_status` and
/// `now::current` both need, read once.
fn recent_history(conn: &Connection, cfg: &Config, now: i64) -> anyhow::Result<Histories> {
    let from = now - (cfg.baseline.days as i64 + 1) * DAY;
    series::history(conn, cfg, from, now + 1)
}

fn end_status(
    conn: &Connection,
    cfg: &Config,
    now: i64,
    hist: &Histories,
) -> anyhow::Result<Vec<EndStatus>> {
    let recent = series::all_samples(conn, now - DAY, now + 1)?;
    let none = series::History::default();
    let today = series::day_of(now);
    let mut out: Vec<EndStatus> = recent
        .into_iter()
        .filter(|(e, _)| cfg.studies(&e.0, &e.1))
        .map(|(end, s)| {
            let h = hist.get(&end).unwrap_or(&none);
            let tail = &s[s.len().saturating_sub(1)..];
            EndStatus {
                samples: s.len(),
                last: s.last().map_or(0, |x| x.ts),
                every: series::interval(&s),
                baseline_days: series::baseline_days(h, today, &cfg.baseline),
                residual: series::residuals(tail, h, &cfg.baseline)
                    .last()
                    .map(|r| r.1),
                end,
            }
        })
        .collect();
    out.sort_by_key(|e| (edge(&e.end.0, &e.end.1), e.end.0.clone()));
    Ok(out)
}

/// A candidate's heading, as plain text.
fn candidate_title(cfg: &Config, c: &Candidate) -> String {
    match c {
        Candidate::Wet(w) => format!("Wet antenna? at {}", cfg.label(&w.site)),
        Candidate::Duct(d) => format!("Ducting or fading? on {} links", d.links.len()),
        Candidate::Front(f) => format!(
            "Widespread drop? {} links at {} sites",
            f.drops.len(),
            f.sites.len()
        ),
    }
}

fn candidate_pill(c: &Candidate) -> String {
    let (color, word) = c.pill();
    pill(color, word)
}

async fn overview(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let here = app.here();
    let now = now_ms();
    let cfg = &app.cfg;
    let a = app.clone();
    let (ends, ind, seen) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let db = a.db();
        let hist = recent_history(&db, &a.cfg, now)?;
        let ends = end_status(&db, &a.cfg, now, &hist)?;
        let ind = now::current(&db, &a.cfg, now, &hist)?;
        let seen = links::seen(&db, &a.cfg, now)?;
        Ok((ends, ind, seen))
    })
    .await??;
    let mut b = String::from(
        "<h1>Mesh weather sensor</h1><p class=banner>Experimental. This looks for weather \
         effects in the link levels the mesh already reports and lists them as candidates \
         with their evidence. It does not measure rain. <a href=\"/methods\">How it works</a></p>",
    );
    b.push_str(&indicator_html(&ind));
    b.push_str(&status_html(&app, now));
    let recent = events::list(&app.db(), now - 7 * DAY, now + 1, None)?;
    b.push_str("<h2>Candidates in the last 7 days</h2>");
    if recent.is_empty() {
        b.push_str("<p class=mute>None.</p>");
    } else {
        b.push_str("<ul class=plain>");
        for c in recent.iter().take(10) {
            let _ = write!(
                b,
                "<li>{} {} <span class=\"mute small\">{}</span></li>",
                candidate_pill(c),
                esc(&candidate_title(cfg, c)),
                esc(&when(cfg, c.start()))
            );
        }
        let _ = write!(
            b,
            "</ul><p><a href=\"/events\">All candidates and their evidence</a></p>"
        );
    }
    b.push_str("<h2>Links</h2>");
    let pos = evidence::positions(&app.db(), ends.iter().flat_map(|e| [&e.end.0, &e.end.1]))?;
    let length = |a: &str, c: &str| match (pos.get(a), pos.get(c)) {
        (Some(x), Some(y)) => format!("{:.1} km", distance_km(*x, *y)),
        _ => "&ndash;".into(),
    };
    if ends.is_empty() {
        b.push_str(
            "<p>No RF link samples in the last 24 hours yet. This app reads the link levels \
             in the core's node polls; for the backbone, the hub core needs <code>[crawl]</code> \
             on with <code>interval_s = 60</code> for the backbone nodes, agreed with their owners.</p>",
        );
    } else {
        let _ = write!(
            b,
            "<p class=\"mute small\">A baseline needs {} days of history at every hour of day, \
             out of the last {}. The residual is the latest level minus the usual for that \
             hour.</p>\
             <div class=wrap><table><tr><th>Link</th><th>Length</th><th>Heard at</th>\
             <th>Samples, 24 h</th><th>Every</th><th>Baseline</th><th>Residual now</th></tr>",
            cfg.baseline.min_days, cfg.baseline.days
        );
        for e in &ends {
            let (a, c) = edge(&e.end.0, &e.end.1);
            let base = if e.baseline_days >= cfg.baseline.min_days as usize {
                pill(Color::Green, "ready")
            } else {
                format!(
                    "{} <span class=\"mute small\">{} of {} days</span>",
                    pill(Color::Unknown, "building"),
                    e.baseline_days,
                    cfg.baseline.min_days
                )
            };
            let _ = write!(
                b,
                "<tr><td><a href=\"{}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{base}</td><td>{}</td></tr>",
                link_href(&a, &c),
                link_name(cfg, &a, &c),
                length(&a, &c),
                esc(&cfg.label(&e.end.0)),
                e.samples,
                e.every
                    .map(human_duration)
                    .unwrap_or_else(|| "&ndash;".into()),
                e.residual
                    .map(|r| format!("{r:+.1} dB"))
                    .unwrap_or_else(|| "&ndash;".into()),
            );
        }
        b.push_str("</table></div>");
        let slow = ends.iter().filter_map(|e| e.every).any(|x| x > 5 * MIN);
        if slow {
            b.push_str(
                "<p class=\"mute small\">Some links are sampled less than every 5 minutes. Ducting \
                 still shows; the onset of a wet-antenna drop is harder to see.</p>",
            );
        }
        b.push_str(
            "<p class=\"mute small\">Every link as a map layer: <a href=\"/api/links.geojson\">\
             GeoJSON</a>, with each link's length, residual and recent candidates. Each link's \
             page has its data as JSON too.</p>",
        );
    }
    b.push_str(&new_links_html(cfg, &seen));
    sources_html(&app, &mut b)?;
    Ok(page(&here, "/", "Now", &b))
}

/// The new links, longest first.
fn new_longest(seen: &[Seen]) -> Vec<&Seen> {
    let mut new: Vec<&Seen> = seen.iter().filter(|l| l.new).collect();
    new.sort_by(|x, y| y.km.unwrap_or(0.0).total_cmp(&x.km.unwrap_or(0.0)));
    new
}

fn new_links_html(cfg: &Config, seen: &[Seen]) -> String {
    let new = new_longest(seen);
    if new.is_empty() {
        return String::new();
    }
    let mut b = format!(
        "<h2>New links</h2><p class=\"mute small\">First heard in the last two weeks, longest \
         first. Paths of {:.1} km or more are the ones the marine layer bends, so they are \
         the ones worth studying.</p><div class=wrap><table><tr><th>Link</th><th>Length</th>\
         <th>First heard</th><th>Studied</th></tr>",
        cfg.long_km
    );
    let more = new.len().saturating_sub(NEW_SHOWN);
    for l in new.into_iter().take(NEW_SHOWN) {
        let (a, c) = (&l.pair.0, &l.pair.1);
        let length = match l.km {
            Some(k) if l.long(cfg) => format!("{k:.1} km {}", pill(Color::Yellow, "long")),
            Some(k) => format!("{k:.1} km"),
            None => "&ndash;".into(),
        };
        let _ = write!(
            b,
            "<tr><td><a href=\"{}\">{}</a></td><td>{length}</td><td>{}</td><td>{}</td></tr>",
            link_href(a, c),
            link_name(cfg, a, c),
            esc(&when(cfg, l.first)),
            if l.studied {
                "yes"
            } else {
                "no: add it to <code>links</code> in the config"
            }
        );
    }
    b.push_str("</table></div>");
    if more > 0 {
        let _ = write!(
            b,
            "<p class=\"mute small\">And {more} more, shorter. Every link is in the \
             <a href=\"/api/links.geojson\">map layer</a>.</p>"
        );
    }
    b
}

/// New links listed on the front page; a crawl that has just reached a new part of the
/// mesh can hear hundreds.
const NEW_SHOWN: usize = 25;

fn indicator_html(n: &Now) -> String {
    let (color, word) = n.state.pill();
    let mut b = format!(
        "<h2>Marine layer now</h2><p>{} <span class=\"mute small\">{}</span></p><p>{}</p><ul class=\"plain small\">",
        pill(color, word),
        n.confidence.words(),
        esc(&n.verdict)
    );
    for e in &n.evidence {
        let _ = write!(b, "<li>{}</li>", esc(e));
    }
    b.push_str(
        "</ul><p class=\"mute small\">An indication from the link levels and the last sounding, \
         not a measurement. <a href=\"/api/now\">As JSON</a></p>",
    );
    b
}

fn status_html(app: &App, now: i64) -> String {
    let ran = match *app.analyzed.lock().unwrap() {
        Some(t) => format!("last ran {} ago", human_duration(now - t)),
        None => "first run once the core's feed has been read".into(),
    };
    let publishing = if app.publishing {
        "Candidates are published to the mesh as signed <code>weather.candidate</code> events."
    } else if app.cfg.publish {
        "Candidates are not being published: the core token couldn't be read (see below)."
    } else {
        "Publishing to the mesh is off (<code>publish = false</code>)."
    };
    format!(
        "<p class=\"mute small\">Detection runs every 10 minutes ({}). {publishing}</p>",
        esc(&ran)
    )
}

fn report_text(r: &Report) -> String {
    let mut parts = vec![];
    if let Some(mm) = r.rain_1h_mm {
        parts.push(format!("{mm:.1} mm in the last hour"));
    }
    if let Some(t) = &r.text {
        parts.push(t.clone());
    }
    parts.join(", ")
}

/// The radar's newest hour over every site.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RadarHour {
    /// when the hour ended
    pub at_ms: i64,
    pub sites: usize,
    /// sites with 0.1 in (2.5 mm) or more, and the most at any of them
    pub wet_sites: usize,
    pub max_mm: Option<f64>,
}

fn radar_hour(radar: &[&Report]) -> Option<RadarHour> {
    let at_ms = radar.iter().map(|r| r.ts).max()?;
    let hour: Vec<&Report> = radar.iter().filter(|r| r.ts == at_ms).copied().collect();
    let wet: Vec<f64> = hour
        .iter()
        .filter(|r| r.wet() == Some(true))
        .filter_map(|r| r.rain_1h_mm)
        .collect();
    Some(RadarHour {
        at_ms,
        sites: hour.len(),
        wet_sites: wet.len(),
        max_mm: wet.into_iter().reduce(f64::max),
    })
}

/// "1 site", "7 sites"
pub fn sites(n: usize) -> String {
    format!("{n} site{}", if n == 1 { "" } else { "s" })
}

/// The radar's newest hour as one row: a row per site would bury the gauges.
fn radar_row(cfg: &Config, radar: &[&Report]) -> String {
    let Some(h) = radar_hour(radar) else {
        return String::new();
    };
    let said = match h.max_mm {
        None => format!(
            "under 0.1 in (2.5 mm) at {}, which can still be light rain",
            sites(h.sites)
        ),
        Some(mm) => format!(
            "rain at {} of {}, up to {mm:.1} mm in the hour",
            h.wet_sites,
            sites(h.sites)
        ),
    };
    format!(
        "<tr><td>{}</td><td>radar (MRMS), at each site</td><td>{}</td><td>{}</td></tr>",
        sites(h.sites),
        esc(&when(cfg, h.at_ms)),
        esc(&said)
    )
}

fn sources_html(app: &App, b: &mut String) -> Result<(), Fail> {
    let cfg = &app.cfg;
    b.push_str("<h2>Weather data</h2>");
    let (reports, soundings) = {
        let db = app.db();
        (rain::latest(&db)?, sounding::latest(&db)?)
    };
    let max_h = cfg.sounding_height();
    if cfg.aprs.is_none()
        && cfg.nws.is_none()
        && cfg.radar.is_none()
        && cfg.soundings.is_none()
        && reports.is_empty()
    {
        b.push_str(
            "<p>No rain or sounding source is set up, so candidates can't be checked against \
             the weather. See <code>[aprs]</code>, <code>[nws]</code>, <code>[radar]</code> and \
             <code>[soundings]</code> in the config.</p>",
        );
    }
    if !reports.is_empty() {
        b.push_str("<div class=wrap><table><tr><th>Station</th><th>Source</th><th>Last report</th><th>Said</th></tr>");
        let (radar, gauges): (Vec<&Report>, Vec<&Report>) = reports
            .iter()
            .partition(|r| r.source == crate::radar::SOURCE);
        for r in gauges {
            let _ = write!(
                b,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&cfg.label(&r.station)),
                esc(&r.source),
                esc(&when(cfg, r.ts)),
                esc(&report_text(r))
            );
        }
        b.push_str(&radar_row(cfg, &radar));
        b.push_str("</table></div>");
    }
    for s in &soundings {
        let _ = write!(
            b,
            "<p>Sounding {} at {}: {}.</p>",
            esc(&s.station),
            esc(&short_time(s.ts)),
            esc(&s.summary(max_h))
        );
    }
    let problems = app.problems();
    if !problems.is_empty() {
        b.push_str("<ul class=plain>");
        for (_, at, why) in problems {
            let _ = write!(
                b,
                "<li>{} {} <span class=\"mute small\">{}</span></li>",
                pill(Color::Red, "error"),
                esc(&why),
                esc(&short_time(at))
            );
        }
        b.push_str("</ul>");
    }
    Ok(())
}

/// What the front page shows, as `/api/overview` returns it; `top` draws from it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Overview {
    pub generated_ms: i64,
    /// this site's node, "" before the core has polled it
    pub here: String,
    /// each site's label, by node
    pub labels: BTreeMap<String, String>,
    /// when detection last finished; None before its first round
    pub analyzed_ms: Option<i64>,
    /// `publish` in the config, and whether candidates really go out
    pub publish: bool,
    pub publishing: bool,
    /// a residual this far from zero is off the usual level (`duct.departure_db`)
    pub departure_db: f64,
    /// days every hour of day needs before a link has a baseline
    pub min_days: u32,
    pub now: Now,
    /// the studied links heard in the last day
    pub links: Vec<LinkRow>,
    /// the longest new links, and how many more there are
    pub new_links: Vec<NewLink>,
    pub new_more: usize,
    /// candidates in the last 7 days, newest first
    pub candidates: Vec<CandidateRow>,
    /// every rain gauge set for a site or heard from, with its newest report
    pub gauges: Vec<Gauge>,
    pub radar_on: bool,
    pub radar: Option<RadarHour>,
    /// every sounding station set or heard from, with its newest sounding
    pub soundings: Vec<SoundingRow>,
    /// what is failing now
    pub problems: Vec<Problem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LinkRow {
    pub a: String,
    pub b: String,
    pub length_km: Option<f64>,
    pub ends: Vec<EndRow>,
}

/// What `node` hears from `hears`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EndRow {
    pub node: String,
    pub hears: String,
    /// samples in the last day, the newest one's time and the usual gap between them
    pub samples: usize,
    pub last_ms: i64,
    pub every_ms: Option<i64>,
    /// days of history at its least covered hour of day
    pub baseline_days: usize,
    /// the newest sample minus its baseline
    pub residual_db: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewLink {
    pub a: String,
    pub b: String,
    pub length_km: Option<f64>,
    pub first_ms: i64,
    pub long: bool,
    pub studied: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandidateRow {
    pub candidate: Candidate,
    pub title: String,
    pub summary: String,
    /// the evidence, in sentences
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Gauge {
    pub station: String,
    /// "aprs" or "nws"
    pub source: String,
    /// the sites it is set for
    pub sites: Vec<String>,
    /// the newest report's time and what it said; None before the first
    pub at_ms: Option<i64>,
    pub said: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SoundingRow {
    pub station: String,
    pub at_ms: Option<i64>,
    pub summary: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Problem {
    /// "aprs", "radar", "nws <station>", "sounding <station>", "analysis" or "publish"
    pub source: String,
    pub at_ms: i64,
    pub what: String,
}

fn overview_data(app: &App, now: i64) -> anyhow::Result<Overview> {
    let cfg = &app.cfg;
    let db = app.db();
    let hist = recent_history(&db, cfg, now)?;
    let ends = end_status(&db, cfg, now, &hist)?;
    let pos = evidence::positions(&db, ends.iter().flat_map(|e| [&e.end.0, &e.end.1]))?;
    let mut rows: BTreeMap<Pair, LinkRow> = BTreeMap::new();
    for e in ends {
        let (a, b) = edge(&e.end.0, &e.end.1);
        let length_km = match (pos.get(&a), pos.get(&b)) {
            (Some(x), Some(y)) => Some(evidence::round(distance_km(*x, *y), 1)),
            _ => None,
        };
        let row = rows
            .entry((a.clone(), b.clone()))
            .or_insert_with(|| LinkRow {
                a,
                b,
                length_km,
                ends: vec![],
            });
        row.ends.push(EndRow {
            samples: e.samples,
            last_ms: e.last,
            every_ms: e.every,
            baseline_days: e.baseline_days,
            residual_db: e.residual.map(|r| evidence::round(r, 2)),
            node: e.end.0,
            hears: e.end.1,
        });
    }
    let seen = links::seen(&db, cfg, now)?;
    let new = new_longest(&seen);
    let candidates = events::list(&db, now - 7 * DAY, now + 1, None)?
        .into_iter()
        .map(|c| {
            Ok(CandidateRow {
                title: candidate_title(cfg, &c),
                summary: crate::publish::summary(cfg, &c),
                evidence: evidence::gather(&db, cfg, &c)?.lines(cfg),
                candidate: c,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    let reports = rain::latest(&db)?;
    let (radar, heard): (Vec<&Report>, Vec<&Report>) = reports
        .iter()
        .partition(|r| r.source == crate::radar::SOURCE);
    let near = |station: &str| -> Vec<String> {
        cfg.sites
            .iter()
            .filter(|s| s.aprs.iter().chain(&s.nws).any(|x| x == station))
            .map(|s| s.node.clone())
            .collect()
    };
    let mut gauges: Vec<Gauge> = heard
        .iter()
        .map(|r| Gauge {
            station: r.station.clone(),
            source: r.source.clone(),
            sites: near(&r.station),
            at_ms: Some(r.ts),
            said: report_text(r),
        })
        .collect();
    let aprs = cfg.aprs_stations().into_iter().map(|s| ("aprs", s));
    let set = aprs.chain(cfg.nws_stations().into_iter().map(|s| ("nws", s)));
    for (source, station) in set {
        if !gauges
            .iter()
            .any(|g| g.source == source && g.station == station)
        {
            gauges.push(Gauge {
                sites: near(&station),
                station,
                source: source.into(),
                at_ms: None,
                said: String::new(),
            });
        }
    }
    let max_h = cfg.sounding_height();
    let mut soundings: Vec<SoundingRow> = sounding::latest(&db)?
        .iter()
        .map(|s| SoundingRow {
            station: s.station.clone(),
            at_ms: Some(s.ts),
            summary: Some(s.summary(max_h)),
        })
        .collect();
    for station in cfg.soundings.iter().flat_map(|s| &s.stations) {
        if !soundings.iter().any(|s| &s.station == station) {
            soundings.push(SoundingRow {
                station: station.clone(),
                at_ms: None,
                summary: None,
            });
        }
    }
    Ok(Overview {
        generated_ms: now,
        here: app.here(),
        labels: cfg
            .sites
            .iter()
            .filter_map(|s| Some((s.node.clone(), s.label.clone()?)))
            .collect(),
        analyzed_ms: *app.analyzed.lock().unwrap(),
        publish: cfg.publish,
        publishing: app.publishing,
        departure_db: cfg.duct.departure_db,
        min_days: cfg.baseline.min_days,
        now: now::current(&db, cfg, now, &hist)?,
        links: rows.into_values().collect(),
        new_more: new.len().saturating_sub(NEW_SHOWN),
        new_links: new
            .into_iter()
            .take(NEW_SHOWN)
            .map(|l| NewLink {
                a: l.pair.0.clone(),
                b: l.pair.1.clone(),
                length_km: l.km.map(|k| evidence::round(k, 1)),
                first_ms: l.first,
                long: l.long(cfg),
                studied: l.studied,
            })
            .collect(),
        candidates,
        gauges,
        radar_on: cfg.radar.is_some(),
        radar: radar_hour(&radar),
        soundings,
        problems: app
            .problems()
            .into_iter()
            .map(|(source, at_ms, what)| Problem {
                source,
                at_ms,
                what,
            })
            .collect(),
    })
}

/// How long a worked-out overview is served again: each one reads every link's history,
/// and several `top`s polling at once mustn't each redo it.
const OVERVIEW_FOR: i64 = 5_000;

/// The overview as JSON, worked out at most every `OVERVIEW_FOR` and one request at a time.
fn overview_cached(app: &App, now: i64) -> anyhow::Result<String> {
    // held while working, so a request that comes in meanwhile waits and takes this one
    let mut cached = app.overview.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, json)) = cached.as_ref()
        && (0..OVERVIEW_FOR).contains(&(now - at))
    {
        return Ok(json.clone());
    }
    let json = serde_json::to_string(&overview_data(app, now)?)?;
    *cached = Some((now, json.clone()));
    Ok(json)
}

async fn overview_json(State(app): State<Arc<App>>) -> Result<Response, Fail> {
    let json = tokio::task::spawn_blocking(move || overview_cached(&app, now_ms())).await??;
    Ok(([(header::CONTENT_TYPE, "application/json")], json).into_response())
}

fn drops_html(cfg: &Config, drops: &[crate::detect::Drop]) -> String {
    let mut b = String::from("<ul class=plain>");
    for d in drops {
        let _ = write!(
            b,
            "<li><a href=\"{}&amp;days=2&amp;end={}\">{}</a>: down {:.1} dB at {}, {}</li>",
            link_href(&d.a, &d.b),
            d.start + DAY,
            link_name(cfg, &d.a, &d.b),
            d.depth,
            esc(&short_time(d.start)),
            match d.end {
                Some(e) => format!("back after {}", human_duration(e - d.start)),
                None => format!("no recovery seen (last low at {})", short_time(d.last_low)),
            }
        );
    }
    b.push_str("</ul>");
    b
}

fn evidence_html(app: &App, c: &Candidate) -> anyhow::Result<String> {
    let cfg = &app.cfg;
    let mut b = String::new();
    match c {
        Candidate::Wet(w) => {
            b.push_str(&drops_html(cfg, &w.drops));
            let _ = write!(
                b,
                "<p class=small>{} of the site's {} links with data dropped; {} of the mesh's \
                 {} other links dropped at the same time.</p>",
                w.drops.len(),
                w.site_links,
                w.others_dropping,
                w.others
            );
        }
        Candidate::Front(f) => {
            b.push_str(&drops_html(cfg, &f.drops));
            let _ = write!(
                b,
                "<p class=small>{} of the {} links with data dropped at once, at {}: too \
                 widespread for one site's antennas. A rain front wetting several sites looks \
                 like this, and so does anything that hits every link's measurements (a core \
                 or crawl problem, a firmware change).</p>",
                f.drops.len(),
                f.with_data,
                esc(&f
                    .sites
                    .iter()
                    .map(|s| cfg.label(s))
                    .collect::<Vec<_>>()
                    .join(", "))
            );
        }
        Candidate::Duct(d) => {
            let _ = write!(
                b,
                "<p class=small>At the peak ({}), {} of {} links with data were {:.0} dB or more \
                 from their usual level: {} above, {} below. Largest departure {:.1} dB. Links: ",
                esc(&when(cfg, d.peak_at)),
                d.peak_departing,
                d.peak_with_data,
                cfg.duct.departure_db,
                d.peak_up,
                d.peak_down,
                d.max_abs
            );
            let names: Vec<String> = d
                .links
                .iter()
                .map(|(x, y)| {
                    format!(
                        "<a href=\"{}&amp;days=2&amp;end={}\">{}</a>",
                        link_href(x, y),
                        d.last + DAY,
                        link_name(cfg, x, y)
                    )
                })
                .collect();
            let _ = write!(b, "{}.</p>", names.join(", "));
        }
    }
    let ev = evidence::gather(&app.db(), cfg, c)?;
    for l in ev.lines(cfg) {
        let _ = write!(b, "<p class=small>{}</p>", esc(&l));
    }
    Ok(b)
}

async fn events_page(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<String>, Fail> {
    let here = app.here();
    let kind = match q.get("kind").map(String::as_str) {
        Some("wet") => Some("wet"),
        Some("duct") => Some("duct"),
        Some("front") => Some("front"),
        _ => None,
    };
    let a = app.clone();
    let b = tokio::task::spawn_blocking(move || events_body(&a, kind)).await??;
    Ok(page(&here, "/events", "Events", &b))
}

fn events_body(app: &App, kind: Option<&str>) -> anyhow::Result<String> {
    let cfg = &app.cfg;
    let list = events::list(&app.db(), 0, now_ms() + 1, kind)?;
    let mut b = String::from(
        "<h1>Candidate events</h1><p>Each one matched a rule on the <a href=\"/methods\">methods</a> \
         page. They are leads to look at, not measurements: equipment, wind and interference \
         can cause the same shapes.</p><p class=noprint>",
    );
    for (k, label) in [
        (None, "All"),
        (Some("wet"), "Wet antenna"),
        (Some("duct"), "Ducting or fading"),
        (Some("front"), "Widespread drop"),
    ] {
        let href = k.map_or("/events".to_string(), |k| format!("/events?kind={k}"));
        if k == kind {
            let _ = write!(b, "<b>{label}</b> &middot; ");
        } else {
            let _ = write!(b, "<a href=\"{href}\">{label}</a> &middot; ");
        }
    }
    b.push_str("</p>");
    if list.is_empty() {
        b.push_str(
            "<p class=mute>No candidates yet. Detection starts once links have a baseline \
             (see the front page), and runs every 10 minutes.</p>",
        );
    }
    for c in &list {
        let _ = write!(
            b,
            "<h2>{} {}</h2><p class=small>{} to {}</p>{}",
            candidate_pill(c),
            esc(&candidate_title(cfg, c)),
            esc(&when(cfg, c.start())),
            esc(&c
                .end()
                .map(|e| when(cfg, e))
                .unwrap_or_else(|| "still going".into())),
            evidence_html(app, c)?
        );
    }
    Ok(b)
}

async fn now_json(State(app): State<Arc<App>>) -> Result<axum::Json<Now>, Fail> {
    let n = tokio::task::spawn_blocking(move || {
        let (db, now) = (app.db(), now_ms());
        now::current(&db, &app.cfg, now, &recent_history(&db, &app.cfg, now)?)
    })
    .await??;
    Ok(axum::Json(n))
}

async fn results_page(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<String>, Fail> {
    let here = app.here();
    let days: i64 = match q.get("days").map(String::as_str) {
        Some("30") => 30,
        Some("365") => 365,
        _ => 90,
    };
    let a = app.clone();
    let b = tokio::task::spawn_blocking(move || results_cached(&a, days)).await??;
    Ok(page(&here, "/results", "Results", &b))
}

/// The results for `days`, worked out at most every 10 minutes and one at a time: a year
/// of history takes seconds and memory a Pi doesn't have to spare.
fn results_cached(app: &App, days: i64) -> anyhow::Result<String> {
    static DONE: Mutex<BTreeMap<i64, (i64, String)>> = Mutex::new(BTreeMap::new());
    // held while working, so a second request waits for the first instead of redoing it
    let mut done = DONE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, body)) = done.get(&days)
        && now_ms() - at < 10 * MIN
    {
        return Ok(body.clone());
    }
    let at = now_ms();
    let body = results_body(app, days)?;
    done.insert(days, (at, body.clone()));
    Ok(body)
}

fn cmp_text(c: &Option<results::Comparison>, unit: &str) -> String {
    match c {
        None => "not enough data (needs at least 3 hours in each group)".into(),
        Some(c) => format!(
            "{:+.2} {unit} (95% interval {:+.2} to {:+.2}; {} against {} hours): {}",
            c.diff,
            c.diff - c.ci95,
            c.diff + c.ci95,
            c.n_a,
            c.n_b,
            if c.clear() { "clear" } else { "not clear" }
        ),
    }
}

fn results_body(app: &App, days: i64) -> anyhow::Result<String> {
    let cfg = &app.cfg;
    let now = now_ms();
    // its own connection: a year of history takes seconds, and the feed shouldn't wait
    let conn = crate::open(cfg)?;
    let r = results::compute(&conn, cfg, now - days * DAY, now)?;
    let mut b = String::from(
        "<h1>Results</h1><p>The project's test: a statistically clear effect on at least one \
         link class, or a well-documented negative result. Each line compares two groups of \
         hourly residuals; the effect is called clear when its 95% interval leaves out zero. \
         Neighbouring hours aren't independent, so the intervals are too narrow, and with \
         many links one will look clear by chance now and then (about one link in twenty \
         with no real effect): \"clear\" means worth a closer look, not proof.</p><p class=noprint>",
    );
    for (d, label) in [(30, "30 days"), (90, "90 days"), (365, "1 year")] {
        if d == days {
            let _ = write!(b, "<b>{label}</b> &middot; ");
        } else {
            let _ = write!(b, "<a href=\"/results?days={d}\">{label}</a> &middot; ");
        }
    }
    let _ = write!(
        b,
        "</p><p class=\"mute small\">{} links with a baseline from {} to {}.</p>",
        r.links,
        esc(&short_time(r.from)),
        esc(&short_time(r.to))
    );
    b.push_str("<h2>Rain at the site: wet antennas</h2>");
    if r.rain.is_empty() {
        b.push_str(
            "<p>No link has a weather station set at either end, so this can't be tested. \
             Add <code>aprs</code> or <code>nws</code> stations to the <code>[[site]]</code> \
             entries.</p>",
        );
    } else {
        let _ = write!(
            b,
            "<p>All links together, rain hours minus dry hours: {}.</p>\
             <div class=wrap><table><tr><th>Link</th><th>Stations</th><th>Rain minus dry</th></tr>",
            esc(&cmp_text(&r.rain_all, "dB"))
        );
        for x in &r.rain {
            let _ = write!(
                b,
                "<tr><td><a href=\"{}\">{}</a></td><td>{}</td><td>{}</td></tr>",
                link_href(&x.pair.0, &x.pair.1),
                link_name(cfg, &x.pair.0, &x.pair.1),
                esc(&x.stations.join(", ")),
                esc(&cmp_text(&x.cmp, "dB"))
            );
        }
        b.push_str(
            "</table></div><p class=\"mute small\">A wet antenna shows as a negative \
             difference: the link a few dB down while it rains.</p>",
        );
    }
    let _ = write!(
        b,
        "<h2>Night against day: ducting and fading</h2><p>Size of departures from the usual \
         level, night (20 to 06 local) minus day (08 to 18 local), all links: {}.</p>\
         <div class=wrap><table><tr><th>Local hour</th><th>Mean departure</th><th>Link-hours</th></tr>",
        esc(&cmp_text(&r.night, "dB"))
    );
    for (h, m, n) in &r.by_hour {
        let _ = write!(
            b,
            "<tr><td>{h:02}:00</td><td>{}</td><td>{n}</td></tr>",
            if *n > 0 {
                format!("{m:.2} dB")
            } else {
                "&ndash;".into()
            }
        );
    }
    b.push_str("</table></div>");
    let tally = events::list(&conn, r.from, r.to, None)?;
    let (mut rain_n, mut dry_n, mut none_n) = (0, 0, 0);
    for c in tally.iter().filter(|c| matches!(c, Candidate::Wet(_))) {
        match evidence::gather(&conn, cfg, c)?.rain {
            Some(crate::rain::Verdict::Rain(_)) => rain_n += 1,
            Some(crate::rain::Verdict::Dry(_)) => dry_n += 1,
            _ => none_n += 1,
        }
    }
    let count = |k: &str| tally.iter().filter(|c| c.kind() == k).count();
    let _ = write!(
        b,
        "<h2>Candidates</h2><p>{} wet-antenna candidates: {rain_n} with rain reported nearby, \
         {dry_n} with dry reports, {none_n} that couldn't be checked. {} widespread drops. \
         {} ducting or fading candidates.</p>",
        count("wet"),
        count("front"),
        count("duct")
    );
    Ok(b)
}

async fn events_json(State(app): State<Arc<App>>) -> Result<axum::Json<serde_json::Value>, Fail> {
    let list = events::list(&app.db(), 0, now_ms() + 1, None)?;
    Ok(axum::Json(
        json!({"generated": now_ms(), "candidates": list}),
    ))
}

fn markers(
    cfg: &Config,
    reports: &[Report],
    soundings: &[Sounding],
    cands: &[Candidate],
) -> Vec<(i64, String)> {
    let max_h = cfg.sounding_height();
    let mut m: Vec<(i64, String)> = reports
        .iter()
        .filter(|r| r.wet() == Some(true))
        .map(|r| {
            let radar = if r.source == crate::radar::SOURCE {
                "radar "
            } else {
                ""
            };
            let at = cfg.label(&r.station);
            (r.ts, format!("{radar}rain at {at}: {}", report_text(r)))
        })
        .collect();
    m.extend(soundings.iter().map(|s| {
        (
            s.ts,
            format!("sounding {}: {}", s.station, s.summary(max_h)),
        )
    }));
    m.extend(cands.iter().map(|c| {
        let what = match c {
            Candidate::Wet(w) => format!("wet-antenna candidate at {}", cfg.label(&w.site)),
            Candidate::Duct(_) => "ducting or fading candidate".into(),
            Candidate::Front(f) => format!("widespread drop on {} links", f.drops.len()),
        };
        (c.start(), what)
    }));
    m.sort_by_key(|x| x.0);
    m
}

struct LinkData {
    samples: Vec<Vec<series::Sample>>,
    /// each end's history for its baseline, in the order of `samples`
    history: Vec<series::History>,
    reports: Vec<Report>,
    soundings: Vec<Sounding>,
    cands: Vec<Candidate>,
}

/// The link and period a `/link` or `/api/link` query asks for, and the bin width that
/// period is drawn in.
struct LinkQuery {
    a: String,
    c: String,
    days: i64,
    from: i64,
    to: i64,
    width: i64,
}

fn link_query(cfg: &Config, q: &HashMap<String, String>, now: i64) -> LinkQuery {
    let (a, c) = edge(
        &norm_node(q.get("a").map_or("", |s| s)),
        &norm_node(q.get("b").map_or("", |s| s)),
    );
    let days: i64 = match q.get("days").map(String::as_str) {
        Some("2") => 2,
        Some("30") => 30,
        _ => 7,
    };
    let to = q
        .get("end")
        .and_then(|e| e.parse::<i64>().ok())
        .map_or(now, |e| e.clamp(0, now));
    LinkQuery {
        a,
        c,
        days,
        from: to - days * DAY,
        to,
        width: if days == 30 {
            HOUR
        } else {
            cfg.baseline.bin_min as i64 * MIN
        },
    }
}

/// One direction of a link, binned: what the link page draws and `/api/link` returns.
struct Direction {
    end: End,
    level: Vec<(i64, f64)>,
    baseline: Vec<(i64, f64)>,
    residual: Vec<(i64, f64)>,
    noise: Vec<(i64, f64)>,
}

fn directions(cfg: &Config, ends: &[End; 2], data: &LinkData, width: i64) -> Vec<Direction> {
    ends.iter()
        .zip(&data.samples)
        .zip(&data.history)
        .map(|((e, s), h)| {
            let wb = series::with_baseline(s, h, &cfg.baseline);
            let bin = |pts: Vec<(i64, f64)>| series::bin(&pts, width);
            Direction {
                end: e.clone(),
                level: bin(s.iter().map(|x| (x.ts, x.signal)).collect()),
                baseline: bin(wb.iter().map(|x| (x.0, x.2)).collect()),
                residual: bin(wb.iter().map(|x| (x.0, x.1 - x.2)).collect()),
                noise: bin(s.iter().filter_map(|x| Some((x.ts, x.noise?))).collect()),
            }
        })
        .collect()
}

fn load_link(
    conn: &Connection,
    cfg: &Config,
    ends: &[End; 2],
    from: i64,
    to: i64,
) -> anyhow::Result<LinkData> {
    let (a, c) = (&ends[0].0, &ends[0].1);
    let span = from - cfg.baseline.days as i64 * DAY;
    Ok(LinkData {
        samples: ends
            .iter()
            .map(|e| series::samples(conn, e, from, to + 1))
            .collect::<anyhow::Result<_>>()?,
        history: series::history_of(conn, cfg, ends, span, to + 1)?,
        reports: rain::reports(
            conn,
            &cfg.stations_near(&[a.clone(), c.clone()]),
            from,
            to + 1,
        )?,
        soundings: sounding::between(conn, from, to + 1)?,
        cands: events::list(conn, from, to + 1, None)?
            .into_iter()
            .filter(|x| x.involves(a, c))
            .collect(),
    })
}

async fn link_page(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, Fail> {
    let here = app.here();
    let cfg = &app.cfg;
    let now = now_ms();
    let LinkQuery {
        a,
        c,
        days,
        from,
        to,
        width,
    } = link_query(cfg, &q, now);
    let ends: [End; 2] = [(a.clone(), c.clone()), (c.clone(), a.clone())];
    let data = {
        let (app, ends) = (app.clone(), ends.clone());
        tokio::task::spawn_blocking(move || load_link(&app.db(), &app.cfg, &ends, from, to))
            .await??
    };
    if data.samples.iter().all(Vec::is_empty) && to == now && days == 7 {
        let body = "<h1>Not found</h1><p>No RF samples for a link between those sites in the \
                    last week. <a href=\"/\">All links</a></p>";
        return Ok((StatusCode::NOT_FOUND, page(&here, "", "Not found", body)).into_response());
    }
    let mut marks = markers(cfg, &data.reports, &data.soundings, &data.cands);
    marks.retain(|m| (from..=to).contains(&m.0));
    let dirs = directions(cfg, &ends, &data, width);
    let mut b = String::new();
    let _ = write!(b, "<h1>{}</h1><p class=noprint>", link_name(cfg, &a, &c));
    for (d, label) in [(2, "2 days"), (7, "7 days"), (30, "30 days")] {
        if d == days {
            let _ = write!(b, "<b>{label}</b> &middot; ");
        } else {
            let _ = write!(
                b,
                "<a href=\"{}&amp;days={d}&amp;end={to}\">{label}</a> &middot; ",
                link_href(&a, &c)
            );
        }
    }
    let _ = write!(
        b,
        "<a href=\"{}&amp;days={days}&amp;end={}\">earlier</a>",
        link_href(&a, &c),
        to - days * DAY
    );
    if to < now {
        let _ = write!(
            b,
            " &middot; <a href=\"{}&amp;days={days}\">now</a>",
            link_href(&a, &c)
        );
    }
    let _ = write!(
        b,
        " &middot; <a href=\"/api/link?a={}&amp;b={}&amp;days={days}&amp;end={to}\">as JSON</a></p>",
        enc(&a),
        enc(&c)
    );

    let label = |e: &End| format!("{} hears {}", cfg.label(&e.0), cfg.label(&e.1));
    let resid: Vec<Line> = dirs
        .iter()
        .filter(|d| !d.residual.is_empty())
        .map(|d| Line {
            label: label(&d.end),
            points: d.residual.clone(),
        })
        .collect();
    let chart = |lines: Vec<Line>, unit: &str, rule: Option<(f64, String)>| Chart {
        from,
        to,
        unit: unit.into(),
        lines,
        markers: marks.clone(),
        rule,
        gap_ms: 3 * width,
    };
    b.push_str("<h2>Residual</h2>");
    if resid.is_empty() {
        let have = data
            .history
            .iter()
            .map(|h| series::baseline_days(h, series::day_of(to), &cfg.baseline))
            .max()
            .unwrap_or(0);
        let _ = write!(
            b,
            "<p>No baseline for this period yet: every hour of day needs {} days of history \
             before a day, and this link has {have} at its least covered hour.</p>",
            cfg.baseline.min_days
        );
    } else {
        b.push_str(
            "<p class=\"mute small\">Received level minus the usual for that hour of day. The \
             dashed line is zero; vertical lines mark rain reports, soundings and candidates \
             (listed below).</p>",
        );
        b.push_str(
            &chart(resid, "dB", Some((0.0, "the usual for that hour".into())))
                .render("Residual received level"),
        );
    }
    for d in dirs.iter().filter(|d| !d.level.is_empty()) {
        let mut lines = vec![Line {
            label: "level".into(),
            points: d.level.clone(),
        }];
        if !d.baseline.is_empty() {
            lines.push(Line {
                label: "baseline".into(),
                points: d.baseline.clone(),
            });
        }
        let _ = write!(b, "<h2>{}</h2>", esc(&label(&d.end)));
        b.push_str(
            &chart(lines, "dBm", None).render(&format!("Received level, {}", label(&d.end))),
        );
    }
    let noise: Vec<Line> = dirs
        .iter()
        .filter(|d| !d.noise.is_empty())
        .map(|d| Line {
            label: label(&d.end),
            points: d.noise.clone(),
        })
        .collect();
    if !noise.is_empty() {
        b.push_str(
            "<h2>Noise floor</h2><p class=\"mute small\">A rise here with a fall in SNR is \
             interference, not weather.</p>",
        );
        b.push_str(&chart(noise, "dBm", None).render("Noise floor"));
    }
    if !marks.is_empty() {
        b.push_str("<h2>Marked on the charts</h2><ul class=plain>");
        for (t, text) in &marks {
            let _ = write!(
                b,
                "<li><span class=\"mute small\">{}</span> {}</li>",
                esc(&when(cfg, *t)),
                esc(text)
            );
        }
        b.push_str("</ul>");
    }
    Ok(page(
        &here,
        "",
        &format!("{} - {}", cfg.label(&a), cfg.label(&c)),
        &b,
    )
    .into_response())
}

fn points(pts: &[(i64, f64)]) -> Vec<Value> {
    pts.iter()
        .map(|(t, v)| json!([t, evidence::round(*v, 2)]))
        .collect()
}

/// One link's data as JSON: what its page draws, the hours it rained or was dry near
/// either end, and the candidates it was part of.
async fn link_json(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, Fail> {
    let LinkQuery {
        a,
        c,
        from,
        to,
        width,
        ..
    } = link_query(&app.cfg, &q, now_ms());
    let ends: [End; 2] = [(a.clone(), c.clone()), (c.clone(), a.clone())];
    let stations = app.cfg.stations_near(&[a.clone(), c.clone()]);
    let (data, wet, dry) = {
        let (app, ends, stations) = (app.clone(), ends.clone(), stations.clone());
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let db = app.db();
            let data = load_link(&db, &app.cfg, &ends, from, to)?;
            let after = app.cfg.baseline.dry_after_h;
            let (wet, dry) = rain::wet_and_dry(&db, &stations, from, to + 1, after)?;
            Ok((data, wet, dry))
        })
        .await??
    };
    if data.samples.iter().all(Vec::is_empty) {
        let why = format!("no RF samples for a link between {a:?} and {c:?} in that period");
        return Ok((StatusCode::NOT_FOUND, axum::Json(json!({"error": why}))).into_response());
    }
    let ends: Vec<Value> = directions(&app.cfg, &ends, &data, width)
        .iter()
        .map(|d| {
            json!({"node": d.end.0, "hears": d.end.1, "level_dbm": points(&d.level),
                   "baseline_dbm": points(&d.baseline), "residual_db": points(&d.residual),
                   "noise_dbm": points(&d.noise)})
        })
        .collect();
    let rain: BTreeMap<i64, bool> = wet
        .into_iter()
        .map(|h| (h, true))
        .chain(dry.into_iter().map(|h| (h, false)))
        .collect();
    let rain: Vec<Value> = rain
        .into_iter()
        .map(|(h, wet)| json!({"hour_ms": h, "wet": wet}))
        .collect();
    Ok(axum::Json(json!({
        "a": a, "b": c, "from_ms": from, "to_ms": to, "bin_ms": width, "ends": ends,
        "rain_stations": stations, "rain_hours": rain, "candidates": data.cands,
    }))
    .into_response())
}

/// The links as a GeoJSON map layer: a line per link between its nodes' reported
/// positions, with its length, whether it is new and studied, its residual if fresh and
/// the kinds of candidate it was part of in the last day; and a point per node.
fn layer(conn: &Connection, cfg: &Config, now: i64) -> anyhow::Result<Value> {
    let ends = end_status(conn, cfg, now, &recent_history(conn, cfg, now)?)?;
    let seen = links::seen(conn, cfg, now)?;
    let nodes: BTreeSet<&String> = seen.iter().flat_map(|l| [&l.pair.0, &l.pair.1]).collect();
    let pos = evidence::positions(conn, nodes)?;
    let recent = events::list(conn, now - DAY, now + 1, None)?;
    let mut features = vec![];
    for l in &seen {
        let (a, b) = (&l.pair.0, &l.pair.1);
        let (Some(pa), Some(pb)) = (pos.get(a), pos.get(b)) else {
            continue;
        };
        // a residual from a link that has gone quiet says nothing about now
        let fresh: Vec<f64> = ends
            .iter()
            .filter(|e| edge(&e.end.0, &e.end.1) == l.pair && now - e.last <= 30 * MIN)
            .filter_map(|e| e.residual)
            .collect();
        let residual = (!fresh.is_empty())
            .then(|| evidence::round(fresh.iter().sum::<f64>() / fresh.len() as f64, 2));
        let kinds: BTreeSet<&str> = recent
            .iter()
            .filter(|c| c.involves(a, b))
            .map(Candidate::kind)
            .collect();
        let km = distance_km(*pa, *pb);
        features.push(json!({
            "type": "Feature", "id": format!("{a}|{b}"),
            "geometry": {"type": "LineString", "coordinates": [[pa.lon, pa.lat], [pb.lon, pb.lat]]},
            "properties": {
                "a": a, "b": b, "a_label": cfg.label(a), "b_label": cfg.label(b),
                "length_km": evidence::round(km, 2), "first_ms": l.first, "last_ms": l.last,
                "new": l.new, "long": km >= cfg.long_km, "studied": l.studied,
                "residual_db": residual, "candidates_24h": kinds,
            },
        }));
    }
    for (n, p) in &pos {
        features.push(json!({
            "type": "Feature", "id": n,
            "geometry": {"type": "Point", "coordinates": [p.lon, p.lat]},
            "properties": {"node": n, "label": cfg.label(n)},
        }));
    }
    Ok(json!({"type": "FeatureCollection", "generated_ms": now, "features": features}))
}

async fn links_geojson(State(app): State<Arc<App>>) -> Result<Response, Fail> {
    let a = app.clone();
    let v = tokio::task::spawn_blocking(move || layer(&a.db(), &a.cfg, now_ms())).await??;
    Ok((
        [(header::CONTENT_TYPE, "application/geo+json")],
        v.to_string(),
    )
        .into_response())
}

/// `x` to two significant digits: rain loss runs from thousandths of a dB to whole dB.
fn sig2(x: f64) -> String {
    let decimals = if x > 0.0 {
        (1 - x.log10().floor() as i32).max(0) as usize
    } else {
        1
    };
    format!("{x:.decimals$}")
}

async fn methods_page(State(app): State<Arc<App>>) -> Html<String> {
    let here = app.here();
    let c = &app.cfg;
    let (w, d, bl) = (&c.wet, &c.duct, &c.baseline);
    let set = c
        .export
        .frequency_mhz
        .or_else(|| c.sites.iter().find_map(|s| s.frequency_mhz));
    // with no channel set, the middle of the 5650-5925 MHz ham band AREDN uses
    let mhz = set.unwrap_or(5800.0);
    let loss = if mhz >= 1000.0 {
        let (h, v) = crate::p838::db_per_km(mhz, 10.0);
        format!(
            "At {mhz:.0} MHz{}, ITU-R P.838-3 puts it at {} dB per km in heavy rain (10 mm/h) \
             with horizontal polarisation and {} with vertical, so a 30 km backbone path loses \
             {} to {} dB while it lasts.",
            if set.is_some() {
                " (the channel set in the config)"
            } else {
                " (no channel is set in the config, so the middle of the band)"
            },
            sig2(h),
            sig2(v),
            sig2(30.0 * h.min(v)),
            sig2(30.0 * h.max(v))
        )
    } else {
        format!(
            "The channel set in the config, {mhz:.0} MHz, is below 1 GHz, where ITU-R \
             P.838-3's fits begin; rain costs even less there."
        )
    };
    let mut b = String::from(
        "<h1>Methods</h1>\
         <p class=banner>Experimental. The rules below find candidates worth a look. None of \
         them measures rain, and a negative result is a result.</p>\
         <h2>What 5 GHz links can and can't see</h2>",
    );
    let _ = write!(
        b,
        "<p>Rain falling across the path costs very little at the 5.7 to 5.9 GHz channels \
         AREDN uses. {loss} The nodes report signal in whole dB and it wanders by a dB or two \
         anyway. That is far too small to map rainfall, which is why the commercial-link rain \
         work (OpenSense) uses links at 10 to 40 GHz and above.</p>"
    );
    b.push_str(
        "<p>Two things are big enough to see:</p>\
         <ul class=plain>\
         <li><b>Wet antennas.</b> A film of water on a radome or dish costs a few dB, on every \
         link at that site and in both directions, and goes away as it drains or dries over \
         hours. Studies of 5 GHz links in rain found this was the effect they could see. It \
         says it is raining (or misty, or dewy) at a site, not how hard.</li>\
         <li><b>Ducting and fading.</b> Along the coast a warm, dry layer often sits on top of \
         the cool, moist marine layer. Refractivity falls sharply across that inversion and \
         can bend 5 GHz signals back down (a duct, when it falls faster than 157 N-units per \
         km), raising some links and fading others by many dB, often in the evening and at \
         night. Soundings show the inversion's height and strength.</li></ul>\
         <p>Plenty else also moves link levels: the radios' temperature, sun on the dish, \
         wind moving a mount, rate changes, and interference. A daily pattern from any of \
         these is taken out by the baseline; the rest is why everything here is a candidate.</p>",
    );
    let _ = write!(
        b,
        "<h2>The baseline and the residual</h2>\
         <p>The level studied is the received signal each node reports for each RF neighbor \
         (or SNR plus noise floor where that is all it reports). Every hour is summarized as \
         a median. For each day, the baseline at each hour of day (UTC) is the median of that \
         hour over the previous {} days, once at least {} of them have data, leaving out the \
         hours it rained at either end of the link and the {} hours after while the antennas \
         dry: the baseline is the link's dry level. Rain comes from the stations and radar set \
         for the two sites; a link with neither has every hour in its baseline. An hour of day \
         it rained at on so many days that fewer than {} dry ones are left takes every day \
         instead, wet ones too. The window rolls, so the baseline follows the season, and a \
         pattern that lasts for weeks, such as a summer of nightly ducting, becomes the usual \
         level and leaves the residual. The residual is each sample minus its baseline, taken \
         as {}-minute medians, and the two ends of a link are averaged.</p>",
        bl.days, bl.min_days, bl.dry_after_h, bl.min_days, bl.bin_min
    );
    let _ = write!(
        b,
        "<h2>Wet-antenna rule</h2><p>A link drops when its residual falls at least {} dB below \
         where it was (the median over the {} minutes ending {} minutes earlier, or the usual \
         level if it was above that, so coming down from a ducting boost doesn't count), stays \
         more than {} dB below that for at least {} minutes, comes back within {} hours, and never \
         falls more than {} dB (bigger drops are outages). A site is a candidate when at least \
         {} of its links, and at least {:.0}% of its links with data, drop within {} minutes of \
         each other, while no more than {:.0}% of the mesh's other links drop at the same \
         time (more than that is mesh-wide, not one site). Each candidate is then checked \
         against the site's rain stations from {} hours before the drop to an hour after it \
         ends.</p>",
        w.drop_db,
        w.pre_min,
        w.onset_min,
        w.recover_db,
        w.hold_min,
        w.max_recover_h,
        w.max_drop_db,
        w.min_links,
        w.min_frac * 100.0,
        w.onset_min,
        w.max_other_frac * 100.0,
        w.rain_before_h
    );
    let _ = write!(
        b,
        "<h2>Widespread-drop rule</h2><p>When a site's links drop together but more than \
         {:.0}% of the mesh's other links drop within {} minutes as well, the drop is too \
         widespread to be one site's antennas. All those drops together are listed as their \
         own kind of candidate, merged with any that share a drop, and a wet-antenna \
         candidate whose drops all lie inside one gives way to it. A rain front wetting \
         several sites looks like this, and so does anything that hits every link's \
         measurements at once, such as a core or crawl problem. It is checked against the \
         rain stations and lightning near every site involved.</p>",
        w.max_other_frac * 100.0,
        w.onset_min
    );
    let _ = write!(
        b,
        "<h2>Wind</h2><p>A strong gust can move a dish or its mount and swing a link's level \
         much as weather in the path would. Every candidate is checked against the strongest \
         gust the APRS weather stations near its sites reported, from an hour before it to an \
         hour after it ends. A gust of {:.0} mph or more flags it: shown on the candidate, \
         and <code>windy: true</code> in the published event. The candidate stays, since \
         rain and wind often come together, but read it as possibly wind. The marine-layer \
         indicator drops to medium confidence when the links are off their usual level and \
         a station reported such a gust in the last hour.</p>",
        c.gust_mph
    );
    let _ = write!(
        b,
        "<p>Lightning strikes from ridgewarden-lightning within {:.0} km of a site, in the \
         same window as the rain check, count as supporting evidence: a storm overhead is \
         rain nearby even where no gauge reports it. Strikes with two possible positions are \
         left out.</p>",
        w.strike_km
    );
    let _ = write!(
        b,
        "<h2>Ducting and fading rule</h2><p>A {}-minute bin departs when at least {} links \
         have data, and at least {} of them, and at least {:.0}% of them, are {} dB or more \
         from their baseline, either way. Departing bins no more than two bins apart form an \
         episode, and an episode lasting at least {} minutes is a candidate. Each is shown \
         with the nearest sounding from every station: its inversion and steepest \
         refractivity gradient (ITU-R P.453 refractivity from pressure, temperature and dew \
         point).</p>",
        bl.bin_min,
        d.min_with_data,
        d.min_links,
        d.min_frac * 100.0,
        d.departure_db,
        d.min_duration_min
    );
    b.push_str(
        "<h2>The marine-layer indicator</h2><p>The front page judges the present from the \
         newest complete residual bin (ended no more than 30 minutes ago) and the latest \
         sounding. Calm when no link is far from its usual level, disturbed when that bin \
         meets the ducting rule, ducting or fading when the last three bins (at least half \
         an hour) all have. Many links down together with none up is called a widespread \
         drop instead. The sounding adds the height of the inversion over the marine layer. \
         Confidence starts high and drops to medium when the sounding is over 12 hours old, \
         missing, or disagrees with the links (disturbed links but no low inversion or \
         trapping layer), and to low when fewer links report than the ducting rule needs. \
         With no fresh residuals it says it can't tell.</p>",
    );
    b.push_str(
        "<h2>Published candidates</h2><p>Candidates go out to the mesh as signed \
         <code>weather.candidate</code> events in the core's main log, when first found, \
         again when they end, and once more if a later scan withdraws them, so any app can \
         allow for weather from its own core. (Link Doctor reads the same candidates from \
         <code>/api/events</code> today.) The rain readings fetched from APRS-IS, \
         NWS and radar go out as <code>weather.rain</code> telemetry. Both shapes are in the \
         README.</p>",
    );
    let _ = write!(
        b,
        "<h2>New links</h2><p>A link first heard at least a day after the app's first data is \
         new for two weeks, and listed on the front page. One {:.1} km \
         or longer is flagged as long: long paths near the coast are the ones the marine \
         layer bends, so they are the ones worth studying. With <code>links</code> set in the \
         config, a new link isn't studied until it is added there.</p>",
        c.long_km
    );
    b.push_str(
        "<h2>Sources</h2><ul class=plain>\
         <li>Link levels: the ridgewarden core's node polls. For minute data on the backbone, \
         one hub core crawls the backbone nodes every 60 seconds, agreed with their owners.</li>\
         <li>Rain: APRS weather stations heard over APRS-IS (most are CWOP members; the rain \
         in the last hour), NWS observations from api.weather.gov (precipitation in the \
         last hour and the observer's description), and the NWS MRMS radar-only estimate of \
         the last hour at each site's own position, on a 1 km grid in steps of 0.1 in \
         (2.5 mm), from NOAA's map service. Gauges sit in towns and valleys; radar is the one \
         source for the peak itself, though it misses rain below the beam. Its 0 can still be \
         light rain or drizzle, so radar marks rain hours and only gauges mark dry ones.</li>\
         <li>Soundings: the University of Wyoming upper-air archive.</li></ul>\
         <p>All thresholds are in the config file, so a result can be rerun with different \
         ones.</p>",
    );
    page(&here, "/methods", "Methods", &b)
}

async fn export_page(State(app): State<Arc<App>>) -> Html<String> {
    let here = app.here();
    let mut b = String::from(
        "<h1>Dataset</h1><p>Download the link levels in the OpenSense conventions for \
         commercial microwave link data, as a NetCDF classic file: one CML per link, two \
         sublinks (one per direction), received signal level in dBm on a regular time axis, \
         plus the noise floor and SNR the nodes report. Site positions come from the nodes' \
         own reports; frequency and channel width from the config. There is no transmit \
         level: the core's polls don't carry it.</p>",
    );
    let _ = write!(
        b,
        "<form method=get action=\"/export.nc\"><p><label>Days back \
         <select name=days><option>1</option><option selected>7</option><option>30</option>\
         <option>90</option></select></label> \
         <label>Time step <select name=step><option value=60 selected>1 minute</option>\
         <option value=300>5 minutes</option><option value=900>15 minutes</option>\
         <option value=3600>1 hour</option></select></label> \
         <button type=submit>Download .nc</button></p></form>\
         <p class=\"mute small\">Raw samples are kept {} days. Check a file with \
         <code>ncdump -h file.nc</code>, or open it with xarray: \
         <code>xarray.open_dataset(\"file.nc\")</code>.</p>",
        app.cfg.keep_days
    );
    page(&here, "/export", "Dataset", &b)
}

async fn export_nc(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, Fail> {
    let days: i64 = q.get("days").and_then(|d| d.parse().ok()).unwrap_or(7);
    let step: i64 = q.get("step").and_then(|d| d.parse().ok()).unwrap_or(60);
    if !(1..=app.cfg.keep_days as i64).contains(&days) || !(60..=3600).contains(&step) {
        return Ok((
            StatusCode::BAD_REQUEST,
            format!(
                "days must be 1 to {} and step 60 to 3600 seconds",
                app.cfg.keep_days
            ),
        )
            .into_response());
    }
    let Some(busy) = Busy::take() else {
        return Ok((
            StatusCode::TOO_MANY_REQUESTS,
            "another export is running; try again in a minute",
        )
            .into_response());
    };
    let cfg = app.cfg.clone();
    let now = now_ms();
    let from = now - days * DAY;
    let built = tokio::task::spawn_blocking(move || {
        // held until the file is built, even if the client has gone
        let _busy = busy;
        let conn = crate::open(&cfg)?;
        crate::export::build(&conn, &cfg, from, now, step)?.write()
    })
    .await?;
    match built {
        Ok(bytes) => {
            let name = format!(
                "ridgewarden-weather-{}.nc",
                ms_to_rfc3339(now)[..10].replace('-', "")
            );
            Ok((
                [
                    (header::CONTENT_TYPE, "application/x-netcdf".to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{name}\""),
                    ),
                ],
                bytes,
            )
                .into_response())
        }
        Err(e) => Ok((StatusCode::BAD_REQUEST, format!("{e:#}")).into_response()),
    }
}

/// One export at a time: each holds the whole file in memory.
static EXPORTING: AtomicBool = AtomicBool::new(false);

struct Busy;

impl Busy {
    fn take() -> Option<Busy> {
        EXPORTING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Busy)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        EXPORTING.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ridgewarden_client::core::Projection;
    use ridgewarden_client::mesh::Mesh;
    use ridgewarden_client::mesh::testing::Fake;
    use rusqlite::params;

    const T0: i64 = 1_795_000_000_000 - 1_795_000_000_000 % DAY;

    fn fake() -> Fake {
        let f = Fake::new(vec![
            Box::new(Mesh) as Box<dyn Projection>,
            Box::new(crate::strikes::Strikes),
        ]);
        for s in [
            series::SCHEMA,
            rain::SCHEMA,
            sounding::SCHEMA,
            events::SCHEMA,
        ] {
            f.conn.execute_batch(s).unwrap();
        }
        f
    }

    fn poll(f: &mut Fake, node: &str, heard: &str, at: i64, lat: f64, lon: f64) {
        f.add(
            "node.poll",
            json!({"node": node, "ok": true, "lat": lat, "lon": lon, "links": [
                {"neighbor": heard, "type": "RF", "signal": -60.0, "noise": -95.0}]}),
            at,
        );
    }

    #[test]
    fn the_map_layer_draws_each_link_from_longitude_and_latitude() {
        let mut f = fake();
        let cfg: Config = toml::from_str("").unwrap();
        let now = T0 + 10 * MIN;
        poll(&mut f, "TST-A", "TST-B", now - 5 * MIN, 35.0, -120.5);
        poll(&mut f, "TST-B", "TST-A", now - 5 * MIN, 35.2, -120.7);
        let v = layer(&f.conn, &cfg, now).unwrap();
        assert_eq!(v["type"], "FeatureCollection");
        let fs = v["features"].as_array().unwrap();
        let kind =
            |k: &str| -> Vec<&Value> { fs.iter().filter(|x| x["geometry"]["type"] == k).collect() };
        let lines = kind("LineString");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["id"], "tst-a|tst-b");
        assert_eq!(
            lines[0]["geometry"]["coordinates"],
            json!([[-120.5, 35.0], [-120.7, 35.2]])
        );
        // 0.2 degrees of latitude and of longitude at 35 N: about 28.7 km
        let km = lines[0]["properties"]["length_km"].as_f64().unwrap();
        assert!((km - 28.7).abs() < 0.3, "{km}");
        assert_eq!(lines[0]["properties"]["long"], true);
        let points: Vec<(Value, Value)> = kind("Point")
            .into_iter()
            .map(|x| (x["id"].clone(), x["geometry"]["coordinates"].clone()))
            .collect();
        assert_eq!(
            points,
            [
                (json!("tst-a"), json!([-120.5, 35.0])),
                (json!("tst-b"), json!([-120.7, 35.2]))
            ]
        );
    }

    #[test]
    fn the_front_page_and_the_link_page_take_the_dry_baseline() {
        let mut f = fake();
        let mut cfg: Config =
            toml::from_str("[aprs]\n[[site]]\nnode = \"tst-hub\"\naprs = [\"CW0001\"]").unwrap();
        cfg.validate().unwrap();
        // 19 days at -60 dBm, except 14:00 to 17:00 on the 12 days it rained at the hub,
        // when the link was 3 dB down: most days, so a median over every day is the wet level
        let rained = |hour: i64| (hour - T0) / DAY < 12 && (14..17).contains(&(hour % DAY / HOUR));
        for k in 0..19 * 24 {
            let hour = T0 + k * HOUR;
            let level = if rained(hour) { -63.0 } else { -60.0 };
            for (node, nb) in [("tst-hub", "tst-n1"), ("tst-n1", "tst-hub")] {
                f.conn
                    .execute(
                        "INSERT INTO link_hours (node, neighbor, hour, n, signal) VALUES (?1, ?2, ?3, 12, ?4)",
                        params![node, nb, hour, level],
                    )
                    .unwrap();
            }
            if rained(hour) {
                let r = Report {
                    source: "aprs".into(),
                    station: "CW0001".into(),
                    ts: hour + 50 * MIN,
                    rain_1h_mm: Some(2.0),
                    ..Default::default()
                };
                rain::store(&f.conn, &r).unwrap();
            }
        }
        // a dry afternoon: the link at its usual -60 at 15:35
        let now = T0 + 19 * DAY + 15 * HOUR + 40 * MIN;
        poll(&mut f, "TST-HUB", "TST-N1", now - 5 * MIN, 35.3, -120.6);
        poll(&mut f, "TST-N1", "TST-HUB", now - 5 * MIN, 35.4, -120.7);
        let hist = recent_history(&f.conn, &cfg, now).unwrap();
        let ends = end_status(&f.conn, &cfg, now, &hist).unwrap();
        assert_eq!(ends.len(), 2);
        for e in &ends {
            assert_eq!(e.baseline_days, 19);
            let r = e.residual.unwrap();
            assert!(r.abs() < 0.01, "{:?} residual {r}", e.end);
        }
        let pair: [End; 2] = [
            ("tst-hub".into(), "tst-n1".into()),
            ("tst-n1".into(), "tst-hub".into()),
        ];
        let data = load_link(&f.conn, &cfg, &pair, now - DAY, now).unwrap();
        for d in directions(&cfg, &pair, &data, 10 * MIN) {
            assert_eq!(d.baseline.last().map(|b| b.1), Some(-60.0), "{:?}", d.end);
            assert_eq!(d.residual.last().map(|r| r.1), Some(0.0), "{:?}", d.end);
        }
    }

    #[test]
    fn the_overview_has_each_link_once_and_every_source_set_up() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: Config = toml::from_str(&format!(
            "data_dir = {:?}\npublish = false\nlinks = [[\"tst-hub\", \"tst-n1\"]]\n[aprs]\n\
             [radar]\n[soundings]\nstations = [\"72393\"]\n[[site]]\nnode = \"tst-hub\"\n\
             label = \"Hub Peak\"\naprs = [\"N0CALL-13\", \"N0CALL-14\"]",
            dir.path()
        ))
        .unwrap();
        cfg.validate().unwrap();
        let mut f = fake();
        let now = T0 + 10 * MIN;
        // heard for three days, so a link that turns up now is new
        f.conn
            .execute(
                "INSERT INTO link_hours (node, neighbor, hour, n, signal) VALUES ('tst-hub', 'tst-n1', ?1, 12, -60)",
                params![now - 3 * DAY - now % HOUR],
            )
            .unwrap();
        poll(&mut f, "TST-HUB", "TST-N1", now - 5 * MIN, 35.0, -120.5);
        poll(&mut f, "TST-N1", "TST-HUB", now - 4 * MIN, 35.2, -120.7);
        poll(&mut f, "TST-N2", "TST-HUB", now - 50 * MIN, 35.3, -120.5);
        let r = Report {
            source: "aprs".into(),
            station: "N0CALL-13".into(),
            ts: now - 3 * MIN,
            rain_1h_mm: Some(1.5),
            ..Default::default()
        };
        rain::store(&f.conn, &r).unwrap();
        let radar = Report {
            source: crate::radar::SOURCE.into(),
            station: "tst-hub".into(),
            ts: now - 20 * MIN,
            rain_1h_mm: Some(3.0),
            ..Default::default()
        };
        rain::store(&f.conn, &radar).unwrap();
        let drop = crate::detect::Drop {
            a: "tst-hub".into(),
            b: "tst-n1".into(),
            start: now - 2 * HOUR,
            end: Some(now - HOUR),
            last_low: now - HOUR - 10 * MIN,
            depth: 3.0,
        };
        let wet = Candidate::Wet(crate::detect::Wet {
            site: "tst-hub".into(),
            start: drop.start,
            end: drop.end,
            drops: vec![drop],
            site_links: 1,
            others: 0,
            others_dropping: 0,
        });
        f.conn
            .execute(
                "INSERT INTO events (kind, start, end, site, detail) VALUES ('wet', ?1, ?2, 'tst-hub', ?3)",
                params![wet.start(), wet.end(), serde_json::to_string(&wet).unwrap()],
            )
            .unwrap();
        let app = App::new(cfg).unwrap();
        *app.db() = f.conn;
        app.failed("aprs", "APRS-IS 127.0.0.1:9: connection refused");
        *app.analyzed.lock().unwrap() = Some(now - MIN);

        let o = overview_data(&app, now).unwrap();
        // top reads it back through the same types
        let o: Overview = serde_json::from_str(&serde_json::to_string(&o).unwrap()).unwrap();
        assert_eq!(o.links.len(), 1, "both directions of one link");
        let l = &o.links[0];
        let heard: Vec<(&str, &str)> = l
            .ends
            .iter()
            .map(|e| (e.node.as_str(), e.hears.as_str()))
            .collect();
        assert_eq!((l.a.as_str(), l.b.as_str()), ("tst-hub", "tst-n1"));
        assert_eq!(heard, [("tst-hub", "tst-n1"), ("tst-n1", "tst-hub")]);
        assert_eq!(l.ends[1].last_ms, now - 4 * MIN);
        assert_eq!(l.ends[0].residual_db, None, "no history, no baseline");
        assert!((l.length_km.unwrap() - 28.7).abs() < 0.3);
        let gauges: Vec<(&str, Option<i64>, &str)> = o
            .gauges
            .iter()
            .map(|g| (g.station.as_str(), g.at_ms, g.said.as_str()))
            .collect();
        assert_eq!(
            gauges,
            [
                ("N0CALL-13", Some(now - 3 * MIN), "1.5 mm in the last hour"),
                ("N0CALL-14", None, ""),
            ],
            "a station set up but never heard is listed too"
        );
        assert_eq!(o.gauges[0].sites, ["tst-hub"]);
        assert_eq!(o.soundings.len(), 1);
        assert_eq!(
            (o.soundings[0].station.as_str(), o.soundings[0].at_ms),
            ("72393", None)
        );
        assert!(o.radar_on);
        assert_eq!(
            o.radar,
            Some(RadarHour {
                at_ms: now - 20 * MIN,
                sites: 1,
                wet_sites: 1,
                max_mm: Some(3.0)
            })
        );
        assert_eq!(o.analyzed_ms, Some(now - MIN));
        let new: Vec<(&str, &str, i64, bool, bool)> = o
            .new_links
            .iter()
            .map(|l| (l.a.as_str(), l.b.as_str(), l.first_ms, l.long, l.studied))
            .collect();
        assert_eq!(
            new,
            [("tst-hub", "tst-n2", now - 50 * MIN, true, false)],
            "new, 33 km long, and not one of `links`"
        );
        assert!((o.new_links[0].length_km.unwrap() - 33.4).abs() < 0.2);
        assert_eq!(o.new_more, 0);
        assert_eq!(o.problems.len(), 1);
        assert_eq!(o.problems[0].source, "aprs");
        assert_eq!(o.labels["tst-hub"], "Hub Peak");
        assert_eq!(o.candidates.len(), 1);
        let c = &o.candidates[0];
        assert_eq!(c.title, "Wet antenna? at Hub Peak");
        assert!(
            c.evidence.iter().any(|e| e.contains("N0CALL-13")),
            "{:?}",
            c.evidence
        );
        assert_eq!(o.now.state, crate::now::State::Unknown);
        assert!(!o.publish && !o.publishing);

        // served again for a few seconds, then worked out afresh
        let first = overview_cached(&app, now).unwrap();
        app.failed(
            "radar",
            "radar (MRMS): example.org answered 503 Service Unavailable",
        );
        assert_eq!(
            overview_cached(&app, now + OVERVIEW_FOR - 1).unwrap(),
            first
        );
        let later = overview_cached(&app, now + OVERVIEW_FOR).unwrap();
        assert!(later.contains("answered 503"), "{later}");
    }

    #[tokio::test]
    async fn site_labels_are_escaped_on_the_front_and_events_pages() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: Config = toml::from_str(&format!(
            "data_dir = {:?}\npublish = false\n[[site]]\nnode = \"tst-hub\"\nlabel = \"<b>Hub</b>\"",
            dir.path()
        ))
        .unwrap();
        cfg.validate().unwrap();
        let app = App::new(cfg).unwrap();
        let start = now_ms() - 2 * HOUR;
        let wet = Candidate::Wet(crate::detect::Wet {
            site: "tst-hub".into(),
            start,
            end: Some(start + HOUR),
            drops: vec![],
            site_links: 2,
            others: 0,
            others_dropping: 0,
        });
        app.db()
            .execute(
                "INSERT INTO events (kind, start, end, site, detail) VALUES ('wet', ?1, ?2, 'tst-hub', ?3)",
                params![wet.start(), wet.end(), serde_json::to_string(&wet).unwrap()],
            )
            .unwrap();
        let app = Arc::new(app);
        let Ok(Html(front)) = overview(State(app.clone())).await else {
            panic!("the front page failed");
        };
        let events = events_body(&app, None).unwrap();
        for (name, page) in [("front", front), ("events", events)] {
            assert!(
                page.contains("Wet antenna? at &lt;b&gt;Hub&lt;/b&gt;"),
                "{name}: {page}"
            );
            assert!(!page.contains("<b>Hub</b>"), "{name} lets markup through");
        }
    }

    #[test]
    fn the_radar_row_is_the_newest_hour_over_every_site() {
        let at = |site: &str, ts: i64, mm: f64| Report {
            source: crate::radar::SOURCE.into(),
            station: site.into(),
            ts,
            rain_1h_mm: Some(mm),
            ..Default::default()
        };
        let reports = [
            at("tst-hub", T0, 3.0),
            at("tst-n1", T0, 0.0),
            at("tst-n2", T0, 5.1),
            at("tst-hub", T0 - HOUR, 9.0),
        ];
        let refs: Vec<&Report> = reports.iter().collect();
        assert_eq!(
            radar_hour(&refs),
            Some(RadarHour {
                at_ms: T0,
                sites: 3,
                wet_sites: 2,
                max_mm: Some(5.1)
            }),
            "the hour before doesn't count, and a 0 isn't rain"
        );
        assert_eq!(radar_hour(&refs[1..2]).unwrap().max_mm, None);
        assert_eq!(radar_hour(&[]), None);
    }
}
