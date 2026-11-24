//! `ridgewarden-weather top`: the app live in a terminal, for an operator on a headless Pi
//! over SSH. It reads the app's own JSON API (`/api/overview`, and `/api/link` for the
//! chart that is open), not its database, so it needs no config and runs as any user.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use ridgewarden_client::tui::ratatui::Frame;
use ridgewarden_client::tui::ratatui::crossterm::event::{KeyCode, KeyEvent};
use ridgewarden_client::tui::ratatui::layout::{Constraint, Layout, Rect};
use ridgewarden_client::tui::ratatui::symbols::Marker;
use ridgewarden_client::tui::ratatui::text::{Line, Span};
use ridgewarden_client::tui::ratatui::widgets::{
    Axis, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table, TableState, Wrap,
};
use ridgewarden_client::tui::{
    self, Chrome, Fetch, Notice, Pill, Refresh, Theme, View, move_key, pill_table, tab_key,
    word_pill, word_table,
};
use ridgewarden_client::ui::Color;
use ridgewarden_client::util::{human_duration, ms_to_rfc3339};
use serde::Deserialize;

use crate::detect::{self, Pair};
use crate::events::Candidate;
use crate::now::FRESH;
use crate::series::{HOUR, MIN};
use crate::web::{CandidateRow, EndRow, LinkRow, Overview, enc, sites};

const TABS: [&str; 5] = ["Now", "Links", "Candidates", "Rain", "New links"];
/// How far back a link's chart goes; `/api/link` takes 2, 7 or 30.
const CHART_DAYS: i64 = 2;

/// Run `top` against the app at `base` until the user quits, reading again every `every`.
pub fn run(base: &str, every: Duration) -> Result<()> {
    let open = Arc::new(Mutex::new(None));
    let mut loader = Loader {
        // the overview works out every link's baseline, which takes a moment on a Pi
        fetch: Fetch::new(Duration::from_secs(10))?,
        base: base.trim_end_matches('/').to_string(),
        open: open.clone(),
    };
    let refresh = Refresh::spawn(every, async move || loader.load().await)?;
    let mut top = Top::new(Theme::from_env(), refresh, open);
    tui::run(&mut top, Duration::from_millis(250))?;
    Ok(())
}

/// One read of the app: the overview, and the open link's data while a chart is open.
struct Snap {
    got: Instant,
    overview: Result<Overview, String>,
    link: Option<(Pair, Result<LinkData, String>)>,
}

struct Loader {
    fetch: Fetch,
    base: String,
    /// the link whose chart is open, set by the view
    open: Arc<Mutex<Option<Pair>>>,
}

impl Loader {
    async fn load(&mut self) -> Snap {
        let overview = self
            .fetch
            .json(&format!("{}/api/overview", self.base))
            .await
            .map_err(|e| e.to_string());
        let open = self.open.lock().unwrap().clone();
        let link = match open {
            Some(p) if overview.is_ok() => {
                let url = format!(
                    "{}/api/link?a={}&b={}&days={CHART_DAYS}",
                    self.base,
                    enc(&p.0),
                    enc(&p.1)
                );
                let got = self.fetch.json(&url).await.map_err(|e| e.to_string());
                Some((p, got))
            }
            // an app that didn't answer for the overview won't for the chart; the last one stays
            _ => None,
        };
        Snap {
            got: Instant::now(),
            overview,
            link,
        }
    }
}

/// One link's data as `/api/link` returns it: the parts the chart uses.
#[derive(Clone, Debug, Deserialize)]
struct LinkData {
    from_ms: i64,
    to_ms: i64,
    bin_ms: i64,
    ends: Vec<LinkEnd>,
    rain_stations: Vec<String>,
    rain_hours: Vec<RainHour>,
    candidates: Vec<Candidate>,
}

#[derive(Clone, Debug, Deserialize)]
struct LinkEnd {
    node: String,
    hears: String,
    level_dbm: Vec<(i64, f64)>,
    baseline_dbm: Vec<(i64, f64)>,
    residual_db: Vec<(i64, f64)>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
struct RainHour {
    hour_ms: i64,
    wet: bool,
}

/// Where a studied link is against its baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    /// within `departure_db` of the usual level for the hour
    Usual,
    Below,
    Above,
    /// no sample in the last half hour, so its residual says nothing about now
    Quiet,
    /// no baseline for this hour yet
    Building,
}

impl Level {
    fn pill(self) -> (Color, &'static str) {
        match self {
            Level::Usual => (Color::Green, "USUAL"),
            Level::Below => (Color::Yellow, "BELOW"),
            Level::Above => (Color::Yellow, "ABOVE"),
            Level::Quiet => (Color::Unknown, "QUIET"),
            Level::Building => (Color::Unknown, "BUILDING"),
        }
    }
}

/// A row of the Links page.
struct LinkLine<'a> {
    row: &'a LinkRow,
    name: String,
    level: Level,
    /// the fresh ends' residuals averaged, as detection averages the two ends
    residual: Option<f64>,
    /// days of history at the least covered hour of day, at the end with more
    baseline_days: usize,
    every: Option<i64>,
}

fn link_lines(o: &Overview, now: i64) -> Vec<LinkLine<'_>> {
    let mut out: Vec<LinkLine> = o
        .links
        .iter()
        .map(|row| {
            let fresh: Vec<&EndRow> = row
                .ends
                .iter()
                .filter(|e| now.saturating_sub(e.last_ms) <= FRESH)
                .collect();
            let rs: Vec<f64> = fresh.iter().filter_map(|e| e.residual_db).collect();
            let residual = (!rs.is_empty()).then(|| rs.iter().sum::<f64>() / rs.len() as f64);
            let level = match residual {
                _ if fresh.is_empty() => Level::Quiet,
                None => Level::Building,
                Some(r) if r <= -o.departure_db => Level::Below,
                Some(r) if r >= o.departure_db => Level::Above,
                Some(_) => Level::Usual,
            };
            LinkLine {
                name: link_name(o, &row.a, &row.b),
                level,
                residual,
                baseline_days: row.ends.iter().map(|e| e.baseline_days).max().unwrap_or(0),
                every: row.ends.iter().filter_map(|e| e.every_ms).min(),
                row,
            }
        })
        .collect();
    out.sort_by(|x, y| x.name.cmp(&y.name));
    out
}

/// A row of the Rain page: a rain gauge, the radar, or a sounding station.
#[derive(Debug, PartialEq)]
struct Source {
    pill: Option<Pill>,
    kind: String,
    station: String,
    near: String,
    at: Option<i64>,
    said: String,
    /// why reading it fails, while it does
    failing: Option<String>,
}

/// How often each kind of source reports: three of these without a word and it is stale.
fn gap(kind: &str) -> i64 {
    match kind {
        // APRS weather stations beacon every 5 to 10 minutes
        "aprs" => 10 * MIN,
        // soundings fly at 00 and 12 UTC
        "sounding" => 12 * HOUR,
        // NWS observations and the radar come hourly
        _ => HOUR,
    }
}

fn sources(o: &Overview, now: i64) -> Vec<Source> {
    // the app files a source's errors under these keys (see App::failed)
    let problem = |key: &str| {
        o.problems
            .iter()
            .find(|p| p.source == key)
            .map(|p| p.what.clone())
    };
    let source = |kind: &str, station: &str, near, at, said, failing: Option<String>| Source {
        pill: Pill::freshness(failing.is_some(), at, now, gap(kind)),
        kind: kind.to_string(),
        station: station.to_string(),
        near,
        at,
        said,
        failing,
    };
    let mut out = vec![];
    for g in &o.gauges {
        let key = match g.source.as_str() {
            "aprs" => "aprs".to_string(),
            other => format!("{other} {}", g.station),
        };
        let near: Vec<&str> = g.sites.iter().map(|s| label(o, s)).collect();
        out.push(source(
            &g.source,
            &g.station,
            near.join(", "),
            g.at_ms,
            g.said.clone(),
            problem(&key),
        ));
    }
    if o.radar_on || o.radar.is_some() {
        let (near, at, said) = match &o.radar {
            Some(r) => (
                sites(r.sites),
                Some(r.at_ms),
                match r.max_mm {
                    Some(mm) => format!(
                        "rain at {} of {}, up to {mm:.1} mm in the hour",
                        r.wet_sites,
                        sites(r.sites)
                    ),
                    None => {
                        "under 0.1 in (2.5 mm) everywhere, which can still be drizzle".to_string()
                    }
                },
            ),
            None => (String::new(), None, String::new()),
        };
        out.push(source("radar", "MRMS", near, at, said, problem("radar")));
    }
    for s in &o.soundings {
        out.push(source(
            "sounding",
            &s.station,
            String::new(),
            s.at_ms,
            s.summary.clone().unwrap_or_default(),
            problem(&format!("sounding {}", s.station)),
        ));
    }
    out
}

fn label<'a>(o: &'a Overview, node: &'a str) -> &'a str {
    o.labels.get(node).map_or(node, String::as_str)
}

fn link_name(o: &Overview, a: &str, b: &str) -> String {
    format!("{} - {}", label(o, a), label(o, b))
}

/// "11-23 01:10", in UTC.
fn clock(ms: i64) -> String {
    let t = ms_to_rfc3339(ms);
    format!("{} {}", &t[5..10], &t[11..16])
}

/// "3 min ago"
fn ago(now: i64, t: i64) -> String {
    format!("{} ago", human_duration(now.saturating_sub(t)))
}

/// What to say when the app never answered: its fetch error, and what to check.
fn no_answer(e: &str) -> String {
    if reason(e) == "HTTP 404" {
        return format!(
            "{e}: this ridgewarden-weather is older than top and has no /api/overview. \
             Update it to the same version."
        );
    }
    format!(
        "Can't reach the app: {e}. Is it running (systemctl status ridgewarden-weather)? If \
         it listens somewhere else, point --url at it."
    )
}

/// The few words a fetch error ends with: "could not connect", "timed out", "HTTP 503".
fn reason(err: &str) -> &str {
    err.rsplit_once(": ").map_or(err, |(_, why)| why)
}

fn db(v: Option<f64>) -> String {
    v.map_or("-".into(), |v| format!("{v:+.1} dB"))
}

fn dbm(v: Option<f64>) -> String {
    v.map_or("-".into(), |v| format!("{v:.1} dBm"))
}

/// "2 below, 7 usual": the counts that aren't zero.
fn counts(parts: &[(usize, &str)]) -> String {
    let parts: Vec<String> = parts
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
    parts.join(", ")
}

/// Wet hours near either end of a link, joined into spells: (start, end).
fn wet_spells(hours: &[RainHour]) -> Vec<(i64, i64)> {
    let mut wet: Vec<i64> = hours.iter().filter(|h| h.wet).map(|h| h.hour_ms).collect();
    wet.sort();
    let mut out: Vec<(i64, i64)> = vec![];
    for h in wet {
        match out.last_mut() {
            Some(s) if s.1 == h => s.1 = h.saturating_add(HOUR),
            _ => out.push((h, h.saturating_add(HOUR))),
        }
    }
    out
}

/// "11-22 01:00 to 05:00", or with the end's date when it differs.
fn spell(from: i64, to: i64) -> String {
    let (a, b) = (clock(from), clock(to));
    if a[..5] == b[..5] {
        format!("{a} to {}", &b[6..])
    } else {
        format!("{a} to {b}")
    }
}

/// Both ends' residual bins averaged, as detection sees the link.
fn residual(d: &LinkData) -> Vec<(i64, f64)> {
    let ends = d
        .ends
        .iter()
        .map(|e| ((e.node.clone(), e.hears.clone()), e.residual_db.clone()))
        .collect();
    detect::link_series(&ends)
        .into_values()
        .next()
        .unwrap_or_default()
}

/// Points as chart lines, broken where more than `gap` passes between two.
fn segments(pts: &[(i64, f64)], gap: i64) -> Vec<Vec<(f64, f64)>> {
    let mut out: Vec<Vec<(f64, f64)>> = vec![];
    let mut prev: Option<i64> = None;
    for &(t, v) in pts {
        match out.last_mut() {
            Some(s) if prev.is_some_and(|p| t.saturating_sub(p) <= gap) => s.push((t as f64, v)),
            _ => out.push(vec![(t as f64, v)]),
        }
        prev = Some(t);
    }
    out
}

/// A page's rows as keys that stay with their items while rows re-sort or come and go
/// between reads.
fn row_keys(o: &Overview, tab: usize, now: i64) -> Vec<String> {
    let pair = |a: &str, b: &str| format!("{a}|{b}");
    match tab {
        1 => link_lines(o, now)
            .iter()
            .map(|l| pair(&l.row.a, &l.row.b))
            .collect(),
        // a later scan finding a candidate again keeps its start, kind and first link
        2 => o
            .candidates
            .iter()
            .map(|r| {
                let c = &r.candidate;
                let first = c.links().into_iter().next().unwrap_or_default();
                format!("{}|{}|{}", c.start(), c.kind(), pair(&first.0, &first.1))
            })
            .collect(),
        3 => sources(o, now)
            .iter()
            .map(|s| pair(&s.kind, &s.station))
            .collect(),
        4 => o.new_links.iter().map(|l| pair(&l.a, &l.b)).collect(),
        _ => vec![],
    }
}

/// The open link's chart: the last good read of it, and why the newest failed if it did.
struct Plot {
    pair: Pair,
    data: Option<LinkData>,
    err: Option<String>,
}

struct Top {
    theme: Theme,
    refresh: Refresh<Snap>,
    /// the link whose chart is open; the loader reads it too
    open: Arc<Mutex<Option<Pair>>>,
    /// the newest overview and when it was read
    data: Option<(Instant, Overview)>,
    /// why the latest read failed, if it did; the last good one stays on screen
    err: Option<String>,
    /// the open chart, once read
    chart: Option<Plot>,
    tab: usize,
    /// each page's selected row
    rows: [TableState; TABS.len()],
    /// rows a page had room for at the last draw, for PgUp and PgDn
    page: usize,
}

impl Top {
    fn new(theme: Theme, refresh: Refresh<Snap>, open: Arc<Mutex<Option<Pair>>>) -> Self {
        Self {
            theme,
            refresh,
            open,
            data: None,
            err: None,
            chart: None,
            tab: 0,
            rows: Default::default(),
            page: 0,
        }
    }

    fn opened(&self) -> Option<Pair> {
        self.open.lock().unwrap().clone()
    }

    fn open_chart(&mut self, p: Pair) {
        *self.open.lock().unwrap() = Some(p);
        self.chart = None;
        self.refresh.again();
    }

    fn close_chart(&mut self) {
        *self.open.lock().unwrap() = None;
        self.chart = None;
    }

    fn take(&mut self, s: Snap) {
        match s.overview {
            Ok(o) => {
                let picked: Vec<Option<String>> =
                    (0..TABS.len()).map(|t| self.selected_key(t)).collect();
                self.data = Some((s.got, o));
                self.err = None;
                self.follow(picked);
            }
            Err(e) => self.err = Some(e),
        }
        // a read started before the chart was closed, or another one opened, is dropped
        if let Some((p, got)) = s.link
            && self.opened().as_ref() == Some(&p)
        {
            let last = self
                .chart
                .take()
                .filter(|c| c.pair == p)
                .and_then(|c| c.data);
            self.chart = Some(match got {
                Ok(d) => Plot {
                    pair: p,
                    data: Some(d),
                    err: None,
                },
                Err(e) => Plot {
                    pair: p,
                    data: last,
                    err: Some(e),
                },
            });
        }
    }

    /// The key of a page's selected row.
    fn selected_key(&self, tab: usize) -> Option<String> {
        let i = self.rows[tab].selected()?;
        row_keys(&self.data.as_ref()?.1, tab, self.now()?)
            .into_iter()
            .nth(i)
    }

    /// Put each page's selection back on the item it was on before the newest read; one
    /// that has gone leaves the selection where it was, on whatever is there now.
    fn follow(&mut self, picked: Vec<Option<String>>) {
        let Some(now) = self.now() else { return };
        let Some((_, o)) = &self.data else { return };
        for (tab, key) in picked.into_iter().enumerate() {
            let keys = row_keys(o, tab, now);
            let at = key.and_then(|k| keys.iter().position(|x| *x == k));
            let at = at.or(self.rows[tab]
                .selected()
                .map(|i| i.min(keys.len().saturating_sub(1))));
            self.rows[tab].select(at.filter(|_| !keys.is_empty()));
        }
    }

    /// The app's clock: when the overview was made, plus the time since it was read.
    fn now(&self) -> Option<i64> {
        let (got, o) = self.data.as_ref()?;
        Some(
            o.generated_ms
                .saturating_add(got.elapsed().as_millis() as i64),
        )
    }

    /// Rows in a page's table.
    fn len(&self, tab: usize) -> usize {
        let Some((_, o)) = &self.data else { return 0 };
        match tab {
            1 => o.links.len(),
            2 => o.candidates.len(),
            3 => sources(o, o.generated_ms).len(),
            4 => o.new_links.len(),
            _ => 0,
        }
    }

    /// The link on the selected row, on the pages that list links.
    fn selected_link(&self) -> Option<Pair> {
        let o = &self.data.as_ref()?.1;
        let i = self.rows[self.tab].selected()?;
        match self.tab {
            1 => link_lines(o, self.now()?)
                .get(i)
                .map(|l| (l.row.a.clone(), l.row.b.clone())),
            4 => o.new_links.get(i).map(|l| (l.a.clone(), l.b.clone())),
            _ => None,
        }
    }

    fn status(&self) -> Span<'static> {
        let Some((got, _)) = &self.data else {
            return match self.err {
                Some(_) => Span::styled("not answering", self.theme.bad()),
                None => Span::styled("loading", self.theme.mute()),
            };
        };
        let age = format!(
            "updated {} ago",
            human_duration(got.elapsed().as_millis() as i64)
        );
        match self.err {
            Some(_) => Span::styled(format!("not answering, {age}"), self.theme.bad()),
            None => Span::styled(age, self.theme.mute()),
        }
    }

    fn help(&self) -> &'static str {
        if self.opened().is_some() {
            return "q quit  Esc back  1-5 Tab pages  r reload";
        }
        match self.tab {
            0 => "q quit  1-5 Tab pages  r reload",
            1 | 4 => "q quit  1-5 Tab pages  Up Down PgUp PgDn move  Enter chart  r reload",
            _ => "q quit  1-5 Tab pages  Up Down PgUp PgDn move  r reload",
        }
    }
}

impl View for Top {
    fn tick(&mut self) -> io::Result<()> {
        if let Some(s) = self.refresh.latest()? {
            self.take(s);
        }
        Ok(())
    }

    fn key(&mut self, k: KeyEvent) -> bool {
        if let Some(t) = tab_key(&k, self.tab, TABS.len()) {
            self.close_chart();
            self.tab = t;
        } else if matches!(k.code, KeyCode::Char('r' | 'R')) {
            self.refresh.again();
        } else if self.opened().is_some() {
            if matches!(k.code, KeyCode::Esc | KeyCode::Backspace | KeyCode::Enter) {
                self.close_chart();
            }
        } else if k.code == KeyCode::Enter
            && let Some(p) = self.selected_link()
        {
            self.open_chart(p);
        } else {
            let len = self.len(self.tab);
            move_key(&mut self.rows[self.tab], &k, len, self.page);
        }
        true
    }

    fn draw(&mut self, f: &mut Frame) {
        let title = match &self.data {
            Some((_, o)) if !o.here.is_empty() => {
                format!("ridgewarden-weather (experimental)  {}", o.here)
            }
            _ => "ridgewarden-weather (experimental)".into(),
        };
        let chrome = Chrome {
            title: &title,
            status: self.status(),
            tabs: &TABS,
            tab: self.tab,
            help: self.help(),
        };
        let Some(mut area) = chrome.draw(f, &self.theme) else {
            return;
        };
        let theme = &self.theme;
        if let (Some(e), Some(_)) = (&self.err, &self.data) {
            let [banner, rest] =
                Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
            let msg = format!("Reading failed ({}), so this is from earlier.", reason(e));
            f.render_widget(Line::styled(msg, theme.bad()), banner);
            area = rest;
        }
        // a table's header and the lines above it
        self.page = area.height.saturating_sub(4).into();
        let open = self.opened();
        let Some(now) = self.now() else {
            let msg = self.err.as_deref().map(no_answer);
            let n = match &msg {
                Some(m) => Notice::Error(m),
                None => Notice::Loading,
            };
            f.render_widget(n.paragraph(theme), area);
            return;
        };
        let Some((_, o)) = &self.data else { return };
        let c = Ctx { theme, o, now };
        if let Some(p) = open {
            chart_page(&c, f, area, &p, self.chart.as_ref());
            return;
        }
        let rows = &mut self.rows[self.tab];
        match self.tab {
            0 => now_page(&c, f, area),
            1 => links_page(&c, f, area, rows),
            2 => candidates_page(&c, f, area, rows),
            3 => rain_page(&c, f, area, rows),
            _ => new_page(&c, f, area, rows),
        }
    }
}

/// What every page draws from.
struct Ctx<'a> {
    theme: &'a Theme,
    o: &'a Overview,
    /// the app's clock, for ages
    now: i64,
}

/// Rows for a table of `rows` rows in `height`, leaving `below` rows and a gap for the
/// detail under it.
fn fit(rows: usize, height: u16, below: u16) -> u16 {
    (rows as u16).min(height.saturating_sub(below + 1))
}

/// About how many rows `lines` take wrapped to `width`; word wrap can take one more.
fn wrapped_rows(lines: &[Line], width: u16) -> u16 {
    let rows: usize = lines
        .iter()
        .map(|l| l.width().div_ceil(width.max(1) as usize).max(1))
        .sum();
    rows as u16 + 1
}

/// Lines over a page's table, and the area left for the table.
fn above(f: &mut Frame, area: Rect, lines: Vec<Line<'static>>) -> Rect {
    let [head, rest] = Layout::vertical([
        Constraint::Length(lines.len() as u16 + 1),
        Constraint::Fill(1),
    ])
    .areas(area);
    f.render_widget(Paragraph::new(lines), head);
    rest
}

/// "Detection runs every 10 minutes, last 4 min ago. Candidates go out to the mesh."
fn detection(o: &Overview, now: i64) -> String {
    let ran = match o.analyzed_ms {
        Some(t) => format!(
            "Detection runs every 10 minutes, last {} ago.",
            human_duration(now.saturating_sub(t))
        ),
        None => "Detection starts once the core's feed has been read.".into(),
    };
    let out = if o.publishing {
        "Candidates go out to the mesh as weather.candidate events."
    } else if o.publish {
        "Candidates aren't going out: the core's token couldn't be read."
    } else {
        "Publishing to the mesh is off (publish = false)."
    };
    format!("{ran} {out}")
}

/// A candidate as a row of a `word_table`: its kind, start, end and heading.
fn candidate_row(r: &CandidateRow) -> (Option<(&'static str, Color)>, Vec<Cell<'static>>) {
    let (color, word) = r.candidate.pill();
    let end = r.candidate.end().map_or("going".into(), clock);
    let cells = vec![
        Cell::from(clock(r.candidate.start())),
        Cell::from(end),
        Cell::from(r.title.clone()),
    ];
    (Some((word, color)), cells)
}

const CANDIDATE_WIDTHS: [Constraint; 3] = [
    Constraint::Length(11),
    Constraint::Length(11),
    Constraint::Fill(1),
];

fn now_page(c: &Ctx, f: &mut Frame, area: Rect) {
    let o = c.o;
    let n = &o.now;
    let (color, word) = n.state.pill();
    let mut lines = vec![
        Line::from(vec![
            word_pill(c.theme, word, color),
            Span::raw("  "),
            Span::styled(n.confidence.words(), c.theme.mute()),
        ]),
        Line::from(n.verdict.clone()),
    ];
    for p in &o.problems {
        lines.push(Line::styled(
            format!("{} (since {} UTC)", p.what, clock(p.at_ms)),
            c.theme.bad(),
        ));
    }
    lines.push(Line::styled(detection(o, c.now), c.theme.mute()));
    for e in &n.evidence {
        lines.push(Line::styled(format!("- {e}"), c.theme.mute()));
    }
    let shown = o.candidates.len().min(5);
    let list_h = shown.max(2) as u16;
    let text_h = wrapped_rows(&lines, area.width).min(area.height.saturating_sub(list_h + 2));
    let [top, _, head, list] = Layout::vertical([
        Constraint::Length(text_h),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), top);
    let mut heading = "Candidates in the last 7 days (UTC)".to_string();
    if o.candidates.len() > shown {
        heading.push_str(&format!(", newest {shown} of {}", o.candidates.len()));
    }
    f.render_widget(Line::styled(heading, c.theme.heading()), head);
    if o.candidates.is_empty() {
        let why = "None. Detection starts once links have a baseline, and runs every 10 minutes.";
        f.render_widget(Notice::Empty(why).paragraph(c.theme), list);
        return;
    }
    let rows = o.candidates[..shown].iter().map(candidate_row);
    f.render_widget(word_table(c.theme, &[], &CANDIDATE_WIDTHS, rows), list);
}

fn links_page(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) {
    let o = c.o;
    let lines = link_lines(o, c.now);
    if lines.is_empty() {
        let why = "No RF link samples in the last 24 hours yet. This app reads the link levels in \
                   the core's node polls; for the backbone, the hub core needs [crawl] on with \
                   interval_s = 60 for the backbone nodes, agreed with their owners.";
        f.render_widget(Notice::Empty(why).paragraph(c.theme), area);
        return;
    }
    let n = |l: Level| lines.iter().filter(|x| x.level == l).count();
    let summary = counts(&[
        (n(Level::Below), "below"),
        (n(Level::Above), "above"),
        (n(Level::Usual), "usual"),
        (n(Level::Quiet), "quiet"),
        (n(Level::Building), "building a baseline"),
    ]);
    let area = above(
        f,
        area,
        vec![
            Line::from(format!(
                "{} studied links heard in the last day: {summary}.",
                lines.len()
            )),
            Line::styled(
                format!(
                    "Residual: the newest level minus the usual for that hour. {:.0} dB is off.",
                    o.departure_db
                ),
                c.theme.mute(),
            ),
        ],
    );
    let items = lines.iter().map(|l| {
        let (color, word) = l.level.pill();
        let base = if l.baseline_days >= o.min_days as usize {
            "ready".to_string()
        } else {
            format!("{} of {} d", l.baseline_days, o.min_days)
        };
        let cells = vec![
            Cell::from(l.name.clone()),
            Cell::from(l.row.length_km.map_or("-".into(), |k| format!("{k:.1}"))),
            Cell::from(l.every.map_or("-".into(), human_duration)),
            Cell::from(base),
            Cell::from(db(l.residual)),
        ];
        (Some((word, color)), cells)
    });
    let header = ["Link", "km", "Every", "Baseline", "Residual"];
    let widths = [
        Constraint::Fill(1),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(9),
        Constraint::Length(8),
    ];
    f.render_stateful_widget(word_table(c.theme, &header, &widths, items), area, rows);
}

/// Links listed under a candidate; a widespread drop can have dozens, which would push the
/// rest off the screen.
const LINKS_SHOWN: usize = 3;

/// "and 1 more link", "and 9 more links"
fn and_more(n: usize) -> String {
    format!("and {n} more link{}", if n == 1 { "" } else { "s" })
}

/// A candidate's summary, its evidence, then its links, for the lines under the list.
fn candidate_detail(c: &Ctx, r: &CandidateRow) -> Vec<Line<'static>> {
    let o = c.o;
    let mut out = vec![Line::from(r.summary.clone())];
    out.extend(
        r.evidence
            .iter()
            .map(|e| Line::styled(e.clone(), c.theme.mute())),
    );
    match &r.candidate {
        Candidate::Wet(w) => out.extend(drop_lines(c, &w.drops)),
        Candidate::Front(fr) => out.extend(drop_lines(c, &fr.drops)),
        Candidate::Duct(d) => {
            let mut names: Vec<String> = (d.links.iter().take(LINKS_SHOWN))
                .map(|(a, b)| link_name(o, a, b))
                .collect();
            if d.links.len() > LINKS_SHOWN {
                names.push(and_more(d.links.len() - LINKS_SHOWN));
            }
            out.push(Line::from(format!(
                "{} up, {} down at the peak, largest {:.1} dB. Links: {}.",
                d.peak_up,
                d.peak_down,
                d.max_abs,
                names.join(", ")
            )));
        }
    }
    out
}

fn drop_lines(c: &Ctx, drops: &[detect::Drop]) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = (drops.iter().take(LINKS_SHOWN))
        .map(|d| {
            let back = match d.end {
                Some(e) => format!("back after {}", human_duration(e.saturating_sub(d.start))),
                None => "no recovery seen".into(),
            };
            Line::from(format!(
                "{}: down {:.1} dB at {}, {back}",
                link_name(c.o, &d.a, &d.b),
                d.depth,
                clock(d.start)
            ))
        })
        .collect();
    if drops.len() > LINKS_SHOWN {
        out.push(Line::from(and_more(drops.len() - LINKS_SHOWN)));
    }
    out
}

fn candidates_page(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) {
    let list = &c.o.candidates;
    if list.is_empty() {
        let why = "No candidates in the last 7 days. Detection starts once links have a baseline \
                   (see Links), and runs every 10 minutes.";
        f.render_widget(Notice::Empty(why).paragraph(c.theme), area);
        return;
    }
    let head = "The last 7 days, newest first, from start to end (UTC).";
    let area = above(f, area, vec![Line::styled(head, c.theme.mute())]);
    let [top, _, more] = Layout::vertical([
        Constraint::Length(fit(list.len(), area.height, 8)),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    let items = list.iter().map(candidate_row);
    f.render_stateful_widget(
        word_table(c.theme, &[], &CANDIDATE_WIDTHS, items),
        top,
        rows,
    );
    let text = match rows.selected().and_then(|i| list.get(i)) {
        Some(r) => Paragraph::new(candidate_detail(c, r)),
        None => Paragraph::new(Line::styled(
            "Pick a candidate with the arrow keys to see its evidence here.",
            c.theme.mute(),
        )),
    };
    f.render_widget(text.wrap(Wrap { trim: false }), more);
}

fn rain_page(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) {
    let list = sources(c.o, c.now);
    if list.is_empty() {
        let why = "No rain or sounding source is set up, so candidates can't be checked against \
                   the weather. See [aprs], [nws], [radar] and [soundings] in the config.";
        f.render_widget(Notice::Empty(why).paragraph(c.theme), area);
        return;
    }
    let [top, _, more] = Layout::vertical([
        Constraint::Length(fit(list.len() + 1, area.height, 4)),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    let items = list.iter().map(|s| {
        let said = match s.at {
            Some(_) => s.said.clone(),
            None => "nothing heard yet".to_string(),
        };
        let cells = vec![
            Cell::from(s.kind.clone()),
            Cell::from(s.station.clone()),
            Cell::from(s.near.clone()),
            Cell::from(s.at.map_or("-".into(), |t| ago(c.now, t))),
            Cell::from(said),
        ];
        (s.pill, cells)
    });
    let header = ["Source", "Station", "Near", "Last", "Said"];
    let widths = [
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(14),
        Constraint::Length(10),
        Constraint::Fill(1),
    ];
    f.render_stateful_widget(pill_table(c.theme, &header, &widths, items), top, rows);
    let text = match rows.selected().and_then(|i| list.get(i)) {
        Some(s) => {
            let mut lines = vec![];
            if let Some(why) = &s.failing {
                lines.push(Line::styled(why.clone(), c.theme.bad()));
            }
            if let Some(t) = s.at {
                lines.push(Line::from(format!(
                    "{} {} at {} UTC: {}",
                    s.kind,
                    s.station,
                    clock(t),
                    s.said
                )));
            }
            Paragraph::new(lines)
        }
        None => Paragraph::new(Line::styled(
            "Stale: nothing for three of its usual gaps (10 min for APRS, an hour for NWS and \
             radar, 12 h for soundings). Down: the app can't read it now.",
            c.theme.mute(),
        )),
    };
    f.render_widget(text.wrap(Wrap { trim: false }), more);
}

fn new_page(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) {
    let o = c.o;
    if o.new_links.is_empty() {
        let why = "No new links. A link first heard at least a day after the app's first data is \
                   listed here for two weeks.";
        f.render_widget(Notice::Empty(why).paragraph(c.theme), area);
        return;
    }
    let mut head = vec![
        Line::from("First heard in the last two weeks, longest first."),
        Line::styled(
            "Long paths are the ones the marine layer bends. Study one: add it to links.",
            c.theme.mute(),
        ),
    ];
    if o.new_more > 0 {
        head.push(Line::styled(
            format!("And {} more, shorter: see /api/links.geojson.", o.new_more),
            c.theme.mute(),
        ));
    }
    let area = above(f, area, head);
    let items = o.new_links.iter().map(|l| {
        let mut length = vec![Span::raw(
            l.length_km.map_or("-".into(), |k| format!("{k:.1} km")),
        )];
        // as on the front page, the flag sits by the length
        if l.long {
            length.extend([Span::raw(" "), word_pill(c.theme, "LONG", Color::Yellow)]);
        }
        Row::new([
            Cell::from(link_name(o, &l.a, &l.b)),
            Cell::from(Line::from(length)),
            Cell::from(clock(l.first_ms)),
            Cell::from(if l.studied { "yes" } else { "no" }),
        ])
    });
    let widths = [
        Constraint::Fill(1),
        Constraint::Length(14),
        Constraint::Length(11),
        Constraint::Length(7),
    ];
    let header = Row::new(["Link", "Length", "First (UTC)", "Studied"]).style(c.theme.heading());
    let table = Table::new(items, widths)
        .header(header)
        .row_highlight_style(c.theme.selected());
    f.render_stateful_widget(table, area, rows);
}

/// The lines under a link's chart: each end now, the rain near it, its candidates.
fn chart_text(c: &Ctx, d: &LinkData) -> Vec<Line<'static>> {
    let o = c.o;
    let last = |s: &[(i64, f64)]| s.last().map(|p| p.1);
    let mut out: Vec<Line> = d
        .ends
        .iter()
        .map(|e| {
            Line::from(format!(
                "{} hears {}: {}, usual {}, residual {}",
                label(o, &e.node),
                label(o, &e.hears),
                dbm(last(&e.level_dbm)),
                dbm(last(&e.baseline_dbm)),
                db(last(&e.residual_db))
            ))
        })
        .collect();
    let spells: Vec<String> = wet_spells(&d.rain_hours)
        .into_iter()
        .map(|(a, b)| spell(a, b))
        .collect();
    let stations = d.rain_stations.join(", ");
    out.push(Line::styled(
        match (d.rain_stations.is_empty(), spells.is_empty()) {
            (true, _) => "No rain source is set for either end.".to_string(),
            (false, true) => format!("No rain reported near either end ({stations})."),
            (false, false) => format!("Rain near either end: {} ({stations}).", spells.join(", ")),
        },
        c.theme.mute(),
    ));
    for x in &d.candidates {
        let end = x.end().map_or("still going".into(), clock);
        out.push(Line::styled(
            format!("{}: {} to {end}", x.pill().1, clock(x.start())),
            c.theme.mute(),
        ));
    }
    out
}

fn chart_page(c: &Ctx, f: &mut Frame, area: Rect, p: &Pair, chart: Option<&Plot>) {
    let o = c.o;
    let studied = o.links.iter().find(|l| (&l.a, &l.b) == (&p.0, &p.1));
    let new = o.new_links.iter().find(|l| (&l.a, &l.b) == (&p.0, &p.1));
    let km = (studied.and_then(|l| l.length_km))
        .or(new.and_then(|l| l.length_km))
        .map_or(String::new(), |k| format!(", {k:.1} km"));
    let failed = chart.and_then(|ch| ch.err.as_deref());
    let under = match failed {
        Some(e) => Line::styled(
            format!(
                "Reading the chart failed ({}), so it is from earlier.",
                reason(e)
            ),
            c.theme.bad(),
        ),
        None => Line::styled(
            format!("Residual over the last {CHART_DAYS} days in dB, both ends averaged (UTC)."),
            c.theme.mute(),
        ),
    };
    let [head, rest] = Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]).areas(area);
    f.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!("{}{km}", link_name(o, &p.0, &p.1)),
                c.theme.heading(),
            ),
            under,
        ]),
        head,
    );
    let Some(d) = chart.and_then(|ch| ch.data.as_ref()) else {
        let n = failed.map_or(Notice::Loading, Notice::Error);
        return f.render_widget(n.paragraph(c.theme), rest);
    };
    let text = chart_text(c, d);
    let [plot, info] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(text.len() as u16 + 1),
    ])
    .areas(rest);
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), info);
    let pts = residual(d);
    if pts.is_empty() {
        let why = format!(
            "No baseline for these two days yet: every hour of day needs {} days of history first.",
            o.min_days
        );
        f.render_widget(Notice::Empty(&why).paragraph(c.theme), plot);
        return;
    }
    let lines = segments(&pts, 3 * d.bin_ms);
    let (x0, x1) = (d.from_ms as f64, d.to_ms as f64);
    let zero = [(x0, 0.0), (x1, 0.0)];
    let lim = pts
        .iter()
        .map(|p| p.1.abs())
        .fold(o.departure_db, f64::max)
        .ceil()
        + 1.0;
    let mut sets = vec![
        Dataset::default()
            .data(&zero)
            .graph_type(GraphType::Line)
            .marker(Marker::Braille)
            .style(c.theme.mute()),
    ];
    sets.extend(lines.iter().map(|s| {
        Dataset::default()
            .data(s)
            .graph_type(GraphType::Line)
            .marker(Marker::Braille)
    }));
    let chart = Chart::new(sets)
        .x_axis(
            Axis::default()
                .bounds([x0, x1])
                .labels([
                    clock(d.from_ms),
                    clock(d.from_ms / 2 + d.to_ms / 2),
                    clock(d.to_ms),
                ])
                .style(c.theme.mute()),
        )
        .y_axis(
            Axis::default()
                .bounds([-lim, lim])
                .labels([format!("{:+.0}", -lim), "0".into(), format!("{lim:+.0}")])
                .style(c.theme.mute()),
        );
    f.render_widget(chart, plot);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{Drop, Duct, Front, Wet};
    use crate::now::{Confidence, Now, State};
    use crate::series::DAY;
    use crate::web::{Gauge, NewLink, Problem, SoundingRow};
    use ridgewarden_client::tui::MIN_SIZE;
    use ridgewarden_client::tui::ratatui::Terminal;
    use ridgewarden_client::tui::ratatui::backend::TestBackend;
    use ridgewarden_client::tui::ratatui::buffer::Buffer;
    use ridgewarden_client::tui::ratatui::crossterm::event::KeyModifiers;
    use ridgewarden_client::tui::ratatui::style::{Color as Ansi, Modifier};
    use std::fmt::Write as _;

    const NOW: i64 = 1_795_000_000_000;
    /// 01:00 UTC on the day NOW falls in
    const H0: i64 = NOW - NOW % DAY + HOUR;

    fn end(node: &str, hears: &str, age: i64, residual: Option<f64>) -> EndRow {
        EndRow {
            node: node.into(),
            hears: hears.into(),
            samples: 1440,
            last_ms: NOW - age,
            every_ms: Some(MIN),
            baseline_days: 21,
            residual_db: residual,
        }
    }

    fn link(a: &str, b: &str, ends: Vec<EndRow>) -> LinkRow {
        LinkRow {
            a: a.into(),
            b: b.into(),
            length_km: Some(14.2),
            ends,
        }
    }

    fn gauge(station: &str, age: Option<i64>) -> Gauge {
        Gauge {
            station: station.into(),
            source: "aprs".into(),
            sites: vec!["tst-east".into()],
            at_ms: age.map(|a| NOW - a),
            said: age.map_or("", |_| "0.0 mm in the last hour").into(),
        }
    }

    fn wet() -> CandidateRow {
        let drop = Drop {
            a: "tst-east".into(),
            b: "tst-hub".into(),
            start: NOW - 20 * HOUR,
            end: Some(NOW - 16 * HOUR),
            last_low: NOW - 17 * HOUR,
            depth: 3.1,
        };
        CandidateRow {
            candidate: Candidate::Wet(Wet {
                site: "tst-east".into(),
                start: drop.start,
                end: drop.end,
                drops: vec![drop],
                site_links: 2,
                others: 3,
                others_dropping: 0,
            }),
            title: "Wet antenna? at East Knob".into(),
            summary: "Wet antenna? 1 of East Knob's 2 links dropped at 2026-11-17 15:06 UTC."
                .into(),
            evidence: vec!["Rain reported at N0CALL-13 (2.5 mm in the hour).".into()],
        }
    }

    fn overview() -> Overview {
        Overview {
            generated_ms: NOW,
            here: "tst-hub".into(),
            labels: [
                ("tst-hub", "Hub Peak"),
                ("tst-north", "North Ridge"),
                ("tst-east", "East Knob"),
            ]
            .map(|(n, l)| (n.to_string(), l.to_string()))
            .into(),
            analyzed_ms: Some(NOW - 4 * MIN),
            publish: true,
            publishing: true,
            departure_db: 4.0,
            min_days: 7,
            now: Now {
                state: State::Calm,
                confidence: Confidence::High,
                verdict: "Links are at their usual levels: no sign of ducting or fading.".into(),
                evidence: vec![
                    "At 2026-11-18 10:50 UTC, 0 of 5 links with data were 4 dB or more from \
                     their usual level (0 above, 0 below)."
                        .into(),
                ],
                bin_ms: Some(NOW - 20 * MIN),
                with_data: 5,
                departing: 0,
                up: 0,
                down: 0,
                sounding_station: None,
                sounding_ms: None,
                inversion_base_m: None,
                inversion_top_m: None,
                inversion_c: None,
                gradient_n_per_km: None,
            },
            links: vec![
                link(
                    "tst-hub",
                    "tst-north",
                    vec![
                        end("tst-hub", "tst-north", MIN, Some(-5.0)),
                        end("tst-north", "tst-hub", 2 * MIN, Some(-4.0)),
                    ],
                ),
                link(
                    "tst-east",
                    "tst-hub",
                    vec![
                        end("tst-east", "tst-hub", MIN, Some(0.5)),
                        // gone quiet: its old residual says nothing about now
                        end("tst-hub", "tst-east", 2 * HOUR, Some(-9.0)),
                    ],
                ),
                link(
                    "tst-east",
                    "tst-north",
                    vec![EndRow {
                        baseline_days: 3,
                        ..end("tst-east", "tst-north", MIN, None)
                    }],
                ),
            ],
            new_links: vec![NewLink {
                a: "tst-east".into(),
                b: "tst-north".into(),
                length_km: Some(24.1),
                first_ms: NOW - 3 * DAY,
                long: true,
                studied: true,
            }],
            new_more: 0,
            candidates: vec![wet()],
            gauges: vec![
                gauge("N0CALL-13", Some(3 * MIN)),
                gauge("N0CALL-11", Some(5 * HOUR)),
                gauge("N0CALL-12", None),
            ],
            radar_on: false,
            radar: None,
            soundings: vec![SoundingRow {
                station: "72393".into(),
                at_ms: Some(NOW - 9 * HOUR),
                summary: Some("inversion from 0 to 111 m above ground".into()),
            }],
            problems: vec![],
        }
    }

    fn empty() -> Overview {
        Overview {
            links: vec![],
            new_links: vec![],
            candidates: vec![],
            gauges: vec![],
            soundings: vec![],
            ..overview()
        }
    }

    fn view(o: Option<Overview>, err: Option<&str>) -> Top {
        let refresh = Refresh::spawn(Duration::from_secs(3600), async || Snap {
            got: Instant::now(),
            overview: Err("not read in this test".into()),
            link: None,
        })
        .unwrap();
        let mut t = Top::new(Theme { color: false }, refresh, Arc::default());
        // ages count on the app's clock: NOW, give or take the test's own run time
        t.data = o.map(|o| (Instant::now(), o));
        t.err = err.map(String::from);
        t
    }

    /// The text on a `w` by `h` terminal after one draw.
    fn screen(top: &mut Top, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| top.draw(f)).unwrap();
        let b = t.backend().buffer();
        let rows = (0..h).map(|y| (0..w).map(|x| b[(x, y)].symbol()).collect::<String>());
        rows.collect::<Vec<_>>().join("\n")
    }

    fn press(top: &mut Top, code: KeyCode) {
        assert!(top.key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn a_link_is_judged_by_its_fresh_ends_against_the_threshold() {
        let mut o = overview();
        o.links.push(link(
            "tst-hub",
            "tst-west",
            vec![end("tst-hub", "tst-west", MIN, Some(4.0))],
        ));
        o.links.push(link(
            "tst-hub",
            "tst-south",
            vec![end("tst-hub", "tst-south", FRESH + 1, Some(-8.0))],
        ));
        let got: Vec<(String, Level, Option<f64>)> = link_lines(&o, NOW)
            .into_iter()
            .map(|l| (l.name, l.level, l.residual))
            .collect();
        let want = [
            ("East Knob - Hub Peak", Level::Usual, Some(0.5)),
            ("East Knob - North Ridge", Level::Building, None),
            ("Hub Peak - North Ridge", Level::Below, Some(-4.5)),
            ("Hub Peak - tst-south", Level::Quiet, None),
            ("Hub Peak - tst-west", Level::Above, Some(4.0)),
        ]
        .map(|(n, l, r)| (n.to_string(), l, r));
        assert_eq!(got, want, "in name order, unlabelled nodes by name");
        let colors: Vec<Color> = [Level::Usual, Level::Below, Level::Above, Level::Quiet]
            .map(|l| l.pill().0)
            .to_vec();
        assert_eq!(
            colors,
            [Color::Green, Color::Yellow, Color::Yellow, Color::Unknown]
        );
    }

    #[test]
    fn sources_go_stale_after_three_of_their_gaps_and_down_while_failing() {
        let mut o = overview();
        o.gauges = vec![
            gauge("N0CALL-13", Some(30 * MIN)),
            gauge("N0CALL-11", Some(30 * MIN + 1)),
            gauge("N0CALL-12", None),
            Gauge {
                source: "nws".into(),
                ..gauge("KTST", Some(3 * HOUR))
            },
        ];
        o.radar_on = true;
        o.soundings[0].at_ms = Some(NOW - 37 * HOUR);
        let pills = |o: &Overview| -> Vec<(String, Option<Pill>)> {
            sources(o, NOW)
                .into_iter()
                .map(|s| (s.station, s.pill))
                .collect()
        };
        let want = |aprs: Option<Pill>| {
            [
                ("N0CALL-13", aprs.or(Some(Pill::Ok))),
                ("N0CALL-11", aprs.or(Some(Pill::Stale))),
                ("N0CALL-12", aprs),
                ("KTST", Some(Pill::Ok)),
                ("MRMS", None),
                ("72393", Some(Pill::Stale)),
            ]
            .map(|(s, p)| (s.to_string(), p))
            .to_vec()
        };
        assert_eq!(pills(&o), want(None));
        o.problems.push(Problem {
            source: "aprs".into(),
            at_ms: NOW - MIN,
            what: "APRS-IS 127.0.0.1:9: connection refused".into(),
        });
        assert_eq!(
            pills(&o),
            want(Some(Pill::Down)),
            "every APRS station is down with the APRS-IS connection, the rest aren't"
        );
        let s = sources(&o, NOW);
        assert_eq!(
            s[0].failing.as_deref(),
            Some("APRS-IS 127.0.0.1:9: connection refused")
        );
        assert_eq!(s[0].near, "East Knob");
        o.radar = Some(crate::web::RadarHour {
            at_ms: NOW - 50 * MIN,
            sites: 7,
            wet_sites: 2,
            max_mm: Some(3.0),
        });
        let radar = sources(&o, NOW).swap_remove(4);
        assert_eq!(
            (radar.pill, radar.near.as_str(), radar.said.as_str()),
            (
                Some(Pill::Ok),
                "7 sites",
                "rain at 2 of 7 sites, up to 3.0 mm in the hour"
            )
        );
    }

    #[test]
    fn keys_switch_pages_and_open_and_close_a_links_chart() {
        let mut t = view(Some(overview()), None);
        press(&mut t, KeyCode::Char('2'));
        assert_eq!(t.tab, 1);
        press(&mut t, KeyCode::Enter);
        assert_eq!(t.opened(), None, "nothing is picked yet");
        press(&mut t, KeyCode::Down);
        press(&mut t, KeyCode::Down);
        press(&mut t, KeyCode::Enter);
        assert_eq!(
            t.opened(),
            Some(("tst-east".to_string(), "tst-north".to_string())),
            "the second row in name order"
        );
        assert!(screen(&mut t, 80, 24).contains("East Knob - North Ridge"));
        press(&mut t, KeyCode::Esc);
        assert_eq!(t.opened(), None);
        assert_eq!(t.rows[1].selected(), Some(1), "back where it was");
        press(&mut t, KeyCode::Char('5'));
        press(&mut t, KeyCode::Down);
        press(&mut t, KeyCode::Enter);
        assert_eq!(
            t.opened(),
            Some(("tst-east".to_string(), "tst-north".to_string()))
        );
        press(&mut t, KeyCode::Tab);
        assert_eq!(
            (t.tab, t.opened()),
            (0, None),
            "a new page closes the chart"
        );
        press(&mut t, KeyCode::Char('4'));
        press(&mut t, KeyCode::End);
        assert_eq!(t.rows[3].selected(), Some(3), "three gauges and a sounding");
        press(&mut t, KeyCode::Enter);
        assert_eq!(t.opened(), None, "the rain sources have no chart");
    }

    #[test]
    fn every_page_fits_80_by_24() {
        let mut t = view(Some(overview()), None);
        let pages = [
            vec![
                "ridgewarden-weather (experimental)  tst-hub",
                " CALM   high confidence",
                "Links are at their usual levels",
                "Detection runs every 10 minutes, last 4 min ago.",
                " wet antenna?  11-17 15:06 11-17 19:06 Wet antenna? at East Knob",
                "q quit  1-5 Tab pages  r reload",
            ],
            vec![
                "3 studied links heard in the last day: 1 below, 1 usual, 1 building",
                " USUAL     East Knob - Hub Peak",
                " BUILDING  East Knob - North Ridge",
                "3 of 7 d",
                " BELOW     Hub Peak - North Ridge",
                "-4.5 dB",
            ],
            vec![
                "The last 7 days, newest first, from start to end (UTC).",
                " wet antenna?  11-17 15:06 11-17 19:06 Wet antenna? at East Knob",
                "Pick a candidate with the arrow keys",
            ],
            vec![
                "State     Source   Station    Near           Last       Said",
                " OK       aprs     N0CALL-13  East Knob      3 min ago",
                " STALE    aprs     N0CALL-11",
                "nothing heard yet",
            ],
            vec!["East Knob - North Ridge", "24.1 km  LONG"],
        ];
        for (tab, want) in pages.iter().enumerate() {
            t.tab = tab;
            let s = screen(&mut t, 80, 24);
            for w in want {
                assert!(s.contains(w), "page {tab} lacks {w:?}:\n{s}");
            }
            assert!(!screen(&mut t, MIN_SIZE.0, MIN_SIZE.1).contains("Make the terminal"));
        }
        t.tab = 2;
        press(&mut t, KeyCode::Down);
        let s = screen(&mut t, 80, 24);
        assert!(
            s.contains("East Knob - Hub Peak: down 3.1 dB at 11-17 15:06, back after 4 h"),
            "{s}"
        );
        assert!(s.contains("Rain reported at N0CALL-13"), "{s}");
        assert!(screen(&mut t, MIN_SIZE.0 - 1, MIN_SIZE.1).contains("Make the terminal at least"));
    }

    #[test]
    fn loading_empty_and_unreachable_each_say_so() {
        let mut t = view(None, None);
        let s = screen(&mut t, 80, 24);
        assert!(s.contains("Loading...") && s.contains("loading"), "{s}");
        let mut t = view(
            None,
            Some("http://127.0.0.1:8801/api/overview: could not connect"),
        );
        let s = screen(&mut t, 80, 24);
        assert!(s.contains("not answering"), "{s}");
        assert!(
            s.contains("Can't reach the app: http://127.0.0.1:8801/api/overview: could not"),
            "{s}"
        );
        let mut t = view(
            Some(empty()),
            Some("http://127.0.0.1:8801/api/overview: timed out"),
        );
        let pages = [
            "None. Detection starts once links have a baseline",
            "No RF link samples in the last 24 hours yet.",
            "No candidates in the last 7 days.",
            "No rain or sounding source is set up",
            "No new links.",
        ];
        for (tab, want) in pages.iter().enumerate() {
            t.tab = tab;
            let s = screen(&mut t, 80, 24);
            assert!(s.contains(want), "page {tab}:\n{s}");
            assert!(
                s.contains("Reading failed (timed out), so this is from earlier."),
                "the last good read stays, marked with the reason:\n{s}"
            );
            assert!(s.contains("not answering, updated 0 s ago"), "{s}");
        }
    }

    fn link_data() -> LinkData {
        let bins = |vals: &[(i64, f64)]| -> Vec<(i64, f64)> {
            vals.iter().map(|&(m, v)| (NOW - m * MIN, v)).collect()
        };
        LinkData {
            from_ms: NOW - 2 * DAY,
            to_ms: NOW,
            bin_ms: 10 * MIN,
            ends: vec![
                LinkEnd {
                    node: "tst-hub".into(),
                    hears: "tst-north".into(),
                    level_dbm: bins(&[(10, -73.0)]),
                    baseline_dbm: bins(&[(10, -70.0)]),
                    residual_db: bins(&[(30, -1.0), (20, -2.0), (10, -3.0)]),
                },
                LinkEnd {
                    node: "tst-north".into(),
                    hears: "tst-hub".into(),
                    level_dbm: bins(&[(10, -71.0)]),
                    baseline_dbm: bins(&[(10, -70.0)]),
                    residual_db: bins(&[(30, -3.0), (10, -1.0)]),
                },
            ],
            rain_stations: vec!["N0CALL-13".into()],
            rain_hours: [(0, true), (1, true), (2, false), (5, true)]
                .map(|(h, wet)| RainHour {
                    hour_ms: H0 + h * HOUR,
                    wet,
                })
                .to_vec(),
            candidates: vec![wet().candidate],
        }
    }

    #[test]
    fn a_chart_averages_both_ends_and_names_the_wet_spells() {
        let d = link_data();
        assert_eq!(
            residual(&d),
            [
                (NOW - 30 * MIN, -2.0),
                (NOW - 20 * MIN, -2.0),
                (NOW - 10 * MIN, -2.0)
            ]
        );
        let lines = segments(&[(0, 1.0), (10, 2.0), (20, 3.0), (60, 4.0)], 30);
        assert_eq!(
            lines,
            [
                vec![(0.0, 1.0), (10.0, 2.0), (20.0, 3.0)],
                vec![(60.0, 4.0)]
            ],
            "broken where 40 passes"
        );
        assert_eq!(
            wet_spells(&d.rain_hours),
            [(H0, H0 + 2 * HOUR), (H0 + 5 * HOUR, H0 + 6 * HOUR)]
        );
        assert_eq!(spell(H0, H0 + 2 * HOUR), "11-18 01:00 to 03:00");
        assert_eq!(
            spell(H0 + 22 * HOUR, H0 + 25 * HOUR),
            "11-18 23:00 to 11-19 02:00"
        );

        let mut t = view(Some(overview()), None);
        t.tab = 1;
        let p = ("tst-hub".to_string(), "tst-north".to_string());
        t.open_chart(p.clone());
        assert!(screen(&mut t, 80, 24).contains("Loading..."));
        let read = |link: Result<LinkData, String>| Snap {
            got: Instant::now(),
            overview: Ok(overview()),
            link: Some((p.clone(), link)),
        };
        t.take(read(Ok(d)));
        let s = screen(&mut t, 80, 24);
        for want in [
            "Hub Peak - North Ridge, 14.2 km",
            "Hub Peak hears North Ridge: -73.0 dBm, usual -70.0 dBm, residual -3.0 dB",
            "North Ridge hears Hub Peak: -71.0 dBm, usual -70.0 dBm, residual -1.0 dB",
            "Rain near either end: 11-18 01:00 to 03:00, 11-18 06:00 to 07:00 (N0CALL-13).",
            "wet antenna?: 11-17 15:06 to 11-17 19:06",
            "q quit  Esc back",
        ] {
            assert!(s.contains(want), "lacks {want:?}:\n{s}");
        }
        assert!(
            s.chars().any(|c| ('\u{2801}'..='\u{28ff}').contains(&c)),
            "the residual is drawn:\n{s}"
        );
        // a failed read keeps the chart on screen, marked
        t.take(read(Err(
            "http://127.0.0.1:8801/api/link?a=x: HTTP 503".into()
        )));
        let s = screen(&mut t, 80, 24);
        assert!(
            s.contains("Reading the chart failed (HTTP 503), so it is from earlier."),
            "{s}"
        );
        assert!(s.contains("Hub Peak hears North Ridge: -73.0 dBm"), "{s}");
        // with no good read before it, the error is all there is
        t.close_chart();
        t.open_chart(p.clone());
        t.take(read(Err(
            "http://127.0.0.1:8801/api/link?a=x: HTTP 404".into()
        )));
        assert!(
            screen(&mut t, 80, 24).contains("ERROR  http://127.0.0.1:8801/api/link?a=x: HTTP 404")
        );
    }

    #[test]
    fn a_chart_read_for_a_link_no_longer_open_is_dropped() {
        let mut t = view(Some(overview()), None);
        let p = ("tst-hub".to_string(), "tst-north".to_string());
        let snap = |p: &Pair| Snap {
            got: Instant::now(),
            overview: Ok(overview()),
            link: Some((p.clone(), Ok(link_data()))),
        };
        t.take(snap(&p));
        assert!(t.chart.is_none(), "no chart is open");
        t.open_chart(p.clone());
        t.take(snap(&("tst-east".to_string(), "tst-hub".to_string())));
        assert!(t.chart.is_none(), "another link's");
        t.take(snap(&p));
        assert!(t.chart.is_some());
        t.take(Snap {
            got: Instant::now(),
            overview: Err("timed out".into()),
            link: None,
        });
        assert!(
            t.data.is_some() && t.err.is_some(),
            "the last good read stays"
        );
    }

    fn duct() -> CandidateRow {
        let links = [
            ("tst-east", "tst-hub"),
            ("tst-east", "tst-north"),
            ("tst-hub", "tst-north"),
            ("tst-hub", "tst-west"),
        ]
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .to_vec();
        CandidateRow {
            candidate: Candidate::Duct(Duct {
                start: NOW - 2 * HOUR,
                end: None,
                last: NOW - HOUR,
                links,
                peak_at: NOW - 90 * MIN,
                peak_departing: 4,
                peak_with_data: 6,
                peak_up: 3,
                peak_down: 1,
                max_abs: 6.3,
            }),
            title: "Ducting or fading? on 4 links".into(),
            summary: "Ducting or fading? 4 of 6 links 4 dB or more from their usual level.".into(),
            evidence: vec!["Sounding 99999 at 2026-11-18 00:00 UTC: an inversion.".into()],
        }
    }

    fn front() -> CandidateRow {
        let drops: Vec<Drop> = ["tst-a", "tst-b", "tst-c", "tst-d", "tst-e"]
            .map(|n| Drop {
                a: "tst-hub".into(),
                b: n.into(),
                start: NOW - 3 * HOUR,
                end: None,
                last_low: NOW - HOUR,
                depth: 3.0,
            })
            .to_vec();
        CandidateRow {
            candidate: Candidate::Front(Front {
                start: NOW - 3 * HOUR,
                end: None,
                drops,
                sites: vec!["tst-hub".into()],
                with_data: 9,
            }),
            title: "Widespread drop? 5 links at 6 sites".into(),
            summary: "Widespread drop? 5 of 9 links at 6 sites dropped.".into(),
            evidence: vec!["Rain reported at N0CALL-13 (2.5 mm in the hour).".into()],
        }
    }

    #[test]
    fn a_selection_stays_on_its_item_between_reads() {
        let mut t = view(Some(overview()), None);
        let read = |o: Overview| Snap {
            got: Instant::now(),
            overview: Ok(o),
            link: None,
        };
        t.tab = 2;
        press(&mut t, KeyCode::Down);
        // a newer candidate arrives at the top
        let mut o = overview();
        o.candidates.insert(0, duct());
        t.take(read(o.clone()));
        assert_eq!(t.rows[2].selected(), Some(1), "still the wet antenna");
        let s = screen(&mut t, 80, 24);
        assert!(s.contains("East Knob - Hub Peak: down 3.1 dB"), "{s}");
        // a link that sorts in front of the selected one
        t.tab = 1;
        press(&mut t, KeyCode::End);
        assert_eq!(t.rows[1].selected(), Some(2), "Hub Peak - North Ridge");
        o.labels.insert("tst-apple".into(), "Apple Hill".into());
        o.links.push(link(
            "tst-apple",
            "tst-hub",
            vec![end("tst-apple", "tst-hub", MIN, Some(0.1))],
        ));
        t.take(read(o.clone()));
        assert_eq!(t.rows[1].selected(), Some(3), "it moved down one");
        // gone: the selection stays where it was, on whatever is there now
        o.candidates.remove(1);
        t.take(read(o));
        assert_eq!(t.rows[2].selected(), Some(0));
    }

    #[test]
    fn a_candidate_shows_its_evidence_first_and_counts_a_long_list() {
        let mut o = overview();
        o.candidates = vec![front(), duct(), wet()];
        let mut t = view(Some(o), None);
        t.tab = 2;
        press(&mut t, KeyCode::Down);
        let s = screen(&mut t, 160, 30);
        let at = |w: &str| s.find(w).unwrap_or_else(|| panic!("lacks {w:?}:\n{s}"));
        assert!(at("Rain reported at N0CALL-13") < at("Hub Peak - tst-a: down 3.0 dB"));
        assert!(at("Hub Peak - tst-c: down 3.0 dB") < at("and 2 more links"));
        assert!(!s.contains("tst-d: down"), "{s}");
        press(&mut t, KeyCode::Down);
        let s = screen(&mut t, 160, 30);
        let want = "3 up, 1 down at the peak, largest 6.3 dB. Links: East Knob - Hub Peak, East \
                    Knob - North Ridge, Hub Peak - North Ridge, and 1 more link.";
        assert!(s.contains(want), "{s}");
        let at = |w: &str| s.find(w).unwrap_or_else(|| panic!("lacks {w:?}:\n{s}"));
        assert!(at("Sounding 99999") < at("3 up, 1 down"));
    }

    #[test]
    fn each_kind_of_source_goes_stale_after_three_of_its_own_gaps() {
        // each kind's last report three of its gaps ago, and `late` more
        let rows = |late: i64| {
            let age = |gap: i64| 3 * gap + late;
            let mut o = overview();
            o.gauges = vec![
                gauge("N0CALL-13", Some(age(10 * MIN))),
                Gauge {
                    source: "nws".into(),
                    ..gauge("KTST", Some(age(HOUR)))
                },
            ];
            o.radar_on = true;
            o.radar = Some(crate::web::RadarHour {
                at_ms: NOW - age(HOUR),
                sites: 2,
                wet_sites: 0,
                max_mm: None,
            });
            o.soundings[0].at_ms = Some(NOW - age(12 * HOUR));
            sources(&o, NOW)
                .into_iter()
                .map(|s| (s.kind, s.pill))
                .collect::<Vec<_>>()
        };
        let all = |p: Pill| {
            ["aprs", "nws", "radar", "sounding"]
                .map(|k| (k.to_string(), Some(p)))
                .to_vec()
        };
        // 30 minutes for APRS, 3 hours for NWS and radar, 36 hours for soundings
        assert_eq!(rows(0), all(Pill::Ok));
        assert_eq!(rows(1), all(Pill::Stale));
    }

    #[test]
    fn the_now_page_says_how_many_candidates_it_leaves_out() {
        let mut o = overview();
        o.candidates = (0..7).map(|_| wet()).collect();
        let mut t = view(Some(o), None);
        let s = screen(&mut t, 80, 24);
        assert!(
            s.contains("Candidates in the last 7 days (UTC), newest 5 of 7"),
            "{s}"
        );
    }

    #[test]
    fn an_app_without_the_overview_is_called_older() {
        let mut t = view(None, Some("http://127.0.0.1:8801/api/overview: HTTP 404"));
        let s = screen(&mut t, 80, 24);
        assert!(s.contains("is older than top"), "{s}");
        assert!(!s.contains("Is it running"), "{s}");
    }

    #[test]
    fn r_and_shift_r_both_read_again() {
        let mut t = view(Some(overview()), None);
        let read = |t: &Top| {
            let until = Instant::now() + Duration::from_secs(10);
            while Instant::now() < until {
                if t.refresh.latest().unwrap().is_some() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            false
        };
        assert!(read(&t), "the first read");
        for k in ['r', 'R'] {
            press(&mut t, KeyCode::Char(k));
            assert!(read(&t), "{k} reads again");
        }
    }

    #[test]
    fn times_that_make_no_sense_dont_crash_the_view() {
        let mut o = overview();
        o.generated_ms = i64::MAX;
        o.links[0].ends[0].last_ms = i64::MIN;
        if let Candidate::Wet(w) = &mut o.candidates[0].candidate {
            w.drops[0].start = i64::MIN;
            w.drops[0].end = Some(i64::MAX);
        }
        let mut t = view(Some(o), None);
        t.rows[2].select(Some(0));
        for tab in 0..TABS.len() {
            t.tab = tab;
            screen(&mut t, 80, 24);
        }
    }

    #[test]
    fn times_and_levels_read_plainly() {
        assert_eq!(clock(NOW), "11-18 11:06");
        assert_eq!(ago(NOW, NOW - 3 * MIN), "3 min ago");
        assert_eq!(
            (db(Some(-4.04)), db(Some(0.26)), db(None)),
            ("-4.0 dB".into(), "+0.3 dB".into(), "-".into())
        );
        assert_eq!(dbm(Some(-70.26)), "-70.3 dBm");
        assert_eq!(
            counts(&[(2, "below"), (0, "above"), (7, "usual")]),
            "2 below, 7 usual"
        );
    }

    /// What the app served on the simulator, with three weeks of simulated history: its
    /// overview, and the East Knob - Hub Peak link's last two days.
    fn served() -> Top {
        let o: Overview =
            serde_json::from_str(include_str!("../tests/fixtures/top-overview.json")).unwrap();
        let d: LinkData =
            serde_json::from_str(include_str!("../tests/fixtures/top-link.json")).unwrap();
        let mut t = view(Some(o), None);
        t.chart = Some(Plot {
            pair: ("tst-east".into(), "tst-hub".into()),
            data: Some(d),
            err: None,
        });
        t
    }

    fn show_chart(t: &mut Top, open: bool) {
        *t.open.lock().unwrap() = open.then(|| ("tst-east".into(), "tst-hub".into()));
    }

    #[test]
    fn what_the_app_served_reads_at_80_by_24() {
        let mut t = served();
        let pages = [
            vec![
                " wet antenna?     11-23 03:00 11-23 08:30 Wet antenna? at East Knob",
                " ducting/fading?  11-22 03:10 11-22 05:20 Ducting or fading? on 4 links",
            ],
            vec![
                "9 studied links heard in the last day: 7 usual, 1 quiet, 1 building a baseline.",
                " BUILDING  Coast Hill - North Ridge              28.4",
                " USUAL     East Knob - Hub Peak                  14.3",
                // cut in the simulator over half an hour before
                " QUIET     East Knob - Valley Tower              23.1  60 s   ready     -",
            ],
            vec![],
            vec![" STALE    aprs     N0CALL-14  Coast Hill     5 h ago"],
        ];
        for (tab, want) in pages.iter().enumerate() {
            t.tab = tab;
            let s = screen(&mut t, 80, 24);
            for w in want {
                assert!(s.contains(w), "page {tab} lacks {w:?}:\n{s}");
            }
        }
        show_chart(&mut t, true);
        let s = screen(&mut t, 80, 24);
        for want in [
            "East Knob - Hub Peak, 14.3 km",
            "Rain near either end: 11-23 01:00 to 06:00 (N0CALL-11, N0CALL-13).",
            "wet antenna?: 11-23 03:00 to 11-23 08:30",
        ] {
            assert!(s.contains(want), "the chart lacks {want:?}:\n{s}");
        }
    }

    /// A drawn frame as an HTML page of colored monospace cells, as a dark terminal shows it.
    fn html(b: &Buffer) -> String {
        let css = |c: Ansi, default: &'static str| match c {
            Ansi::Black => "#0d1117",
            Ansi::Red => "#f85149",
            Ansi::Green => "#3fb950",
            Ansi::Yellow => "#d29922",
            Ansi::Gray => "#8b949e",
            Ansi::White => "#f0f6fc",
            _ => default,
        };
        let mut out = String::from(
            "<!doctype html><meta charset=utf-8><title>ridgewarden-weather top</title><style>\
             body{margin:0;background:#0d1117}pre{display:inline-block;margin:0;\
             padding:14px 18px;color:#c9d1d9;font:15px/1.35 'Cascadia Mono',Consolas,monospace}\
             i{display:inline-block;width:1ch;font-style:normal}</style><pre>",
        );
        for y in 0..b.area.height {
            for x in 0..b.area.width {
                let c = &b[(x, y)];
                let (mut fg, mut bg) = (css(c.fg, "#c9d1d9"), css(c.bg, "#0d1117"));
                if c.modifier.contains(Modifier::DIM) {
                    fg = "#7d8590";
                }
                if c.modifier.contains(Modifier::REVERSED) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let bold = if c.modifier.contains(Modifier::BOLD) {
                    ";font-weight:700"
                } else {
                    ""
                };
                let sym = match c.symbol() {
                    "&" => "&amp;",
                    "<" => "&lt;",
                    ">" => "&gt;",
                    s => s,
                };
                let _ = write!(
                    out,
                    "<i style=\"color:{fg};background:{bg}{bold}\">{sym}</i>"
                );
            }
            out.push('\n');
        }
        out + "</pre>"
    }

    /// The README's pictures of top: with README_SHOTS=1, each page drawn from what the app
    /// served, written as HTML to take a screenshot of.
    #[test]
    fn readme_pictures() {
        if std::env::var("README_SHOTS").as_deref() != Ok("1") {
            return;
        }
        let mut t = served();
        t.theme = Theme { color: true };
        let pages = [
            ("now", 0, false),
            ("links", 1, false),
            ("chart", 1, true),
            ("candidates", 2, false),
            ("rain", 3, false),
        ];
        for (name, tab, chart) in pages {
            t.tab = tab;
            show_chart(&mut t, chart);
            // East Knob - Hub Peak, the link the chart is of, and the newest candidate
            t.rows[tab].select(match tab {
                1 => Some(3),
                2 => Some(0),
                _ => None,
            });
            let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
            term.draw(|f| t.draw(f)).unwrap();
            let path = std::env::temp_dir().join(format!("ridgewarden-weather-top-{name}.html"));
            std::fs::write(&path, html(term.backend().buffer())).unwrap();
            println!("{}", path.display());
        }
    }

    #[tokio::test]
    async fn top_reads_what_the_app_serves() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: crate::config::Config = toml::from_str(&format!(
            "data_dir = {:?}\npublish = false\n[[site]]\nnode = \"tst-hub\"\nlabel = \"Hub Peak\"",
            dir.path()
        ))
        .unwrap();
        cfg.validate().unwrap();
        let app = crate::App::new(cfg).unwrap();
        let now = ridgewarden_client::util::now_ms();
        for (node, nb, lat) in [
            ("tst-hub", "tst-north", 35.0),
            ("tst-north", "tst-hub", 35.2),
        ] {
            let db = app.db();
            db.execute(
                "INSERT INTO polls (event_id, ts, author_key, node, ok, lat, lon) VALUES (?1, ?2, 'k', ?3, 1, ?4, -120.5)",
                rusqlite::params![node, now - MIN, node, lat],
            )
            .unwrap();
            db.execute(
                "INSERT INTO link_samples (event_id, ts, node, neighbor, link_type, signal, noise)
                 VALUES (?1, ?2, ?3, ?4, 'RF', -70, -95)",
                rusqlite::params![node, now - MIN, node, nb],
            )
            .unwrap();
        }
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let router = crate::web::router(Arc::new(app));
        tokio::spawn(async move { axum::serve(l, router).await });
        let mut loader = Loader {
            fetch: Fetch::new(Duration::from_secs(10)).unwrap(),
            base,
            open: Arc::default(),
        };
        let s = loader.load().await;
        let o = s.overview.unwrap();
        assert!(s.link.is_none());
        assert_eq!(o.links.len(), 1);
        let lines = link_lines(&o, o.generated_ms);
        assert_eq!(
            (lines[0].name.as_str(), lines[0].level),
            ("Hub Peak - tst-north", Level::Building)
        );
        let p = ("tst-hub".to_string(), "tst-north".to_string());
        *loader.open.lock().unwrap() = Some(p.clone());
        let s = loader.load().await;
        let (got, d) = s.link.unwrap();
        let d = d.unwrap();
        assert_eq!(got, p);
        assert_eq!((d.to_ms - d.from_ms, d.ends.len()), (CHART_DAYS * DAY, 2));
        assert_eq!(d.ends[0].level_dbm.len(), 1);
        *loader.open.lock().unwrap() = Some(("tst-hub".into(), "tst-gone".into()));
        let (_, d) = loader.load().await.link.unwrap();
        assert!(
            d.unwrap_err().ends_with("HTTP 404"),
            "no samples for that link"
        );
        let mut lost = Loader {
            base: format!("{}/gone", loader.base),
            ..loader
        };
        let s = lost.load().await;
        assert!(s.overview.unwrap_err().ends_with("HTTP 404"));
        assert!(s.link.is_none(), "no chart read when the overview failed");
    }
}
