//! `weather.toml`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ridgewarden_client::util::{norm_node, valid_node};
use ridgewarden_lightning::config::valid_call;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// the ridgewarden core on this box; it should be the hub that crawls the backbone
    #[serde(default = "d_core_url")]
    pub core_url: String,
    /// the core's `local.token`, for publishing candidates as signed events
    #[serde(default = "d_token")]
    pub token_file: PathBuf,
    /// publish candidates to the mesh as `weather.candidate` events; on at one site only
    #[serde(default = "d_true")]
    pub publish: bool,
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "d_listen")]
    pub listen: SocketAddr,
    /// raw link samples are kept this long; hourly medians are kept 400 days
    #[serde(default = "d_keep")]
    pub keep_days: u32,
    /// local standard time minus UTC, in hours, for saying when events happen
    #[serde(default)]
    pub utc_offset_h: i32,
    /// a gust at least this strong near a site, mph, flags its candidates: wind moving a
    /// dish or mount looks like weather in the path
    #[serde(default = "d_gust")]
    pub gust_mph: f64,
    /// links to study, as ["node-a", "node-b"] pairs; empty means every RF link
    #[serde(default)]
    pub links: Vec<[String; 2]>,
    #[serde(default)]
    pub baseline: BaselineCfg,
    #[serde(default)]
    pub wet: WetCfg,
    #[serde(default)]
    pub duct: DuctCfg,
    #[serde(default, rename = "site")]
    pub sites: Vec<SiteCfg>,
    #[serde(default)]
    pub aprs: Option<AprsCfg>,
    #[serde(default)]
    pub nws: Option<NwsCfg>,
    #[serde(default)]
    pub soundings: Option<SoundingCfg>,
    #[serde(default)]
    pub export: ExportCfg,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct BaselineCfg {
    /// the daily cycle is the median of each hour of day over this many days before
    #[serde(default = "d_21")]
    pub days: u32,
    /// days of data an hour of day needs before it has a baseline
    #[serde(default = "d_7")]
    pub min_days: u32,
    /// residuals are taken as medians over bins this long
    #[serde(default = "d_10")]
    pub bin_min: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct WetCfg {
    /// a link counts as dropping when it falls at least this far below where it was
    #[serde(default = "d_2f")]
    pub drop_db: f64,
    /// bigger drops are outages or obstructions, not water on a radome
    #[serde(default = "d_10f")]
    pub max_drop_db: f64,
    /// the level before a drop ends this long before it, so only a fast fall counts
    #[serde(default = "d_30")]
    pub onset_min: u32,
    /// "where it was" is the median over this long before the onset window
    #[serde(default = "d_60")]
    pub pre_min: u32,
    /// shorter dips are ignored
    #[serde(default = "d_20")]
    pub hold_min: u32,
    /// recovered once back within this of where it was
    #[serde(default = "d_1f")]
    pub recover_db: f64,
    /// a drop that hasn't recovered by then is a step change (realigned dish, new cable)
    #[serde(default = "d_12")]
    pub max_recover_h: u32,
    /// links at one site that must drop together
    #[serde(default = "d_2")]
    pub min_links: usize,
    /// and at least this share of that site's links with data
    #[serde(default = "d_half")]
    pub min_frac: f64,
    /// if more than this share of the other links drop at the same time, it's a widespread
    /// drop (a front, or something hitting the whole mesh), not one site
    #[serde(default = "d_half")]
    pub max_other_frac: f64,
    /// rain reports this long before the drop still count
    #[serde(default = "d_2h")]
    pub rain_before_h: u32,
    /// lightning strikes this close to the site, in the same window, are supporting evidence
    #[serde(default = "d_30f")]
    pub strike_km: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct DuctCfg {
    /// a link departs when its residual is at least this far from zero, either way
    #[serde(default = "d_4f")]
    pub departure_db: f64,
    /// links departing at once
    #[serde(default = "d_3")]
    pub min_links: usize,
    /// and at least this share of the links with data
    #[serde(default = "d_03")]
    pub min_frac: f64,
    /// a bin with fewer links reporting says too little about the mesh to count
    #[serde(default = "d_5")]
    pub min_with_data: usize,
    #[serde(default = "d_30")]
    pub min_duration_min: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct SiteCfg {
    pub node: String,
    #[serde(default)]
    pub label: Option<String>,
    /// APRS weather stations near the site (most are CWOP), e.g. ["CW0000"]
    #[serde(default)]
    pub aprs: Vec<String>,
    /// NWS observation stations near the site, e.g. ["KSBP"]
    #[serde(default)]
    pub nws: Vec<String>,
    /// the site's channel, for the export
    #[serde(default)]
    pub frequency_mhz: Option<f64>,
    /// antenna height above sea level, for the export
    #[serde(default)]
    pub antenna_alt_m: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct AprsCfg {
    /// APRS-IS server (host:port), read-only
    #[serde(default = "d_aprs")]
    pub server: String,
    /// callsign to log in with; read-only, so no passcode
    #[serde(default = "d_login")]
    pub login: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct NwsCfg {
    #[serde(default = "d_nws")]
    pub url: String,
    /// api.weather.gov refuses requests without one; name the app and a contact
    pub user_agent: String,
    #[serde(default = "d_30")]
    pub interval_min: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct SoundingCfg {
    /// the University of Wyoming sounding text list
    #[serde(default = "d_uwyo")]
    pub url: String,
    /// WMO station numbers, e.g. ["72393"] for Vandenberg
    pub stations: Vec<String>,
    /// only the lowest part of the sounding matters: the marine layer and the backbone
    /// sites are in the lowest kilometre or so
    #[serde(default = "d_1500")]
    pub max_height_m: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct ExportCfg {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub institution: Option<String>,
    #[serde(default)]
    pub licence: Option<String>,
    /// the mesh's channel, used for sites without their own frequency_mhz
    #[serde(default)]
    pub frequency_mhz: Option<f64>,
}

fn d_core_url() -> String {
    "http://127.0.0.1:8790".into()
}
fn d_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden-weather")
}
fn d_listen() -> SocketAddr {
    "0.0.0.0:8801".parse().unwrap()
}
fn d_keep() -> u32 {
    90
}
fn d_gust() -> f64 {
    30.0
}
fn d_token() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden/local.token")
}
fn d_true() -> bool {
    true
}
fn d_5() -> usize {
    5
}
fn d_30f() -> f64 {
    30.0
}
fn d_aprs() -> String {
    "rotate.aprs2.net:14580".into()
}
fn d_login() -> String {
    "N0CALL".into()
}
fn d_nws() -> String {
    "https://api.weather.gov".into()
}
fn d_uwyo() -> String {
    "https://weather.uwyo.edu/wsgi/sounding".into()
}
fn d_1500() -> f64 {
    1500.0
}
fn d_21() -> u32 {
    21
}
fn d_7() -> u32 {
    7
}
fn d_10() -> u32 {
    10
}
fn d_12() -> u32 {
    12
}
fn d_20() -> u32 {
    20
}
fn d_30() -> u32 {
    30
}
fn d_60() -> u32 {
    60
}
fn d_2() -> usize {
    2
}
fn d_2h() -> u32 {
    2
}
fn d_3() -> usize {
    3
}
fn d_1f() -> f64 {
    1.0
}
fn d_2f() -> f64 {
    2.0
}
fn d_4f() -> f64 {
    4.0
}
fn d_10f() -> f64 {
    10.0
}
fn d_half() -> f64 {
    0.5
}
fn d_03() -> f64 {
    0.3
}

impl Default for BaselineCfg {
    fn default() -> Self {
        Self {
            days: 21,
            min_days: 7,
            bin_min: 10,
        }
    }
}

impl Default for WetCfg {
    fn default() -> Self {
        Self {
            drop_db: 2.0,
            max_drop_db: 10.0,
            onset_min: 30,
            pre_min: 60,
            hold_min: 20,
            recover_db: 1.0,
            max_recover_h: 12,
            min_links: 2,
            min_frac: 0.5,
            max_other_frac: 0.5,
            rain_before_h: 2,
            strike_km: 30.0,
        }
    }
}

impl Default for DuctCfg {
    fn default() -> Self {
        Self {
            departure_db: 4.0,
            min_links: 3,
            min_frac: 0.3,
            min_with_data: 5,
            min_duration_min: 30,
        }
    }
}

fn positive(v: &[(&str, f64)]) -> Result<()> {
    for (name, x) in v {
        if !(x.is_finite() && *x > 0.0) {
            bail!("{name} must be above 0");
        }
    }
    Ok(())
}

fn fraction(name: &str, x: f64) -> Result<()> {
    if !(0.0..=1.0).contains(&x) {
        bail!("{name} is a share of links, between 0 and 1");
    }
    Ok(())
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Check the settings and normalize node names.
    pub fn validate(&mut self) -> Result<()> {
        if self.keep_days < 2 {
            bail!("keep_days must be at least 2");
        }
        positive(&[("gust_mph", self.gust_mph)])?;
        if !(-12..=14).contains(&self.utc_offset_h) {
            bail!("utc_offset_h must be between -12 and 14");
        }
        let b = &self.baseline;
        if b.days == 0 || b.min_days == 0 || b.min_days > b.days {
            bail!("baseline.min_days must be between 1 and baseline.days");
        }
        if !(1..=60).contains(&b.bin_min) {
            bail!("baseline.bin_min must be between 1 and 60");
        }
        let w = &self.wet;
        positive(&[
            ("wet.drop_db", w.drop_db),
            ("wet.max_drop_db", w.max_drop_db),
            ("wet.recover_db", w.recover_db),
        ])?;
        if w.max_drop_db <= w.drop_db || w.recover_db >= w.drop_db {
            bail!("wet thresholds need recover_db < drop_db < max_drop_db");
        }
        if !(1..=24).contains(&w.max_recover_h) {
            // a drop has to fit in the day of context each scan window keeps
            bail!("wet.max_recover_h must be between 1 and 24");
        }
        if w.onset_min == 0 || w.pre_min == 0 || w.hold_min == 0 || w.max_recover_h == 0 {
            bail!("the [wet] times must be above 0");
        }
        if w.min_links == 0 {
            bail!("wet.min_links must be at least 1");
        }
        fraction("wet.min_frac", w.min_frac)?;
        fraction("wet.max_other_frac", w.max_other_frac)?;
        positive(&[("wet.strike_km", w.strike_km)])?;
        let d = &self.duct;
        positive(&[("duct.departure_db", d.departure_db)])?;
        if d.min_with_data < d.min_links {
            bail!("duct.min_with_data can't be below duct.min_links");
        }
        fraction("duct.min_frac", d.min_frac)?;
        if d.min_links == 0 || d.min_duration_min == 0 {
            bail!("duct.min_links and duct.min_duration_min must be above 0");
        }
        for pair in &mut self.links {
            for n in pair.iter_mut() {
                *n = norm_node(n);
                if !valid_node(n) {
                    bail!("links: {n:?} is not a node name");
                }
            }
        }
        for s in &mut self.sites {
            s.node = norm_node(&s.node);
            if !valid_node(&s.node) {
                bail!("site {:?} is not a node name", s.node);
            }
            for c in &s.aprs {
                if !valid_call(c) {
                    bail!(
                        "site {}: APRS station {c:?} must be an upper-case callsign",
                        s.node
                    );
                }
            }
            for n in &s.nws {
                if !valid_nws(n) {
                    bail!(
                        "site {}: NWS station {n:?} must be an upper-case id like KSBP",
                        s.node
                    );
                }
            }
            if !s.aprs.is_empty() && self.aprs.is_none() {
                bail!(
                    "site {} lists APRS stations but there's no [aprs] section",
                    s.node
                );
            }
            if !s.nws.is_empty() && self.nws.is_none() {
                bail!(
                    "site {} lists NWS stations but there's no [nws] section",
                    s.node
                );
            }
            if s.frequency_mhz.is_some_and(|f| f.is_nan() || f <= 0.0) {
                bail!("site {}: frequency_mhz must be above 0", s.node);
            }
        }
        if let Some(a) = &self.aprs {
            if !a.server.contains(':') {
                bail!("aprs.server looks like host:port, e.g. rotate.aprs2.net:14580");
            }
            if !valid_call(&a.login) {
                bail!("aprs.login must be a callsign like N0CALL");
            }
        }
        if let Some(n) = &self.nws {
            if n.user_agent.trim().is_empty() {
                bail!(
                    "nws.user_agent is required by api.weather.gov, e.g. \"ridgewarden-weather (you@example.org)\""
                );
            }
            if n.interval_min < 10 {
                bail!("nws.interval_min must be at least 10");
            }
        }
        if let Some(s) = &self.soundings {
            if s.stations.is_empty() {
                bail!("[soundings] needs at least one station, e.g. stations = [\"72393\"]");
            }
            for id in &s.stations {
                if id.len() != 5 || !id.bytes().all(|b| b.is_ascii_digit()) {
                    bail!("sounding station {id:?} must be a 5-digit WMO number like 72393");
                }
            }
            positive(&[("soundings.max_height_m", s.max_height_m)])?;
        }
        if self
            .export
            .frequency_mhz
            .is_some_and(|f| f.is_nan() || f <= 0.0)
        {
            bail!("export.frequency_mhz must be above 0");
        }
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("weather.db")
    }

    /// How much of a sounding to look at, in m above the ground.
    pub fn sounding_height(&self) -> f64 {
        self.soundings.as_ref().map_or(d_1500(), |s| s.max_height_m)
    }

    pub fn site(&self, node: &str) -> Option<&SiteCfg> {
        self.sites.iter().find(|s| s.node == node)
    }

    pub fn label(&self, node: &str) -> String {
        self.site(node)
            .and_then(|s| s.label.clone())
            .unwrap_or_else(|| node.to_string())
    }

    /// Whether the link between `a` and `b` is one to study.
    pub fn studies(&self, a: &str, b: &str) -> bool {
        self.links.is_empty()
            || self
                .links
                .iter()
                .any(|[x, y]| (x == a && y == b) || (x == b && y == a))
    }

    /// The APRS and NWS stations set for any of `sites`.
    pub fn stations_near(&self, sites: &[String]) -> Vec<String> {
        let mut v: Vec<String> = sites
            .iter()
            .filter_map(|s| self.site(s))
            .flat_map(|s| s.aprs.iter().chain(&s.nws).cloned())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn aprs_stations(&self) -> Vec<String> {
        let mut v: Vec<String> = self.sites.iter().flat_map(|s| s.aprs.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn nws_stations(&self) -> Vec<String> {
        let mut v: Vec<String> = self.sites.iter().flat_map(|s| s.nws.clone()).collect();
        v.sort();
        v.dedup();
        v
    }
}

fn valid_nws(id: &str) -> bool {
    (3..=5).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

pub const EXAMPLE: &str = r#"# ridgewarden-weather configuration (experimental)
#
# Run this next to the hub core whose [crawl] polls the backbone nodes once a minute
# (interval_s = 60), agreed with the node owners. It works with slower polling too.
core_url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"
publish = true         # log candidates as signed weather.candidate events; one site only
data_dir = "/var/lib/ridgewarden-weather"
listen = "0.0.0.0:8801"
keep_days = 90         # raw samples; hourly medians are kept 400 days
utc_offset_h = -8      # local standard time, for saying when events happen
gust_mph = 30.0        # gusts this strong near a site flag its candidates (dish movement)

# links to study; leave empty for every RF link
links = [
  # ["tst-cuesta", "tst-madonna"],
]

[baseline]
days = 21              # daily cycle = median of each hour of day over this many days
min_days = 7           # days an hour of day needs before it has a baseline
bin_min = 10           # residuals are medians over bins this long

[wet]
drop_db = 2.0          # sudden fall that counts as a drop
max_drop_db = 10.0     # bigger is an outage or obstruction, not a wet radome
onset_min = 30         # the level before ends this long before the drop,
pre_min = 60           # and is the median over this long: a fall within about an hour counts
hold_min = 20          # shorter dips are ignored
recover_db = 1.0       # recovered when back within this of the level before
max_recover_h = 12     # not recovered by then: a step change, not water
min_links = 2          # links at one site dropping together
min_frac = 0.5         # and at least this share of that site's links
max_other_frac = 0.5   # more of the other links dropping at once is a widespread drop
rain_before_h = 2      # rain reports this long before the drop still count
strike_km = 30.0       # lightning strikes this close to the site support a wet candidate

[duct]
departure_db = 4.0     # residual this far from zero, either way
min_links = 3          # links departing at once
min_frac = 0.3         # and at least this share of the links with data
min_with_data = 5      # bins with fewer links reporting don't count
min_duration_min = 30

# One [[site]] per backbone site, with the weather stations near it.
# [[site]]
# node = "tst-cuesta"
# label = "Cuesta Peak"
# aprs = ["CW0000"]      # APRS weather stations (CWOP) heard over APRS-IS
# nws = ["KSBP"]         # NWS observation stations (api.weather.gov)
# frequency_mhz = 5745.0
# antenna_alt_m = 790.0

# Rain from APRS weather packets over APRS-IS, read-only.
# [aprs]
# server = "rotate.aprs2.net:14580"
# login = "N0CALL"

# Rain from NWS observations. api.weather.gov requires a User-Agent with a contact.
# [nws]
# url = "https://api.weather.gov"
# user_agent = "ridgewarden-weather (you@example.org)"
# interval_min = 30

# Upper-air soundings from the University of Wyoming, for the inversion.
# [soundings]
# url = "https://weather.uwyo.edu/wsgi/sounding"
# stations = ["72393"]   # Vandenberg; 72493 is Oakland
# max_height_m = 1500

# Global attributes of the OpenSense export.
[export]
# title = "AREDN backbone link levels"
# author = "N0CALL (you@example.org)"
# institution = ""
# licence = "CC-BY-4.0"
# frequency_mhz = 5745.0
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses_and_validates() {
        let mut c: Config = toml::from_str(EXAMPLE).unwrap();
        c.validate().unwrap();
        assert_eq!(c.listen.port(), 8801);
        assert_eq!(c.wet.min_links, 2);
        assert!(c.nws.is_none());
    }

    #[test]
    fn nws_needs_a_user_agent_and_sites_need_their_sources() {
        let mut c: Config = toml::from_str("[nws]\nuser_agent = \" \"").unwrap();
        assert!(c.validate().unwrap_err().to_string().contains("user_agent"));
        let mut c: Config =
            toml::from_str("[[site]]\nnode = \"TST-Cuesta.local.mesh\"\naprs = [\"CW0000\"]")
                .unwrap();
        assert!(c.validate().unwrap_err().to_string().contains("[aprs]"));
        let mut c: Config = toml::from_str(
            "[aprs]\n[[site]]\nnode = \"TST-Cuesta.local.mesh\"\naprs = [\"CW0000\"]",
        )
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.sites[0].node, "tst-cuesta");
    }
}
