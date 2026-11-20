//! Rain near each site: the ground truth a wet-antenna candidate is checked against.
//!
//! Reports come from APRS weather stations heard over APRS-IS (most are CWOP members;
//! the `r` field is rain in the last hour, parsed by ridgewarden-lightning's APRS code,
//! which also gives the wind and the peak gust kept with each reading)
//! and from NWS observations (`nws.rs`). Each report is kept, so an event can be checked
//! against the rain around it later.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ridgewarden_client::util::{now_ms, rfc3339_to_ms};
use ridgewarden_lightning::rain::parse_packet;
use ridgewarden_lightning::receiver::read_line;
use rusqlite::{Connection, params};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

use crate::App;
use crate::config::AprsCfg;
use crate::series::DAY;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS rain (
    source   TEXT NOT NULL,
    station  TEXT NOT NULL,
    ts       INTEGER NOT NULL,
    rain_1h  REAL,
    rain_24h REAL,
    text     TEXT,
    wind_dir REAL,
    wind_mph REAL,
    gust_mph REAL,
    -- logged to the mesh as weather.rain telemetry yet
    sent     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (source, station, ts)
);";

const KEEP: i64 = 400 * DAY;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Report {
    /// "aprs" or "nws"
    pub source: String,
    pub station: String,
    pub ts: i64,
    /// rain in the hour before `ts`
    pub rain_1h_mm: Option<f64>,
    /// rain in the 24 hours before `ts` (APRS only)
    pub rain_24h_mm: Option<f64>,
    /// the observer's words, e.g. "Light Rain" (NWS only)
    pub text: Option<String>,
    /// where the wind blows from, degrees true (APRS only)
    pub wind_dir_deg: Option<f64>,
    /// sustained wind and the peak gust in the last five minutes, mph (APRS only)
    pub wind_mph: Option<f64>,
    pub gust_mph: Option<f64>,
}

impl Report {
    /// Whether it says rain fell: Some(true) for rain, Some(false) for a measured zero
    /// with no rain in the text, None when it says nothing about rain.
    pub fn wet(&self) -> Option<bool> {
        let said = self.text.as_deref().map(|t| {
            let t = t.to_ascii_lowercase();
            ["rain", "drizzle", "shower", "thunderstorm"]
                .iter()
                .any(|w| t.contains(w))
        });
        match (self.rain_1h_mm, said) {
            (Some(mm), _) if mm > 0.0 => Some(true),
            (_, Some(true)) => Some(true),
            (Some(_), _) => Some(false),
            _ => None,
        }
    }
}

pub fn store(conn: &Connection, r: &Report) -> Result<()> {
    // a report fetched again unchanged isn't sent to the mesh again
    conn.execute(
        "INSERT INTO rain (source, station, ts, rain_1h, rain_24h, text, wind_dir, wind_mph, gust_mph)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(source, station, ts) DO UPDATE SET
             sent = CASE WHEN rain_1h IS excluded.rain_1h AND rain_24h IS excluded.rain_24h
                         AND text IS excluded.text AND gust_mph IS excluded.gust_mph
                         THEN sent ELSE 0 END,
             rain_1h = excluded.rain_1h, rain_24h = excluded.rain_24h, text = excluded.text,
             wind_dir = excluded.wind_dir, wind_mph = excluded.wind_mph,
             gust_mph = excluded.gust_mph",
        params![
            r.source,
            r.station,
            r.ts,
            r.rain_1h_mm,
            r.rain_24h_mm,
            r.text,
            r.wind_dir_deg,
            r.wind_mph,
            r.gust_mph
        ],
    )?;
    Ok(())
}

fn report(r: &rusqlite::Row) -> rusqlite::Result<Report> {
    Ok(Report {
        source: r.get(0)?,
        station: r.get(1)?,
        ts: r.get(2)?,
        rain_1h_mm: r.get(3)?,
        rain_24h_mm: r.get(4)?,
        text: r.get(5)?,
        wind_dir_deg: r.get(6)?,
        wind_mph: r.get(7)?,
        gust_mph: r.get(8)?,
    })
}

const COLS: &str = "source, station, ts, rain_1h, rain_24h, text, wind_dir, wind_mph, gust_mph";

/// The report with the strongest gust from `stations` in [from, to), if any had one.
pub fn strongest_gust(
    conn: &Connection,
    stations: &[String],
    from: i64,
    to: i64,
) -> Result<Option<Report>> {
    Ok(reports(conn, stations, from, to)?
        .into_iter()
        .filter(|r| r.gust_mph.is_some())
        .max_by(|a, b| {
            a.gust_mph
                .unwrap_or(0.0)
                .total_cmp(&b.gust_mph.unwrap_or(0.0))
        }))
}

/// Reports since `from` not yet logged to the mesh, oldest first.
pub fn unsent(conn: &Connection, from: i64, limit: usize) -> Result<Vec<Report>> {
    let mut st = conn.prepare(&format!(
        "SELECT {COLS} FROM rain WHERE sent = 0 AND ts >= ?1 ORDER BY ts LIMIT ?2"
    ))?;
    let rows = st.query_map(params![from, limit as i64], report)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Mark reports as logged, unless a fetch changed them meanwhile.
pub fn mark_sent(conn: &Connection, reports: &[Report]) -> Result<()> {
    for r in reports {
        conn.execute(
            "UPDATE rain SET sent = 1 WHERE source = ?1 AND station = ?2 AND ts = ?3
             AND rain_1h IS ?4 AND rain_24h IS ?5 AND text IS ?6",
            params![
                r.source,
                r.station,
                r.ts,
                r.rain_1h_mm,
                r.rain_24h_mm,
                r.text
            ],
        )?;
    }
    Ok(())
}

pub fn prune(conn: &Connection, now: i64) -> Result<()> {
    conn.execute("DELETE FROM rain WHERE ts < ?1", params![now - KEEP])?;
    Ok(())
}

/// Reports from `stations` in [from, to), oldest first.
pub fn reports(conn: &Connection, stations: &[String], from: i64, to: i64) -> Result<Vec<Report>> {
    let mut st = conn.prepare(&format!(
        "SELECT {COLS} FROM rain WHERE station = ?1 AND ts >= ?2 AND ts < ?3"
    ))?;
    let mut out = vec![];
    for s in stations {
        for r in st.query_map(params![s, from, to], report)? {
            out.push(r?);
        }
    }
    out.sort_by_key(|r| r.ts);
    Ok(out)
}

/// The newest report from each station.
pub fn latest(conn: &Connection) -> Result<Vec<Report>> {
    let mut st = conn.prepare(
        "SELECT source, station, max(ts), rain_1h, rain_24h, text, wind_dir, wind_mph, gust_mph
         FROM rain GROUP BY source, station ORDER BY source, station",
    )?;
    let rows = st.query_map([], report)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// the wettest report in the window
    Rain(Report),
    /// this many reports, none with rain
    Dry(usize),
    NoData,
}

impl Verdict {
    pub fn text(&self) -> String {
        match self {
            Verdict::Rain(r) => format!(
                "Rain reported at {} ({}{}{}).",
                r.station,
                r.rain_1h_mm
                    .map(|mm| format!("{mm:.1} mm in the hour"))
                    .unwrap_or_default(),
                if r.rain_1h_mm.is_some() && r.text.is_some() {
                    ", "
                } else {
                    ""
                },
                r.text.as_deref().unwrap_or("")
            ),
            Verdict::Dry(n) => format!(
                "No rain in {n} report{} from the nearby stations: probably not a wet antenna \
                 (dew, frost, or something at the radio).",
                if *n == 1 { "" } else { "s" }
            ),
            Verdict::NoData => "No rain reports from the nearby stations for that time, so it \
                                can't be checked."
                .into(),
        }
    }
}

/// What the reports say about rain.
pub fn verdict(reports: &[Report]) -> Verdict {
    let wet = reports
        .iter()
        .filter(|r| r.wet() == Some(true))
        .max_by(|a, b| {
            a.rain_1h_mm
                .unwrap_or(0.0)
                .total_cmp(&b.rain_1h_mm.unwrap_or(0.0))
        });
    if let Some(r) = wet {
        return Verdict::Rain(r.clone());
    }
    match reports.iter().filter(|r| r.wet() == Some(false)).count() {
        0 => Verdict::NoData,
        n => Verdict::Dry(n),
    }
}

/// An APRS report with a rain or gust figure, or None.
fn aprs_report(line: &str, ts: i64) -> Option<Report> {
    let (station, wx) = parse_packet(line)?;
    let any = wx.rain_1h_mm.or(wx.rain_24h_mm).or(wx.gust_mph).is_some();
    any.then(|| Report {
        source: "aprs".into(),
        station,
        ts,
        rain_1h_mm: wx.rain_1h_mm,
        rain_24h_mm: wx.rain_24h_mm,
        text: None,
        wind_dir_deg: wx.wind_dir_deg,
        wind_mph: wx.wind_mph,
        gust_mph: wx.gust_mph,
    })
}

/// Load an APRS log: one packet per line in TNC2 form, after its UTC receive time in
/// RFC 3339 (`2026-11-19T18:56:12Z CW0000>APRS,...`). Returns how many rain reports
/// were stored.
pub fn import_aprs(conn: &Connection, text: &str) -> Result<usize> {
    let mut n = 0;
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((ts, packet)) = line.split_once(' ') else {
            bail!("line {}: expected a time and a packet", i + 1);
        };
        let ts = rfc3339_to_ms(ts)
            .with_context(|| format!("line {}: {ts:?} is not an RFC 3339 time", i + 1))?;
        if let Some(r) = aprs_report(packet, ts) {
            store(conn, &r)?;
            n += 1;
        }
    }
    Ok(n)
}

async fn session(app: &App, cfg: &AprsCfg, stations: &[String]) -> Result<()> {
    let s = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::net::TcpStream::connect(&cfg.server),
    )
    .await
    .context("timed out connecting")??;
    let (r, mut w) = s.into_split();
    let login = format!(
        "user {} pass -1 vers ridgewarden-weather {} filter b/{}\r\n",
        cfg.login,
        env!("CARGO_PKG_VERSION"),
        stations.join("/")
    );
    w.write_all(login.as_bytes()).await?;
    let mut r = BufReader::new(r);
    let mut buf = vec![];
    app.ok("aprs");
    loop {
        // servers send a comment line every 20 s or so; silence means a dead connection.
        // A line is at most 512 bytes; reading through take() keeps a server that never
        // sends a newline from filling memory (the rest comes back as junk lines).
        let mut line = (&mut r).take(4096);
        let Some(l) =
            tokio::time::timeout(Duration::from_secs(300), read_line(&mut line, &mut buf))
                .await??
        else {
            bail!("server closed the connection");
        };
        if l.starts_with('#') {
            continue;
        }
        if let Some(rep) = aprs_report(&l, now_ms())
            && stations.contains(&rep.station)
        {
            store(&app.db(), &rep)?;
        }
    }
}

/// Stay connected to APRS-IS for the sites' weather stations, forever.
pub async fn run(app: Arc<App>) {
    let Some(cfg) = app.cfg.aprs.clone() else {
        return;
    };
    let stations = app.cfg.aprs_stations();
    if stations.is_empty() {
        return;
    }
    loop {
        if let Err(e) = session(&app, &cfg, &stations).await {
            app.failed("aprs", &format!("APRS-IS {}: {e:#}", cfg.server));
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_795_000_000_000;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c
    }

    fn rep(station: &str, ts: i64, mm: Option<f64>, text: Option<&str>) -> Report {
        Report {
            source: "nws".into(),
            station: station.into(),
            ts,
            rain_1h_mm: mm,
            rain_24h_mm: None,
            text: text.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn aprs_log_lines_become_rain_reports() {
        let c = db();
        let log = "# heard on APRS-IS\n\
            2026-11-19T18:56:12Z CW0000>APRS,TCPIP*,qAC,AMBCWOP-2:@191856z3523.07N/12052.00W_160/005g006t052r010p125P000h98b10184\n\
            2026-11-19T18:57:00Z N0CALL>APRS:!3523.07N/12052.00W>mobile\n\
            2026-11-19T19:56:12Z CW0000>APRS,TCPIP*,qAC,AMBCWOP-2:@191956z3523.07N/12052.00W_160/005g006t052r...p125P000h98b10184\n";
        assert_eq!(import_aprs(&c, log).unwrap(), 2);
        let r = reports(&c, &["CW0000".into()], 0, i64::MAX).unwrap();
        assert_eq!(r.len(), 2);
        assert!((r[0].rain_1h_mm.unwrap() - 2.54).abs() < 1e-9);
        assert!((r[0].rain_24h_mm.unwrap() - 31.75).abs() < 1e-9);
        // wind and gust ride along: 160/005g006
        assert_eq!(
            (r[0].wind_dir_deg, r[0].wind_mph, r[0].gust_mph),
            (Some(160.0), Some(5.0), Some(6.0))
        );
        assert_eq!(
            strongest_gust(&c, &["CW0000".into()], 0, i64::MAX)
                .unwrap()
                .unwrap()
                .gust_mph,
            Some(6.0)
        );
        // the second report has only the 24-hour figure
        assert_eq!(r[1].rain_1h_mm, None);
        assert_eq!(r[1].wet(), None);
        assert_eq!(r[0].ts, rfc3339_to_ms("2026-11-19T18:56:12Z").unwrap());
        assert!(import_aprs(&c, "yesterday CW0000>APRS:_").is_err());
    }

    #[test]
    fn each_report_goes_to_the_mesh_once_unless_it_changes() {
        let c = db();
        let r = rep("KSBP", T, Some(0.0), Some("Cloudy"));
        store(&c, &r).unwrap();
        let first = unsent(&c, 0, 10).unwrap();
        assert_eq!(first, vec![r.clone()]);
        mark_sent(&c, &first).unwrap();
        store(&c, &r).unwrap();
        assert!(
            unsent(&c, 0, 10).unwrap().is_empty(),
            "fetched again, unchanged"
        );
        let fixed = rep("KSBP", T, Some(0.3), Some("Light Rain"));
        store(&c, &fixed).unwrap();
        assert_eq!(unsent(&c, 0, 10).unwrap(), vec![fixed.clone()]);
        // revised again while that went out: the newer figure still goes
        let again = rep("KSBP", T, Some(0.5), Some("Light Rain"));
        store(&c, &again).unwrap();
        mark_sent(&c, &[fixed]).unwrap();
        assert_eq!(unsent(&c, 0, 10).unwrap(), vec![again]);
    }

    #[test]
    fn rain_beats_dry_and_silence_is_no_data() {
        assert_eq!(verdict(&[]), Verdict::NoData);
        let only_text = rep("KSBP", T, None, Some("Cloudy"));
        assert_eq!(verdict(std::slice::from_ref(&only_text)), Verdict::NoData);
        let dry = rep("KSBP", T, Some(0.0), Some("Cloudy"));
        assert_eq!(verdict(&[dry.clone(), only_text]), Verdict::Dry(1));
        // an NWS observer's "Light Rain" counts even when the gauge field is empty
        let said = rep("KSBP", T, None, Some("Light Rain"));
        let gauge = rep("CW0000", T, Some(1.3), None);
        assert_eq!(
            verdict(&[dry, said.clone(), gauge.clone()]),
            Verdict::Rain(gauge)
        );
        assert!(Verdict::Rain(said).text().contains("Light Rain"));
    }
}
