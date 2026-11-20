//! ridgewarden-weather (experimental): AREDN mesh links as a weather sensor.
//!
//! Runs next to the hub core that crawls the backbone, follows its feed for every node's
//! link levels, takes the daily cycle out of each link, and looks for two effects a 5 GHz
//! link can show: water on a site's antennas (a few dB on every link at that site, during
//! rain) and ducting or fading (large departures on many links at once, often at night
//! along the coast). It checks candidates against rain reports (APRS weather stations,
//! NWS observations) and upper-air soundings, and exports the link data in the OpenSense
//! conventions. It does not measure rain: at 5 GHz rain on the path costs far too little.

pub mod config;
pub mod detect;
pub mod events;
pub mod evidence;
pub mod export;
pub mod netcdf;
pub mod now;
pub mod nws;
pub mod publish;
pub mod rain;
pub mod results;
pub mod series;
pub mod sounding;
pub mod strikes;
pub mod web;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::Result;
use ridgewarden_client::core::Projection;
use ridgewarden_client::mesh::Mesh;
use ridgewarden_client::util::now_ms;
use ridgewarden_client::{CoreClient, open_db};
use rusqlite::Connection;

use crate::config::Config;

pub const PROJECTIONS: &[&dyn Projection] = &[&Mesh, &strikes::Strikes];

pub struct App {
    pub cfg: Config,
    pub core: CoreClient,
    pub db: Mutex<Connection>,
    /// whether candidates go out as signed events (publish is on and the token was read)
    pub publishing: bool,
    /// when the last analysis round finished
    pub analyzed: Mutex<Option<i64>>,
    /// the feed has been read to the end at least once
    caught_up: AtomicBool,
    /// for weather sources on the internet
    pub http: reqwest::Client,
    /// the last error from each outside source, cleared when it works again
    problems: Mutex<BTreeMap<String, (i64, String)>>,
    here: Mutex<Option<String>>,
}

/// Open the app's database with every table it uses.
pub fn open(cfg: &Config) -> Result<Connection> {
    let db = open_db(&cfg.db_path(), PROJECTIONS)?;
    db.execute_batch(series::SCHEMA)?;
    db.execute_batch(rain::SCHEMA)?;
    db.execute_batch(sounding::SCHEMA)?;
    db.execute_batch(events::SCHEMA)?;
    db.execute_batch(publish::SCHEMA)?;
    Ok(db)
}

impl App {
    pub fn new(cfg: Config) -> Result<Self> {
        let mut problems = BTreeMap::new();
        let with_token = if cfg.publish {
            CoreClient::new(&cfg.core_url, Some(&cfg.token_file))
                .map_err(|e| {
                    let why = format!("not publishing candidates: {e:#}");
                    tracing::warn!("{why}");
                    problems.insert("publish".to_string(), (now_ms(), why));
                })
                .ok()
        } else {
            None
        };
        let publishing = with_token.is_some();
        let core = match with_token {
            Some(c) => c,
            None => CoreClient::new(&cfg.core_url, None)?,
        };
        let db = open(&cfg)?;
        Ok(Self {
            cfg,
            core,
            db: Mutex::new(db),
            publishing,
            analyzed: Mutex::new(None),
            caught_up: AtomicBool::new(false),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .user_agent(concat!("ridgewarden-weather/", env!("CARGO_PKG_VERSION")))
                .build()?,
            problems: Mutex::new(problems),
            here: Mutex::new(None),
        })
    }

    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// This site's node name, or "" before the core has polled it.
    pub fn here(&self) -> String {
        self.here.lock().unwrap().clone().unwrap_or_default()
    }

    async fn learn_here(&self) {
        if self.here.lock().unwrap().is_some() {
            return;
        }
        if let Ok(w) = self.core.whoami().await
            && !w.node.is_empty()
        {
            *self.here.lock().unwrap() = Some(w.node);
        }
    }

    pub fn failed(&self, source: &str, why: &str) {
        tracing::warn!("{why}");
        self.problems
            .lock()
            .unwrap()
            .insert(source.to_string(), (now_ms(), why.to_string()));
    }

    pub fn ok(&self, source: &str) {
        self.problems.lock().unwrap().remove(source);
    }

    pub fn problems(&self) -> Vec<(i64, String)> {
        self.problems.lock().unwrap().values().cloned().collect()
    }
}

/// A response body, refused past `max` bytes: a misbehaving server mustn't fill memory.
pub async fn body(mut r: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    let mut out = vec![];
    while let Some(chunk) = r.chunk().await? {
        if out.len() + chunk.len() > max {
            anyhow::bail!("{} sent more than {max} bytes", r.url());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn analyze(cfg: &Config) -> Result<usize> {
    // its own connection, so a long round doesn't hold up the pages or the feed
    let conn = open(cfg)?;
    events::round(&conn, cfg, now_ms())
}

/// Follow the core, analyze every ten minutes, and fetch weather data, forever.
pub fn spawn_loops(app: Arc<App>) {
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            a.learn_here().await;
            match ridgewarden_client::follow(&a.core, &a.db, PROJECTIONS).await {
                Ok(_) => a.caught_up.store(true, Ordering::Release),
                Err(e) => tracing::warn!("following the core: {e:#}"),
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        // analyze what the core holds, not the first pages of it
        while !a.caught_up.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        loop {
            let cfg = a.cfg.clone();
            match tokio::task::spawn_blocking(move || analyze(&cfg)).await {
                Ok(Ok(n)) => {
                    tracing::debug!("analysis: {n} recent candidates");
                    a.ok("analysis");
                    *a.analyzed.lock().unwrap() = Some(now_ms());
                }
                Ok(Err(e)) => a.failed("analysis", &format!("analysis: {e:#}")),
                Err(e) => a.failed("analysis", &format!("analysis task: {e}")),
            }
            if a.publishing {
                match publish::round(&a).await {
                    Ok(0) => a.ok("publish"),
                    Ok(n) => {
                        tracing::info!("published {n} events to the mesh");
                        a.ok("publish");
                    }
                    Err(e) => a.failed("publish", &format!("publishing candidates: {e:#}")),
                }
            }
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    });
    tokio::spawn(rain::run(app.clone()));
    tokio::spawn(nws::run(app.clone()));
    tokio::spawn(sounding::run(app));
}
