use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use ridgewarden_client::util::now_ms;

use ridgewarden_weather::config::{Config, EXAMPLE};
use ridgewarden_weather::series::DAY;
use ridgewarden_weather::{App, export, nws, open, rain, sounding, spawn_loops, top, web};

#[derive(Parser)]
#[command(
    version,
    about = "Experimental: AREDN mesh links as a weather sensor (a ridgewarden app)"
)]
struct Cli {
    #[arg(
        long,
        short,
        global = true,
        default_value = "/etc/ridgewarden/weather.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    /// a University of Wyoming sounding text list page
    Sounding,
    /// an api.weather.gov observations response (JSON)
    Nws,
    /// an APRS log: "<RFC 3339 time> <TNC2 packet>" per line
    Aprs,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a starter config
    Init,
    /// Follow the core, fetch weather data, analyze, and serve the pages
    Run,
    /// Load weather data from a file, e.g. a saved sounding or an APRS log
    Import {
        kind: Kind,
        file: PathBuf,
        /// the NWS station the observations are from
        #[arg(long)]
        station: Option<String>,
    },
    /// Write the link data in the OpenSense conventions as a NetCDF file
    Export {
        #[arg(long, default_value_t = 30)]
        days: u32,
        /// seconds per time step
        #[arg(long, default_value_t = 60)]
        step: u32,
        #[arg(long)]
        out: PathBuf,
    },
    /// Watch it live in the terminal: the marine layer, each studied link against its
    /// baseline, candidates, rain sources and new links
    Top {
        /// where the app answers; reads its API, not the config
        #[arg(long, default_value = "http://127.0.0.1:8801")]
        url: String,
        /// seconds between reads
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=600))]
        every: u64,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // top draws the whole terminal, so a log line would land in the middle of it
    if !matches!(cli.cmd, Cmd::Top { .. }) {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "ridgewarden_weather=info,ridgewarden_client=info".into()),
            )
            .init();
    }
    match cli.cmd {
        Cmd::Top { url, every } => top::run(&url, Duration::from_secs(every)),
        Cmd::Init => {
            if cli.config.exists() {
                bail!(
                    "{} already exists; not overwriting it",
                    cli.config.display()
                );
            }
            if let Some(d) = cli.config.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&cli.config, EXAMPLE)?;
            println!("Wrote {}", cli.config.display());
            Ok(())
        }
        Cmd::Run => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?.block_on(run(cfg))
        }
        Cmd::Import {
            kind,
            file,
            station,
        } => {
            let cfg = Config::load(&cli.config)?;
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let conn = open(&cfg)?;
            match kind {
                Kind::Sounding => {
                    let s = sounding::parse(&text)?;
                    sounding::store(&conn, &s)?;
                    println!(
                        "Stored sounding {} at {}: {}",
                        s.station,
                        ridgewarden_client::util::short_time(s.ts),
                        s.summary(cfg.sounding_height())
                    );
                }
                Kind::Nws => {
                    let Some(station) = station else {
                        bail!("say which station with --station, e.g. --station KSBP");
                    };
                    let reports = nws::parse(&serde_json::from_str(&text)?, &station);
                    for r in &reports {
                        rain::store(&conn, r)?;
                    }
                    println!("Stored {} observations from {station}", reports.len());
                }
                Kind::Aprs => {
                    println!("Stored {} rain reports", rain::import_aprs(&conn, &text)?);
                }
            }
            Ok(())
        }
        Cmd::Export { days, step, out } => {
            if !(1..=3600).contains(&step) || days == 0 {
                bail!("--step is 1 to 3600 seconds and --days at least 1");
            }
            let cfg = Config::load(&cli.config)?;
            let conn = open(&cfg)?;
            let to = now_ms();
            let from = to - days as i64 * DAY;
            let bytes = export::build(&conn, &cfg, from, to, step as i64)?.write()?;
            std::fs::write(&out, &bytes).with_context(|| format!("writing {}", out.display()))?;
            println!("Wrote {} ({} bytes)", out.display(), bytes.len());
            Ok(())
        }
    }
}

async fn run(cfg: Config) -> Result<()> {
    let listen = cfg.listen;
    let app = Arc::new(App::new(cfg)?);
    tracing::info!(
        "ridgewarden-weather {} (experimental) following {}, page on http://{listen}",
        env!("CARGO_PKG_VERSION"),
        app.core.base()
    );
    spawn_loops(app.clone());
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
