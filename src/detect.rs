//! Candidate weather events in the residuals, by explicit rules with the thresholds in
//! `[wet]` and `[duct]`. They are candidates with their evidence, never measurements of
//! rain or of the atmosphere.
//!
//! Both ends of a link see it, so the two ends' binned residuals are averaged into one
//! series per link.
//!
//! Wet antenna (water on a radome or dish at one site costs a few dB on every link there,
//! and drains or dries off over hours):
//! a link drops when its residual falls at least `drop_db` below its level before (the
//! median over `pre_min` ending `onset_min` before, or zero if that was above the
//! baseline, so coming back down from a ducting boost isn't a drop), and stays more than
//! `recover_db` below that level for between `hold_min` and `max_recover_h`, never falling
//! more than `max_drop_db`. A run broken by more than `onset_min` without data isn't
//! judged, since how long the link stayed down is unknown. A site is a candidate when at least `min_links` of its links, and at
//! least `min_frac` of those with data, drop within `onset_min` of each other, while no
//! more than `max_other_frac` of the mesh's other links drop at that time.
//!
//! Widespread drop (a rain front wetting several sites, or anything hitting every link's
//! measurements): what the site check turns down as mesh-wide. When a site's links drop
//! together but more than `max_other_frac` of the other links drop within `onset_min`
//! too, all those drops together are a widespread drop, merged with any that share a
//! drop. A wet candidate whose drops all lie inside a widespread drop gives way to it.
//!
//! Ducting or fading (a layer that bends signals along the coast, often in the evening
//! and at night): a bin departs when at least `min_with_data` links have data and at
//! least `min_links` of them, and at least `min_frac` of them, are `departure_db` or more
//! from their baseline, either way.
//! Departing bins no more than two bins apart form an episode, which is a candidate when
//! it lasts at least `min_duration_min`. An episode still under way in the newest bins
//! has no end yet. A wet antenna or widespread drop shows as links far below their
//! baseline too; an episode with none above it that overlaps one is left to that.

use std::collections::{BTreeMap, BTreeSet};

use ridgewarden_client::graph::edge;
use ridgewarden_client::util::median;
use serde::{Deserialize, Serialize};

use crate::config::{DuctCfg, WetCfg};
use crate::series::{End, HOUR, MIN};

/// A link, as its two node names in order.
pub type Pair = (String, String);

pub type Series = BTreeMap<Pair, Vec<(i64, f64)>>;

/// Average the binned residuals of both ends of each link.
pub fn link_series(ends: &BTreeMap<End, Vec<(i64, f64)>>) -> Series {
    let mut acc: BTreeMap<Pair, BTreeMap<i64, (f64, u32)>> = BTreeMap::new();
    for ((node, nb), pts) in ends {
        let m = acc.entry(edge(node, nb)).or_default();
        for &(t, v) in pts {
            let e = m.entry(t).or_insert((0.0, 0));
            e.0 += v;
            e.1 += 1;
        }
    }
    acc.into_iter()
        .map(|(p, m)| {
            (
                p,
                m.into_iter().map(|(t, (s, n))| (t, s / n as f64)).collect(),
            )
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Drop {
    pub a: String,
    pub b: String,
    pub start: i64,
    /// when it was back within `recover_db`; None while still down
    pub end: Option<i64>,
    /// the last bin seen low
    pub last_low: i64,
    /// the level before minus the lowest point, dB
    pub depth: f64,
}

/// The drops on one link's residual series.
pub fn drops(pair: &Pair, pts: &[(i64, f64)], cfg: &WetCfg) -> Vec<Drop> {
    let onset = cfg.onset_min as i64 * MIN;
    let pre_w = cfg.pre_min as i64 * MIN;
    let hold = cfg.hold_min as i64 * MIN;
    let longest = cfg.max_recover_h as i64 * HOUR;
    let mut out = vec![];
    let mut i = 0;
    while i < pts.len() {
        let (t, r) = pts[i];
        let lo = pts.partition_point(|p| p.0 < t - onset - pre_w);
        let hi = pts.partition_point(|p| p.0 < t - onset);
        let mut before: Vec<f64> = pts[lo..hi].iter().map(|p| p.1).collect();
        let level = match median(&mut before).map(|l| l.min(0.0)) {
            Some(l) if before.len() >= 2 && l - r >= cfg.drop_db => l,
            _ => {
                i += 1;
                continue;
            }
        };
        let mut j = i + 1;
        let mut low = r;
        while j < pts.len() && pts[j].0 - pts[j - 1].0 <= onset && pts[j].1 < level - cfg.recover_db
        {
            low = low.min(pts[j].1);
            j += 1;
        }
        let last_low = pts[j - 1].0;
        let end = match pts.get(j) {
            // the link went quiet before it came back, so how long it stayed down is unknown
            Some(p) if p.0 - last_low > onset => {
                i = j;
                continue;
            }
            p => p.map(|p| p.0),
        };
        let lasted = end.unwrap_or(last_low) - t;
        if end.is_none() && lasted > longest {
            // never came back: a step change, not water; look on from the next bin
            i += 1;
            continue;
        }
        if lasted >= hold && lasted <= longest && level - low <= cfg.max_drop_db {
            out.push(Drop {
                a: pair.0.clone(),
                b: pair.1.clone(),
                start: t,
                end,
                last_low,
                depth: level - low,
            });
        }
        i = j.max(i + 1);
    }
    out
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Wet {
    pub site: String,
    pub start: i64,
    pub end: Option<i64>,
    pub drops: Vec<Drop>,
    /// the site's links with data at the time
    pub site_links: usize,
    /// other links with data, and how many of them dropped too
    pub others: usize,
    pub others_dropping: usize,
}

fn has_data(pts: &[(i64, f64)], from: i64, to: i64) -> bool {
    let i = pts.partition_point(|p| p.0 < from);
    pts.get(i).is_some_and(|p| p.0 <= to)
}

/// Links with data around a drop starting at `t`, among those `pick` accepts.
fn live<'a>(
    series: &'a Series,
    cfg: &WetCfg,
    t: i64,
    pick: impl Fn(&Pair) -> bool,
) -> Vec<&'a Pair> {
    let from = t - (cfg.onset_min + cfg.pre_min) as i64 * MIN;
    let to = t + cfg.hold_min as i64 * MIN;
    series
        .iter()
        .filter(|(p, pts)| pick(p) && has_data(pts, from, to))
        .map(|(p, _)| p)
        .collect()
}

/// Drops starting within `onset_min` of `sorted[k]`, one per link, and where the next
/// cluster starts.
fn cluster(sorted: &[&Drop], k: usize, cfg: &WetCfg) -> (Vec<Drop>, usize) {
    let seed = sorted[k].start;
    let mut group: Vec<Drop> = vec![];
    let mut k = k;
    while k < sorted.len() && sorted[k].start <= seed + cfg.onset_min as i64 * MIN {
        if !group
            .iter()
            .any(|d| d.a == sorted[k].a && d.b == sorted[k].b)
        {
            group.push(sorted[k].clone());
        }
        k += 1;
    }
    (group, k)
}

/// The latest recovery, or None while any drop is still down.
fn ended(drops: &[Drop]) -> Option<i64> {
    drops
        .iter()
        .map(|d| d.end)
        .collect::<Option<Vec<_>>>()
        .and_then(|e| e.into_iter().max())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Front {
    pub start: i64,
    pub end: Option<i64>,
    pub drops: Vec<Drop>,
    /// every site at either end of a dropping link
    pub sites: Vec<String>,
    /// links with data at the time
    pub with_data: usize,
}

/// Sites whose links drop together (wet antennas), and drops too widespread for one
/// site (fronts).
pub fn site_drops(series: &Series, all: &[Drop], cfg: &WetCfg) -> (Vec<Wet>, Vec<Front>) {
    let onset = cfg.onset_min as i64 * MIN;
    let touches = |a: &str, b: &str, s: &str| a == s || b == s;
    let sites: BTreeSet<&String> = series.keys().flat_map(|p| [&p.0, &p.1]).collect();
    let mut wets = vec![];
    let mut raw = vec![];
    for site in sites {
        let mut mine: Vec<&Drop> = all.iter().filter(|d| touches(&d.a, &d.b, site)).collect();
        mine.sort_by_key(|d| d.start);
        let mut k = 0;
        while k < mine.len() {
            let seed = mine[k].start;
            let (group, next) = cluster(&mine, k, cfg);
            k = next;
            let site_links = live(series, cfg, seed, |p| touches(&p.0, &p.1, site)).len();
            let others = live(series, cfg, seed, |p| !touches(&p.0, &p.1, site));
            let elsewhere: Vec<Drop> = all
                .iter()
                .filter(|d| {
                    (d.start - seed).abs() <= onset
                        && others.iter().any(|p| p.0 == d.a && p.1 == d.b)
                })
                .cloned()
                .collect();
            let others_dropping = elsewhere
                .iter()
                .map(|d| (&d.a, &d.b))
                .collect::<BTreeSet<_>>()
                .len();
            let n = group.len();
            if n < cfg.min_links || (n as f64) < cfg.min_frac * site_links as f64 {
                continue;
            }
            if others_dropping as f64 <= cfg.max_other_frac * others.len() as f64 {
                wets.push(Wet {
                    site: site.clone(),
                    start: seed,
                    end: ended(&group),
                    drops: group,
                    site_links,
                    others: others.len(),
                    others_dropping,
                });
            } else {
                let mut drops = group;
                drops.extend(elsewhere);
                raw.push(Front {
                    start: seed,
                    end: None,
                    drops,
                    sites: vec![],
                    with_data: site_links + others.len(),
                });
            }
        }
    }
    let fronts = merge(raw);
    wets.retain(|w| {
        !fronts
            .iter()
            .any(|f| w.drops.iter().all(|d| f.drops.contains(d)))
    });
    wets.sort_by_key(|w| w.start);
    (wets, fronts)
}

/// Fronts that share a drop are one front.
fn merge(mut raw: Vec<Front>) -> Vec<Front> {
    raw.sort_by_key(|f| f.start);
    let mut out: Vec<Front> = vec![];
    for f in raw {
        match out
            .iter_mut()
            .find(|m| f.drops.iter().any(|d| m.drops.contains(d)))
        {
            Some(m) => {
                for d in f.drops {
                    if !m.drops.contains(&d) {
                        m.drops.push(d);
                    }
                }
                m.with_data = m.with_data.max(f.with_data);
            }
            None => out.push(f),
        }
    }
    for f in &mut out {
        f.drops
            .sort_by(|a, b| (a.start, &a.a, &a.b).cmp(&(b.start, &b.a, &b.b)));
        f.start = f.drops[0].start;
        f.end = ended(&f.drops);
        let sites: BTreeSet<String> = f
            .drops
            .iter()
            .flat_map(|d| [d.a.clone(), d.b.clone()])
            .collect();
        f.sites = sites.into_iter().collect();
    }
    out
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Duct {
    pub start: i64,
    /// None while the episode reaches the newest bins
    pub end: Option<i64>,
    /// the end of the last departing bin so far
    pub last: i64,
    /// every link that departed during the episode
    pub links: Vec<Pair>,
    /// at the bin with the largest share departing
    pub peak_at: i64,
    pub peak_departing: usize,
    pub peak_with_data: usize,
    /// how many of those were above and below their baseline
    pub peak_up: usize,
    pub peak_down: usize,
    /// the largest departure seen, dB
    pub max_abs: f64,
}

struct Episode {
    first: i64,
    last: i64,
    links: BTreeSet<Pair>,
    peak: (f64, i64, usize, usize, usize, usize),
    max_abs: f64,
}

impl Episode {
    fn finish(self, bin: i64, to: i64, cfg: &DuctCfg) -> Option<Duct> {
        let (_, peak_at, departing, with_data, up, down) = self.peak;
        (self.last + bin - self.first >= cfg.min_duration_min as i64 * MIN).then(|| Duct {
            start: self.first,
            end: (to - self.last > 3 * bin).then_some(self.last + bin),
            last: self.last + bin,
            links: self.links.into_iter().collect(),
            peak_at,
            peak_departing: departing,
            peak_with_data: with_data,
            peak_up: up,
            peak_down: down,
            max_abs: self.max_abs,
        })
    }
}

/// Episodes when many links depart from their baselines at once. `bin` is the width of
/// the residual bins.
/// `to` is the end of the data looked at: an episode within two bins of it may still be
/// going.
pub fn ducting(series: &Series, bin: i64, to: i64, cfg: &DuctCfg) -> Vec<Duct> {
    let mut by_t: BTreeMap<i64, Vec<(&Pair, f64)>> = BTreeMap::new();
    for (p, pts) in series {
        for &(t, v) in pts {
            by_t.entry(t).or_default().push((p, v));
        }
    }
    let mut out = vec![];
    let mut cur: Option<Episode> = None;
    for (t, vals) in by_t {
        let dep: Vec<&(&Pair, f64)> = vals
            .iter()
            .filter(|(_, v)| v.abs() >= cfg.departure_db)
            .collect();
        let frac = dep.len() as f64 / vals.len() as f64;
        if vals.len() < cfg.min_with_data || dep.len() < cfg.min_links || frac < cfg.min_frac {
            continue;
        }
        if cur.as_ref().is_some_and(|e| t - e.last > 2 * bin) {
            out.extend(cur.take().and_then(|e| e.finish(bin, to, cfg)));
        }
        let e = cur.get_or_insert_with(|| Episode {
            first: t,
            last: t,
            links: BTreeSet::new(),
            peak: (0.0, t, 0, 0, 0, 0),
            max_abs: 0.0,
        });
        e.last = t;
        for (p, v) in &dep {
            e.links.insert((*p).clone());
            e.max_abs = e.max_abs.max(v.abs());
        }
        if frac > e.peak.0 {
            let up = dep.iter().filter(|(_, v)| *v > 0.0).count();
            e.peak = (frac, t, dep.len(), vals.len(), up, dep.len() - up);
        }
    }
    out.extend(cur.and_then(|e| e.finish(bin, to, cfg)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_795_000_000_000 - 1_795_000_000_000 % (24 * HOUR);
    const BIN: i64 = 10 * MIN;

    fn wet_cfg() -> WetCfg {
        crate::config::WetCfg::default()
    }

    fn pair(a: &str, b: &str) -> Pair {
        (a.to_string(), b.to_string())
    }

    /// Two days of 10-minute bins, flat with a little wobble, plus whatever `f` adds.
    fn flat(f: impl Fn(i64) -> f64) -> Vec<(i64, f64)> {
        (0..288)
            .map(|i| {
                let t = T0 + i * BIN;
                let wobble = if i % 2 == 0 { 0.3 } else { -0.3 };
                (t, wobble + f(t))
            })
            .collect()
    }

    /// A wet-antenna shape: down `db` at `at`, back over `hours`.
    fn wet_dip(at: i64, db: f64, hours: i64) -> impl Fn(i64) -> f64 {
        move |t| {
            if t < at || t >= at + hours * HOUR {
                0.0
            } else {
                let k = (t - at) as f64 / (hours * HOUR) as f64;
                -db * (1.0 - k)
            }
        }
    }

    #[test]
    fn a_sudden_dip_that_recovers_over_hours_is_a_drop() {
        let at = T0 + 20 * HOUR;
        let d = drops(&pair("a", "b"), &flat(wet_dip(at, 4.0, 4)), &wet_cfg());
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].start, at);
        assert!((d[0].depth - 4.0).abs() < 0.5, "{}", d[0].depth);
        let back = d[0].end.unwrap() - at;
        assert!(back > 2 * HOUR && back <= 4 * HOUR, "{back}");
    }

    #[test]
    fn slow_drift_short_blips_steps_and_outages_are_not_drops() {
        let at = T0 + 20 * HOUR;
        let cfg = wet_cfg();
        let slow = |t: i64| -3.0 * ((t - T0) as f64 / (48 * HOUR) as f64);
        assert!(drops(&pair("a", "b"), &flat(slow), &cfg).is_empty());
        let blip = move |t: i64| {
            if (at..at + BIN).contains(&t) {
                -5.0
            } else {
                0.0
            }
        };
        assert!(drops(&pair("a", "b"), &flat(blip), &cfg).is_empty());
        // down 4 dB for good: a realigned dish, not water
        let step = move |t: i64| if t >= at { -4.0 } else { 0.0 };
        assert!(drops(&pair("a", "b"), &flat(step), &cfg).is_empty());
        let outage = wet_dip(at, 25.0, 3);
        assert!(drops(&pair("a", "b"), &flat(outage), &cfg).is_empty());
    }

    #[test]
    fn coming_down_from_a_boost_is_not_a_drop_and_doesnt_hide_a_later_one() {
        let boost_end = T0 + 6 * HOUR;
        let at = T0 + 30 * HOUR;
        let dip = wet_dip(at, 4.0, 4);
        let f = move |t: i64| if t < boost_end { 6.0 } else { dip(t) };
        let d = drops(&pair("a", "b"), &flat(f), &wet_cfg());
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].start, at);
    }

    #[test]
    fn a_dip_cut_by_silence_is_not_judged() {
        let at = T0 + 20 * HOUR;
        let pts: Vec<(i64, f64)> = flat(wet_dip(at, 6.0, 4))
            .into_iter()
            .filter(|p| !(at + 2 * BIN..at + 6 * BIN).contains(&p.0))
            .map(|(t, v)| if t >= at + 6 * BIN { (t, 0.0) } else { (t, v) })
            .collect();
        assert!(drops(&pair("a", "b"), &pts, &wet_cfg()).is_empty());
    }

    #[test]
    fn a_drop_still_going_at_the_end_of_the_data_is_open() {
        let at = T0 + 46 * HOUR;
        let d = drops(&pair("a", "b"), &flat(wet_dip(at, 4.0, 6)), &wet_cfg());
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].end, None);
    }

    fn star(wet_links: &[(&str, &str)], at: i64) -> Series {
        let links = [
            ("hub", "n1"),
            ("hub", "n2"),
            ("hub", "n3"),
            ("n1", "n2"),
            ("n2", "n3"),
            ("n3", "n4"),
        ];
        links
            .iter()
            .map(|(a, b)| {
                let wet = wet_links.contains(&(*a, *b));
                let f = wet_dip(at, if wet { 3.5 } else { 0.0 }, 3);
                (pair(a, b), flat(f))
            })
            .collect()
    }

    fn all_drops(s: &Series) -> Vec<Drop> {
        s.iter()
            .flat_map(|(p, v)| drops(p, v, &wet_cfg()))
            .collect()
    }

    #[test]
    fn links_dropping_together_at_one_site_make_a_wet_candidate() {
        let at = T0 + 30 * HOUR;
        let s = star(&[("hub", "n1"), ("hub", "n2"), ("hub", "n3")], at);
        let w = site_drops(&s, &all_drops(&s), &wet_cfg()).0;
        assert_eq!(w.len(), 1, "{w:?}");
        assert_eq!(w[0].site, "hub");
        assert_eq!(w[0].drops.len(), 3);
        assert_eq!(
            (w[0].site_links, w[0].others, w[0].others_dropping),
            (3, 3, 0)
        );
        assert!(w[0].end.is_some());
    }

    #[test]
    fn one_link_alone_or_the_whole_mesh_is_not_a_wet_site() {
        let at = T0 + 30 * HOUR;
        // only one of hub's three links: the path or one radio, not the site
        let s = star(&[("hub", "n1")], at);
        assert!(site_drops(&s, &all_drops(&s), &wet_cfg()).0.is_empty());
        // everything at once: mesh-wide, not one site
        let every = [
            ("hub", "n1"),
            ("hub", "n2"),
            ("hub", "n3"),
            ("n1", "n2"),
            ("n2", "n3"),
            ("n3", "n4"),
        ];
        let s = star(&every, at);
        let w = site_drops(&s, &all_drops(&s), &wet_cfg()).0;
        assert!(w.is_empty(), "{w:?}");
        // but it doesn't vanish: it's a widespread drop
        let f = site_drops(&s, &all_drops(&s), &wet_cfg()).1;
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!((f[0].drops.len(), f[0].with_data), (6, 6));
        assert_eq!(f[0].sites, ["hub", "n1", "n2", "n3", "n4"]);
        // the three hub links alone are one site, not a front
        let s = star(&[("hub", "n1"), ("hub", "n2"), ("hub", "n3")], at);
        assert!(site_drops(&s, &all_drops(&s), &wet_cfg()).1.is_empty());
    }

    #[test]
    fn many_links_departing_for_hours_is_a_ducting_episode() {
        let from = T0 + 26 * HOUR;
        let to = T0 + 30 * HOUR;
        let s: Series = (0..6)
            .map(|i| {
                // four of six links go 6 dB up or down, the way a duct both boosts and fades
                let sign = if i % 2 == 0 { 1.0 } else { -1.0 };
                let f = move |t: i64| {
                    if i < 4 && (from..to).contains(&t) {
                        6.0 * sign
                    } else {
                        0.0
                    }
                };
                (pair("a", &format!("n{i}")), flat(f))
            })
            .collect();
        let d = ducting(&s, BIN, T0 + 48 * HOUR, &DuctCfg::default());
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!((d[0].start, d[0].end), (from, Some(to)));
        assert_eq!(d[0].links.len(), 4);
        assert_eq!((d[0].peak_departing, d[0].peak_with_data), (4, 6));
        assert_eq!((d[0].peak_up, d[0].peak_down), (2, 2));
        // two links departing is under min_links, and 20 minutes is too short
        let short: Series = (0..6)
            .map(|i| {
                let f = move |t: i64| {
                    if (i < 2 && (from..to).contains(&t))
                        || (i < 4 && (from..from + 20 * MIN).contains(&t))
                    {
                        6.0
                    } else {
                        0.0
                    }
                };
                (pair("a", &format!("n{i}")), flat(f))
            })
            .collect();
        assert!(ducting(&short, BIN, T0 + 48 * HOUR, &DuctCfg::default()).is_empty());
        // three of three links departing is too few links reporting to say anything
        let few: Series = s.into_iter().take(3).collect();
        assert!(ducting(&few, BIN, T0 + 48 * HOUR, &DuctCfg::default()).is_empty());
    }

    #[test]
    fn an_episode_reaching_the_newest_bins_has_no_end_yet() {
        // flat() runs to T0 + 48 h; this episode is still going there
        let from = T0 + 45 * HOUR;
        let s: Series = (0..6)
            .map(|i| {
                let f = move |t: i64| if i < 4 && t >= from { 6.0 } else { 0.0 };
                (pair("a", &format!("n{i}")), flat(f))
            })
            .collect();
        let d = ducting(&s, BIN, T0 + 48 * HOUR, &DuctCfg::default());
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].end, None);
        assert_eq!(d[0].last, T0 + 48 * HOUR);
    }

    #[test]
    fn a_wet_site_inside_a_widespread_drop_gives_way_to_it() {
        let at = T0 + 30 * HOUR;
        // the hub's three links and two of the other three drop: 5 of 6
        let s = star(
            &[
                ("hub", "n1"),
                ("hub", "n2"),
                ("hub", "n3"),
                ("n1", "n2"),
                ("n2", "n3"),
            ],
            at,
        );
        let (w, f) = site_drops(&s, &all_drops(&s), &wet_cfg());
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].drops.len(), 5);
        assert_eq!(f[0].start, at);
    }

    #[test]
    fn both_ends_of_a_link_are_averaged() {
        let mut ends = BTreeMap::new();
        ends.insert(
            ("b".to_string(), "a".to_string()),
            vec![(0, -2.0), (BIN, 1.0)],
        );
        ends.insert(("a".to_string(), "b".to_string()), vec![(0, -4.0)]);
        let s = link_series(&ends);
        assert_eq!(s[&pair("a", "b")], vec![(0, -3.0), (BIN, 1.0)]);
    }
}
