//! The pages: an overview of the links and weather sources, a residual chart per link
//! with rain and sounding markers, the candidate events with their evidence, the methods,
//! and the OpenSense export.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use ridgewarden_client::graph::edge;
use ridgewarden_client::svg::{Chart, Line};
use ridgewarden_client::ui::{Color, Page, pill};
use ridgewarden_client::util::{esc, human_duration, ms_to_rfc3339, norm_node, now_ms, short_time};
use serde_json::json;

use crate::App;
use crate::config::Config;
use crate::events::{self, Candidate};
use crate::evidence;
use crate::now::{self, Confidence, Now, State as NowState};
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
fn enc(s: &str) -> String {
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
    every: Option<i64>,
    baseline_days: usize,
    residual: Option<f64>,
}

fn end_status(app: &App, now: i64) -> anyhow::Result<Vec<EndStatus>> {
    let cfg = &app.cfg;
    let db = app.db();
    let recent = series::all_samples(&db, now - DAY, now + 1)?;
    let hours = series::hours(&db, now - (cfg.baseline.days as i64 + 1) * DAY, now + 1)?;
    let today = series::day_of(now);
    let mut out: Vec<EndStatus> = recent
        .into_iter()
        .filter(|(e, _)| cfg.studies(&e.0, &e.1))
        .map(|(end, s)| {
            let h = hours.get(&end).map_or(&[][..], Vec::as_slice);
            let tail = &s[s.len().saturating_sub(1)..];
            EndStatus {
                samples: s.len(),
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

fn candidate_title(cfg: &Config, c: &Candidate) -> String {
    match c {
        Candidate::Wet(w) => format!("Wet antenna? at {}", esc(&cfg.label(&w.site))),
        Candidate::Duct(d) => format!("Ducting or fading? on {} links", d.links.len()),
        Candidate::Front(f) => format!(
            "Widespread drop? {} links at {} sites",
            f.drops.len(),
            f.sites.len()
        ),
    }
}

fn candidate_pill(c: &Candidate) -> String {
    match c {
        Candidate::Wet(_) => pill(Color::Yellow, "wet antenna?"),
        Candidate::Duct(_) => pill(Color::Unknown, "ducting/fading?"),
        Candidate::Front(_) => pill(Color::Yellow, "widespread drop?"),
    }
}

async fn overview(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let here = app.here();
    let now = now_ms();
    let cfg = &app.cfg;
    let a = app.clone();
    let (ends, ind) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let ends = end_status(&a, now)?;
        let ind = now::current(&a.db(), &a.cfg, now)?;
        Ok((ends, ind))
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
                candidate_title(cfg, c),
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
            "<p class=\"mute small\">A baseline needs {} days of history out of the last {}. \
             The residual is the latest level minus the usual for that hour.</p>\
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
    }
    sources_html(&app, &mut b)?;
    Ok(page(&here, "/", "Now", &b))
}

fn indicator_html(n: &Now) -> String {
    let (color, word) = match n.state {
        NowState::Unknown => (Color::Unknown, "UNKNOWN"),
        NowState::Calm => (Color::Green, "CALM"),
        NowState::Some => (Color::Green, "MOSTLY CALM"),
        NowState::Disturbed => (Color::Yellow, "DISTURBED"),
        NowState::Sustained => (Color::Yellow, "DUCTING OR FADING"),
        NowState::Down => (Color::Yellow, "WIDESPREAD DROP"),
    };
    let conf = match n.confidence {
        Confidence::None => "no confidence",
        Confidence::Low => "low confidence",
        Confidence::Medium => "medium confidence",
        Confidence::High => "high confidence",
    };
    let mut b = format!(
        "<h2>Marine layer now</h2><p>{} <span class=\"mute small\">{conf}</span></p><p>{}</p><ul class=\"plain small\">",
        pill(color, word),
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

fn sources_html(app: &App, b: &mut String) -> Result<(), Fail> {
    let cfg = &app.cfg;
    b.push_str("<h2>Weather data</h2>");
    let (reports, soundings) = {
        let db = app.db();
        (rain::latest(&db)?, sounding::latest(&db)?)
    };
    let max_h = cfg.sounding_height();
    if cfg.aprs.is_none() && cfg.nws.is_none() && cfg.soundings.is_none() && reports.is_empty() {
        b.push_str(
            "<p>No rain or sounding source is set up, so candidates can't be checked against \
             the weather. See <code>[aprs]</code>, <code>[nws]</code> and <code>[soundings]</code> \
             in the config.</p>",
        );
    }
    if !reports.is_empty() {
        b.push_str("<div class=wrap><table><tr><th>Station</th><th>Source</th><th>Last report</th><th>Said</th></tr>");
        for r in &reports {
            let _ = write!(
                b,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&r.station),
                esc(&r.source),
                esc(&when(cfg, r.ts)),
                esc(&report_text(r))
            );
        }
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
        for (at, why) in problems {
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
            candidate_title(cfg, c),
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
    let a = app.clone();
    let n = tokio::task::spawn_blocking(move || now::current(&a.db(), &a.cfg, now_ms())).await??;
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
    let b = tokio::task::spawn_blocking(move || results_body(&a, days)).await??;
    Ok(page(&here, "/results", "Results", &b))
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
        .map(|r| (r.ts, format!("rain at {}: {}", r.station, report_text(r))))
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
    hours: std::collections::BTreeMap<End, Vec<series::Hour>>,
    reports: Vec<Report>,
    soundings: Vec<Sounding>,
    cands: Vec<Candidate>,
}

fn load_link(app: &App, ends: &[End; 2], from: i64, to: i64) -> anyhow::Result<LinkData> {
    let cfg = &app.cfg;
    let (a, c) = (&ends[0].0, &ends[0].1);
    let db = app.db();
    Ok(LinkData {
        samples: ends
            .iter()
            .map(|e| series::samples(&db, e, from, to + 1))
            .collect::<anyhow::Result<_>>()?,
        hours: series::hours(&db, from - cfg.baseline.days as i64 * DAY, to + 1)?,
        reports: rain::reports(
            &db,
            &cfg.stations_near(&[a.clone(), c.clone()]),
            from,
            to + 1,
        )?,
        soundings: sounding::between(&db, from, to + 1)?,
        cands: events::list(&db, from, to + 1, None)?
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
    let (a, c) = edge(
        &norm_node(q.get("a").map_or("", |s| s)),
        &norm_node(q.get("b").map_or("", |s| s)),
    );
    let days: i64 = match q.get("days").map(String::as_str) {
        Some("2") => 2,
        Some("30") => 30,
        _ => 7,
    };
    let now = now_ms();
    let to = q
        .get("end")
        .and_then(|e| e.parse::<i64>().ok())
        .map_or(now, |e| e.clamp(0, now));
    let from = to - days * DAY;
    let width = if days == 30 {
        HOUR
    } else {
        cfg.baseline.bin_min as i64 * MIN
    };
    let ends: [End; 2] = [(a.clone(), c.clone()), (c.clone(), a.clone())];
    let LinkData {
        samples,
        hours,
        reports,
        soundings,
        cands,
    } = {
        let (app, ends) = (app.clone(), ends.clone());
        tokio::task::spawn_blocking(move || load_link(&app, &ends, from, to)).await??
    };
    if samples.iter().all(Vec::is_empty) && to == now && days == 7 {
        let body = "<h1>Not found</h1><p>No RF samples for a link between those sites in the \
                    last week. <a href=\"/\">All links</a></p>";
        return Ok((StatusCode::NOT_FOUND, page(&here, "", "Not found", body)).into_response());
    }
    let mut marks = markers(cfg, &reports, &soundings, &cands);
    marks.retain(|m| (from..=to).contains(&m.0));
    let none: Vec<series::Hour> = vec![];
    let h = |e: &End| hours.get(e).unwrap_or(&none);
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
    b.push_str("</p>");

    let label = |e: &End| format!("{} hears {}", cfg.label(&e.0), cfg.label(&e.1));
    let resid: Vec<Line> = ends
        .iter()
        .zip(&samples)
        .map(|(e, s)| Line {
            label: label(e),
            points: series::bin(&series::residuals(s, h(e), &cfg.baseline), width),
        })
        .filter(|l| !l.points.is_empty())
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
        let have = ends
            .iter()
            .map(|e| series::baseline_days(h(e), series::day_of(to), &cfg.baseline))
            .max()
            .unwrap_or(0);
        let _ = write!(
            b,
            "<p>No baseline for this period yet: it needs {} days of history before a day, and \
             this link has {have}.</p>",
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
    for (e, s) in ends.iter().zip(&samples) {
        if s.is_empty() {
            continue;
        }
        let wb = series::with_baseline(s, h(e), &cfg.baseline);
        let mut lines = vec![Line {
            label: "level".into(),
            points: series::bin(
                &s.iter().map(|x| (x.ts, x.signal)).collect::<Vec<_>>(),
                width,
            ),
        }];
        if !wb.is_empty() {
            lines.push(Line {
                label: "baseline".into(),
                points: series::bin(&wb.iter().map(|x| (x.0, x.2)).collect::<Vec<_>>(), width),
            });
        }
        let _ = write!(b, "<h2>{}</h2>", esc(&label(e)));
        b.push_str(&chart(lines, "dBm", None).render(&format!("Received level, {}", label(e))));
    }
    let noise: Vec<Line> = ends
        .iter()
        .zip(&samples)
        .map(|(e, s)| Line {
            label: label(e),
            points: series::bin(
                &s.iter()
                    .filter_map(|x| Some((x.ts, x.noise?)))
                    .collect::<Vec<_>>(),
                width,
            ),
        })
        .filter(|l| !l.points.is_empty())
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

async fn methods_page(State(app): State<Arc<App>>) -> Html<String> {
    let here = app.here();
    let c = &app.cfg;
    let (w, d, bl) = (&c.wet, &c.duct, &c.baseline);
    let mut b = String::from(
        "<h1>Methods</h1>\
         <p class=banner>Experimental. The rules below find candidates worth a look. None of \
         them measures rain, and a negative result is a result.</p>\
         <h2>What 5 GHz links can and can't see</h2>\
         <p>Rain falling across the path costs very little at 5 GHz: about 0.01 dB per km at \
         10 mm/h (ITU-R P.838), so a 30 km backbone path loses about 0.3 dB in steady rain. The \
         nodes report signal in whole dB and it wanders by a dB or two anyway. That is far too \
         small to map rainfall, which is why the commercial-link rain work (OpenSense) uses \
         links at 10 to 40 GHz and above.</p>\
         <p>Two things are big enough to see:</p>\
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
         hour over the previous {} days, once at least {} of them have data. The residual is \
         each sample minus its baseline, taken as {}-minute medians, and the two ends of a \
         link are averaged.</p>",
        bl.days, bl.min_days, bl.bin_min
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
         <code>/api/events</code> today.) The rain readings fetched from APRS-IS \
         and NWS go out as <code>weather.rain</code> telemetry. Both shapes are in the \
         README.</p>",
    );
    b.push_str(
        "<h2>Sources</h2><ul class=plain>\
         <li>Link levels: the ridgewarden core's node polls. For minute data on the backbone, \
         one hub core crawls the backbone nodes every 60 seconds, agreed with their owners.</li>\
         <li>Rain: APRS weather stations heard over APRS-IS (most are CWOP members; the rain \
         in the last hour), and NWS observations from api.weather.gov (precipitation in the \
         last hour and the observer's description).</li>\
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
         own reports. There is no transmit level: AREDN nodes don't report it.</p>",
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
