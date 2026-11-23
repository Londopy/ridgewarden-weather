//! The dataset in the OpenSense CML conventions
//! (github.com/OpenSenseAction/OS_data_format_conventions, netCDF_CML.adoc and
//! netCDF_global_attributes.adoc), written as a NetCDF classic file.
//!
//! Each RF link is a CML (`cml_id` "node-a|node-b", names in order). `sublink_1` runs
//! from site 0 to site 1, so its RSL is what site 1's node reports hearing from site 0;
//! `sublink_2` is the other way. The time axis is regular, `step` seconds apart; each
//! value is the last sample in (time - step, time], as the node reported it, so
//! `sampling` is "instantaneous". The core's polls don't carry transmit power, so there
//! is no `tsl` (the conventions allow leaving out a level that is constant, and transmit
//! power is a fixed setting). The noise floor and SNR the nodes report come along as
//! extra variables, and so does `channel_width`, from the config like `frequency`.
//! Polarisation is unknown (many AREDN radios are dual-polarised) and left out.

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use ridgewarden_client::graph::edge;
use ridgewarden_client::util::{ms_to_rfc3339, short_time};
use ridgewarden_lightning::geo::{LatLon, distance_km};
use rusqlite::Connection;

use crate::config::Config;
use crate::detect::Pair;
use crate::netcdf::{Attr, FILL_DOUBLE, FILL_FLOAT, File, Values, Var};
use crate::series;

/// Cells per signal variable; past this the export would need hundreds of MB of memory.
pub const MAX_CELLS: usize = 8_000_000;

fn text(s: &str) -> Attr {
    Attr::Text(s.to_string())
}

fn chars(names: &[String]) -> (usize, Vec<u8>) {
    let width = names.iter().map(String::len).max().unwrap_or(1).max(1);
    let mut out = Vec::with_capacity(width * names.len());
    for n in names {
        out.extend_from_slice(n.as_bytes());
        out.resize(out.len() + width - n.len(), 0);
    }
    (width, out)
}

fn coord(name: &str, dims: Vec<usize>, units: &str, long: &str, v: Vec<f64>) -> Var {
    Var {
        name: name.into(),
        dims,
        attrs: vec![
            ("units".into(), text(units)),
            ("long_name".into(), text(long)),
            ("_FillValue".into(), Attr::Double(FILL_DOUBLE)),
        ],
        values: Values::Double(v),
    }
}

fn level(name: &str, units: &str, long: &str, v: Vec<f32>) -> Var {
    Var {
        name: name.into(),
        dims: vec![1, 3, 0],
        attrs: vec![
            ("units".into(), text(units)),
            (
                "coordinates".into(),
                text("site_0_lat site_0_lon site_1_lat site_1_lon"),
            ),
            ("long_name".into(), text(long)),
            ("sampling".into(), text("instantaneous")),
            ("_FillValue".into(), Attr::Float(FILL_FLOAT)),
        ],
        values: Values::Float(v),
    }
}

const COMMENT: &str = "Link levels from an AREDN amateur-radio mesh (5 GHz), as each node reports \
them in its sysinfo link data, polled by ridgewarden cores. sublink_1 is site_0 to site_1 (the \
RSL is reported by the node at site_1), sublink_2 is site_1 to site_0. Each value is the last \
sample in (time - step, time]. There is no tsl: the polls do not carry transmit power, which \
is a fixed setting. noise_level and snr are the receiving node's noise floor and \
signal-to-noise ratio. frequency and channel_width (an extra variable, the RF channel width) \
come from the station's configuration, not from the nodes. Polarisation is not known and is \
left out. Missing values are the NetCDF default fill.";

/// The OpenSense dataset for [from, to), `step_s` seconds per time step.
pub fn build(conn: &Connection, cfg: &Config, from: i64, to: i64, step_s: i64) -> Result<File> {
    let step = step_s * 1000;
    let from = from - from.rem_euclid(step);
    let nt = ((to - from) / step).max(1) as usize;
    let last = from + (nt as i64 - 1) * step;
    let samples = series::all_samples(conn, from - step + 1, last + 1)?;
    let cmls: Vec<Pair> = samples
        .keys()
        .filter(|(a, b)| cfg.studies(a, b))
        .map(|(a, b)| edge(a, b))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if cmls.is_empty() {
        bail!(
            "no RF link samples between {} and {}",
            short_time(from),
            short_time(to)
        );
    }
    let cells = cmls.len() * 2 * nt;
    if cells > MAX_CELLS {
        bail!(
            "{} links x 2 directions x {nt} steps is too many values for one export; \
             use fewer days or a longer step",
            cmls.len()
        );
    }
    let mut rsl = vec![FILL_FLOAT; cells];
    let mut noise = vec![FILL_FLOAT; cells];
    let mut snr = vec![FILL_FLOAT; cells];
    for (c, (a, b)) in cmls.iter().enumerate() {
        // sublink_1 is heard at b, sublink_2 at a
        for (s, end) in [(b.clone(), a.clone()), (a.clone(), b.clone())]
            .iter()
            .enumerate()
        {
            for x in samples.get(end).into_iter().flatten() {
                let k = (x.ts - from + step - 1).div_euclid(step) as usize;
                let i = (c * 2 + s) * nt + k;
                rsl[i] = x.signal as f32;
                noise[i] = x.noise.map_or(FILL_FLOAT, |v| v as f32);
                snr[i] = x.snr.map_or(FILL_FLOAT, |v| v as f32);
            }
        }
    }
    let pos = crate::evidence::positions(conn, cmls.iter().flat_map(|(a, b)| [a, b]))?;
    let site = |n: &str, f: fn(LatLon) -> f64| pos.get(n).copied().map_or(FILL_DOUBLE, f);
    let alt = |n: &str| {
        cfg.site(n)
            .and_then(|s| s.antenna_alt_m)
            .unwrap_or(FILL_DOUBLE)
    };
    let freq = |n: &str| {
        cfg.site(n)
            .and_then(|s| s.frequency_mhz)
            .or(cfg.export.frequency_mhz)
            .unwrap_or(FILL_DOUBLE)
    };
    let width = |n: &str| {
        cfg.site(n)
            .and_then(|s| s.channel_width_mhz)
            .or(cfg.export.channel_width_mhz)
            .unwrap_or(FILL_DOUBLE)
    };
    let col = |f: &dyn Fn(&Pair) -> f64| -> Vec<f64> { cmls.iter().map(f).collect() };
    let ids: Vec<String> = cmls.iter().map(|(a, b)| format!("{a}|{b}")).collect();
    let (id_w, id_chars) = chars(&ids);
    let subs = ["sublink_1".to_string(), "sublink_2".to_string()];
    let (sub_w, sub_chars) = chars(&subs);
    let utf8 = ("_Encoding".to_string(), text("utf-8"));

    let now = ridgewarden_client::util::now_ms();
    let mut attrs = vec![
        (
            "title".into(),
            text(
                cfg.export
                    .title
                    .as_deref()
                    .unwrap_or("AREDN mesh link levels"),
            ),
        ),
        (
            "file_author(s)".into(),
            text(cfg.export.author.as_deref().unwrap_or("")),
        ),
        (
            "institution".into(),
            text(cfg.export.institution.as_deref().unwrap_or("")),
        ),
        ("date".into(), text(&ms_to_rfc3339(now))),
        (
            "source".into(),
            text(
                "AREDN node link statistics (received signal, noise floor, SNR) polled over the mesh",
            ),
        ),
        ("version".into(), text("1")),
        (
            "history".into(),
            text(&format!(
                "{} written by ridgewarden-weather {} from {} to {}, step {step_s} s",
                ms_to_rfc3339(now),
                env!("CARGO_PKG_VERSION"),
                ms_to_rfc3339(from),
                ms_to_rfc3339(last)
            )),
        ),
        ("naming convention".into(), text("OpenSense-CML")),
    ];
    if let Some(l) = &cfg.export.licence {
        attrs.push(("licence".into(), text(l)));
    }
    attrs.push((
        "reference".into(),
        text("https://github.com/Londopy/ridgewarden-weather"),
    ));
    attrs.push(("comment".into(), text(COMMENT)));

    Ok(File {
        dims: vec![
            ("time".into(), nt),
            ("cml_id".into(), cmls.len()),
            ("cml_id_strlen".into(), id_w),
            ("sublink_id".into(), 2),
            ("sublink_id_strlen".into(), sub_w),
        ],
        attrs,
        vars: vec![
            Var {
                name: "time".into(),
                dims: vec![0],
                attrs: vec![
                    (
                        "units".into(),
                        text("seconds since 1970-01-01 00:00:00 UTC"),
                    ),
                    ("long_name".into(), text("time_utc")),
                    // OpenSense lists one; the axis is regular, so no value is ever missing
                    ("_FillValue".into(), Attr::Double(FILL_DOUBLE)),
                ],
                values: Values::Double(
                    (0..nt)
                        .map(|k| (from + k as i64 * step) as f64 / 1000.0)
                        .collect(),
                ),
            },
            Var {
                name: "cml_id".into(),
                dims: vec![1, 2],
                attrs: vec![
                    (
                        "long_name".into(),
                        text("commercial_microwave_link_identifier"),
                    ),
                    utf8.clone(),
                ],
                values: Values::Char(id_chars),
            },
            Var {
                name: "sublink_id".into(),
                dims: vec![3, 4],
                attrs: vec![("long_name".into(), text("sublink_identifier")), utf8],
                values: Values::Char(sub_chars),
            },
            coord(
                "site_0_lat",
                vec![1],
                "degrees_in_WGS84_projection",
                "site_0_latitude",
                col(&|p| site(&p.0, |x| x.lat)),
            ),
            coord(
                "site_0_lon",
                vec![1],
                "degrees_in_WGS84_projection",
                "site_0_longitude",
                col(&|p| site(&p.0, |x| x.lon)),
            ),
            coord(
                "site_0_alt",
                vec![1],
                "metres_above_sea",
                "antenna_altitude_above_sea_level_at_site_0",
                col(&|p| alt(&p.0)),
            ),
            coord(
                "site_1_lat",
                vec![1],
                "degrees_in_WGS84_projection",
                "site_1_latitude",
                col(&|p| site(&p.1, |x| x.lat)),
            ),
            coord(
                "site_1_lon",
                vec![1],
                "degrees_in_WGS84_projection",
                "site_1_longitude",
                col(&|p| site(&p.1, |x| x.lon)),
            ),
            coord(
                "site_1_alt",
                vec![1],
                "metres_above_sea",
                "antenna_altitude_above_sea_level_at_site_1",
                col(&|p| alt(&p.1)),
            ),
            coord(
                "length",
                vec![1],
                "m",
                "distance_between_pair_of_antennas",
                col(&|p| match (pos.get(&p.0), pos.get(&p.1)) {
                    (Some(x), Some(y)) => distance_km(*x, *y) * 1000.0,
                    _ => FILL_DOUBLE,
                }),
            ),
            coord(
                "frequency",
                vec![1, 3],
                "MHz",
                "sublink_frequency",
                cmls.iter().flat_map(|p| [freq(&p.0), freq(&p.1)]).collect(),
            ),
            coord(
                "channel_width",
                vec![1, 3],
                "MHz",
                "sublink_channel_width",
                cmls.iter()
                    .flat_map(|p| [width(&p.0), width(&p.1)])
                    .collect(),
            ),
            level("rsl", "dBm", "received_signal_level", rsl),
            level(
                "noise_level",
                "dBm",
                "noise_floor_reported_by_the_receiving_node",
                noise,
            ),
            level(
                "snr",
                "dB",
                "signal_to_noise_ratio_reported_by_the_receiving_node",
                snr,
            ),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netcdf::read;
    use crate::series::{DAY, MIN};
    use ridgewarden_client::core::Projection;
    use ridgewarden_client::mesh::Mesh;
    use ridgewarden_client::mesh::testing::Fake;
    use serde_json::json;

    const T0: i64 = 1_795_000_000_000 - 1_795_000_000_000 % DAY;

    fn var<'a>(f: &'a File, n: &str) -> &'a Var {
        f.vars.iter().find(|v| v.name == n).unwrap()
    }

    fn attr<'a>(v: &'a Var, n: &str) -> Option<&'a Attr> {
        v.attrs.iter().find(|a| a.0 == n).map(|a| &a.1)
    }

    /// Two nodes that hear each other, one poll each.
    fn two_nodes() -> (Fake, Config) {
        let mut f = Fake::new(vec![Box::new(Mesh) as Box<dyn Projection>]);
        let mut cfg: Config = toml::from_str(
            "[[site]]\nnode = \"tst-b\"\nfrequency_mhz = 5745.0\nantenna_alt_m = 700.0\n\
             channel_width_mhz = 20.0\n\
             [export]\nfrequency_mhz = 5805.0\nchannel_width_mhz = 10.0\nlicence = \"CC-BY-4.0\"",
        )
        .unwrap();
        cfg.validate().unwrap();
        f.add(
            "node.poll",
            json!({"node": "TST-A", "ok": true, "lat": 35.0, "lon": -120.0, "links": [
                {"neighbor": "TST-B", "type": "RF", "signal": -61.0, "noise": -95.0, "snr": 34.0}]}),
            T0 + 30_000,
        );
        f.add(
            "node.poll",
            json!({"node": "TST-B", "ok": true, "lat": 35.1, "lon": -120.0, "links": [
                {"neighbor": "TST-A", "type": "RF", "signal": -64.0, "noise": -96.0, "snr": 32.0}]}),
            T0 + 2 * MIN,
        );
        (f, cfg)
    }

    #[test]
    fn exports_both_directions_of_each_link_in_opensense_layout() {
        let (f, cfg) = two_nodes();
        let file = build(&f.conn, &cfg, T0, T0 + 3 * MIN, 60).unwrap();
        let back = read::file(&file.write().unwrap());
        assert_eq!(back, file);
        let dim = |n: &str| back.dims.iter().position(|d| d.0 == n).unwrap();
        let rsl = var(&back, "rsl");
        assert_eq!(
            rsl.dims,
            vec![dim("cml_id"), dim("sublink_id"), dim("time")]
        );
        let Values::Float(v) = &rsl.values else {
            panic!()
        };
        // sublink_1 (a to b) is heard at b, as of the third step; sublink_2 at a, as of the second
        assert_eq!(
            v,
            &vec![FILL_FLOAT, FILL_FLOAT, -64.0, FILL_FLOAT, -61.0, FILL_FLOAT]
        );
        let Values::Char(ids) = &var(&back, "cml_id").values else {
            panic!()
        };
        assert_eq!(ids, b"tst-a|tst-b");
        let Values::Double(len) = &var(&back, "length").values else {
            panic!()
        };
        assert!((len[0] - 11_119.0).abs() < 5.0, "{}", len[0]);
        let Values::Double(fr) = &var(&back, "frequency").values else {
            panic!()
        };
        assert_eq!(fr, &vec![5805.0, 5745.0]);
        let Values::Double(width) = &var(&back, "channel_width").values else {
            panic!()
        };
        assert_eq!(width, &vec![10.0, 20.0]);
        let Values::Double(t) = &var(&back, "time").values else {
            panic!()
        };
        assert_eq!(t[1] - t[0], 60.0);
        assert!(
            back.attrs
                .iter()
                .any(|(k, v)| k == "naming convention" && *v == Attr::Text("OpenSense-CML".into()))
        );
        assert!(!back.vars.iter().any(|v| v.name == "tsl"));
        assert!(back.attrs.iter().any(|(k, v)| k == "reference"
            && matches!(v, Attr::Text(t) if t.starts_with("https://github") && t.contains(".com/Londopy/"))));
        assert!(build(&f.conn, &cfg, T0 + DAY, T0 + 2 * DAY, 60).is_err());
    }

    /// CF 2.3: a letter, then letters, digits and underscores.
    fn cf_name(n: &str) -> bool {
        n.starts_with(|c: char| c.is_ascii_alphabetic())
            && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    }

    /// "seconds since 1970-01-01 00:00:00 UTC", the form CF time units take.
    fn time_units(u: &str) -> bool {
        let Some((unit, since)) = u.split_once(" since ") else {
            return false;
        };
        let mut parts = since.split(' ');
        let date: Vec<u32> = parts
            .next()
            .unwrap_or("")
            .split('-')
            .filter_map(|x| x.parse().ok())
            .collect();
        let clock: Vec<u32> = parts
            .next()
            .unwrap_or("0:0:0")
            .split(':')
            .filter_map(|x| x.parse().ok())
            .collect();
        ["seconds", "minutes", "hours", "days"].contains(&unit)
            && matches!(date[..], [_, 1..=12, 1..=31])
            && matches!(clock[..], [0..=23, 0..=59, 0..=59])
            && parts.next().is_none_or(|tz| tz == "UTC")
    }

    /// What the NetCDF User Guide and the CF conventions ask of a file's structure, the parts
    /// readers such as xarray rely on: names, `_FillValue` of the variable's own type,
    /// coordinate variables over their own dimension and in order, strings as char arrays
    /// with the length last, time units that parse, and `coordinates` naming variables that
    /// share the data's dimensions. OpenSense's two global attribute names with a space and
    /// brackets are let through.
    fn cf_problems(f: &File) -> Vec<String> {
        let mut out = vec![];
        let dim = |i: usize| f.dims[i].0.as_str();
        for n in f
            .dims
            .iter()
            .map(|d| &d.0)
            .chain(f.vars.iter().map(|v| &v.name))
        {
            if !cf_name(n) {
                out.push(format!("name {n:?}"));
            }
        }
        for (n, _) in &f.attrs {
            if !cf_name(n) && n != "file_author(s)" && n != "naming convention" {
                out.push(format!("global attribute {n:?}"));
            }
        }
        let strlen: BTreeSet<usize> = f
            .vars
            .iter()
            .filter(|v| matches!(v.values, Values::Char(_)))
            .filter_map(|v| v.dims.last().copied())
            .collect();
        for v in &f.vars {
            for (n, _) in &v.attrs {
                if !cf_name(n) && n != "_FillValue" && n != "_Encoding" {
                    out.push(format!("{} attribute {n:?}", v.name));
                }
            }
            let fill_ok = matches!(
                (&v.values, attr(v, "_FillValue")),
                (_, None)
                    | (Values::Float(_), Some(Attr::Float(_)))
                    | (Values::Double(_), Some(Attr::Double(_)))
            );
            if !fill_ok {
                out.push(format!(
                    "{}: _FillValue isn't of the variable's type",
                    v.name
                ));
            }
            if !matches!(v.values, Values::Char(_)) && v.dims.iter().any(|d| strlen.contains(d)) {
                out.push(format!("{}: uses a string-length dimension", v.name));
            }
            if let Some(Attr::Text(list)) = attr(v, "coordinates") {
                for c in list.split_whitespace() {
                    match f.vars.iter().find(|x| x.name == c) {
                        Some(x) if x.dims.iter().all(|d| v.dims.contains(d)) => {}
                        _ => out.push(format!(
                            "{}: coordinate {c} is missing or misshapen",
                            v.name
                        )),
                    }
                }
            }
            let Some(own) = f.dims.iter().position(|d| d.0 == v.name) else {
                continue;
            };
            match &v.values {
                Values::Char(_) => {
                    if v.dims.len() != 2 || v.dims[0] != own {
                        out.push(format!("{}: a string coordinate is [{0}, strlen]", v.name));
                    }
                }
                Values::Double(x) => {
                    if v.dims != [own] || !x.windows(2).all(|w| w[0] < w[1]) {
                        out.push(format!("{}: a coordinate rises along {}", v.name, dim(own)));
                    }
                }
                Values::Float(_) => out.push(format!("{}: coordinate stored as float", v.name)),
            }
        }
        match f
            .vars
            .iter()
            .find(|v| v.name == "time")
            .and_then(|v| attr(v, "units"))
        {
            Some(Attr::Text(u)) if time_units(u) => {}
            u => out.push(format!("time units {u:?}")),
        }
        out
    }

    /// The OpenSense CML table (netCDF_CML.adoc): each variable the export writes, its
    /// dimensions (strings without their length dimension), units and long_name.
    const OPENSENSE: &[(&str, &[&str], Option<&str>, &str)] = &[
        (
            "time",
            &["time"],
            Some("seconds since 1970-01-01 00:00:00 UTC"),
            "time_utc",
        ),
        (
            "cml_id",
            &["cml_id"],
            None,
            "commercial_microwave_link_identifier",
        ),
        ("sublink_id", &["sublink_id"], None, "sublink_identifier"),
        (
            "site_0_lat",
            &["cml_id"],
            Some("degrees_in_WGS84_projection"),
            "site_0_latitude",
        ),
        (
            "site_0_lon",
            &["cml_id"],
            Some("degrees_in_WGS84_projection"),
            "site_0_longitude",
        ),
        (
            "site_1_lat",
            &["cml_id"],
            Some("degrees_in_WGS84_projection"),
            "site_1_latitude",
        ),
        (
            "site_1_lon",
            &["cml_id"],
            Some("degrees_in_WGS84_projection"),
            "site_1_longitude",
        ),
        (
            "site_0_alt",
            &["cml_id"],
            Some("metres_above_sea"),
            "antenna_altitude_above_sea_level_at_site_0",
        ),
        (
            "site_1_alt",
            &["cml_id"],
            Some("metres_above_sea"),
            "antenna_altitude_above_sea_level_at_site_1",
        ),
        (
            "length",
            &["cml_id"],
            Some("m"),
            "distance_between_pair_of_antennas",
        ),
        (
            "frequency",
            &["cml_id", "sublink_id"],
            Some("MHz"),
            "sublink_frequency",
        ),
        (
            "rsl",
            &["cml_id", "sublink_id", "time"],
            Some("dBm"),
            "received_signal_level",
        ),
    ];

    fn opensense_problems(f: &File) -> Vec<String> {
        let mut out = vec![];
        let text = |a: Option<&Attr>| match a {
            Some(Attr::Text(t)) => Some(t.clone()),
            _ => None,
        };
        for (name, dims, units, long) in OPENSENSE {
            let Some(v) = f.vars.iter().find(|v| v.name == *name) else {
                out.push(format!("{name} is missing"));
                continue;
            };
            let mut have: Vec<&str> = v.dims.iter().map(|&d| f.dims[d].0.as_str()).collect();
            if matches!(v.values, Values::Char(_)) {
                have.pop();
            }
            if have != *dims {
                out.push(format!("{name} has dimensions {have:?}"));
            }
            if text(attr(v, "units")).as_deref() != *units {
                out.push(format!("{name} has units {:?}", attr(v, "units")));
            }
            if text(attr(v, "long_name")).as_deref() != Some(*long) {
                out.push(format!("{name} has long_name {:?}", attr(v, "long_name")));
            }
        }
        if let Some(rsl) = f.vars.iter().find(|v| v.name == "rsl")
            && (text(attr(rsl, "sampling")).as_deref() != Some("instantaneous")
                || attr(rsl, "_FillValue").is_none()
                || attr(rsl, "coordinates").is_none())
        {
            out.push("rsl needs sampling, _FillValue and coordinates".into());
        }
        if f.vars
            .iter()
            .find(|v| v.name == "time")
            .is_some_and(|t| attr(t, "_FillValue").is_none())
        {
            out.push("time needs _FillValue".into());
        }
        for g in [
            "title",
            "file_author(s)",
            "institution",
            "date",
            "source",
            "version",
            "history",
            "naming convention",
            "licence",
            "reference",
            "comment",
        ] {
            if !f.attrs.iter().any(|a| a.0 == g) {
                out.push(format!("global attribute {g} is missing"));
            }
        }
        out
    }

    #[test]
    fn the_file_has_the_structure_readers_and_the_conventions_ask_for() {
        let (f, cfg) = two_nodes();
        let back = read::file(
            &build(&f.conn, &cfg, T0, T0 + 3 * MIN, 60)
                .unwrap()
                .write()
                .unwrap(),
        );
        assert_eq!(cf_problems(&back), Vec::<String>::new());
        assert_eq!(opensense_problems(&back), Vec::<String>::new());
        // each check catches what it is there for
        let broken = |change: &dyn Fn(&mut File)| {
            let mut b = back.clone();
            change(&mut b);
            [cf_problems(&b), opensense_problems(&b)].concat()
        };
        let var_mut = |b: &mut File, n: &str| b.vars.iter().position(|v| v.name == n).unwrap();
        let p = broken(&|b| {
            let i = var_mut(b, "rsl");
            b.vars[i].attrs.retain(|a| a.0 != "_FillValue");
            b.vars[i]
                .attrs
                .push(("_FillValue".into(), Attr::Double(FILL_DOUBLE)));
        });
        assert!(p.iter().any(|x| x.contains("_FillValue isn't")), "{p:?}");
        let p = broken(&|b| {
            let i = var_mut(b, "time");
            if let Values::Double(t) = &mut b.vars[i].values {
                t.reverse();
            }
        });
        assert!(
            p.iter().any(|x| x.contains("time: a coordinate rises")),
            "{p:?}"
        );
        let p = broken(&|b| {
            let i = var_mut(b, "time");
            b.vars[i].attrs[0].1 = Attr::Text("seconds after launch".into());
        });
        assert!(p.iter().any(|x| x.starts_with("time units")), "{p:?}");
        let p = broken(&|b| {
            let i = var_mut(b, "snr");
            b.vars[i].attrs[1].1 = Attr::Text("site_9_lat site_0_lon".into());
        });
        assert!(
            p.iter().any(|x| x.contains("coordinate site_9_lat")),
            "{p:?}"
        );
        let p = broken(&|b| {
            let i = var_mut(b, "noise_level");
            b.vars[i].name = "noise-level".into();
        });
        assert!(p.iter().any(|x| x.contains("\"noise-level\"")), "{p:?}");
        let p = broken(&|b| {
            let i = var_mut(b, "frequency");
            b.vars[i].attrs[0].1 = Attr::Text("GHz".into());
        });
        assert!(
            p.iter().any(|x| x.starts_with("frequency has units")),
            "{p:?}"
        );
    }
}
