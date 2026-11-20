# ridgewarden-weather

[![CI](https://github.com/Londopy/ridgewarden-weather/actions/workflows/ci.yml/badge.svg)](https://github.com/Londopy/ridgewarden-weather/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Londopy/ridgewarden-weather)](https://github.com/Londopy/ridgewarden-weather/releases/latest)
[![changelog](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Londopy/ridgewarden-weather/gh-pages/changelog-badge.json)](CHANGELOG.md)
[![License](https://img.shields.io/github/license/Londopy/ridgewarden-weather)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange)](https://www.rust-lang.org)
[![Linux](https://img.shields.io/badge/platform-Linux%20(Pi%20%7C%20x86__64)-555)](#building-from-source)

**Experimental: use the mesh's own link levels as a weather sensor, and say plainly what they can and can't show.**

A ridgewarden app for AREDN meshes, and a research project rather than something every site installs. Mountaintop links report their signal levels around the clock, and weather moves those numbers. This app records them, takes the daily cycle out of each link, and looks for two effects a 5 GHz link can really show: water on a site's antennas during rain, and coastal ducting and fading. It lists what it finds as candidate events with the evidence, checks them against rain reports and upper-air soundings, and exports the link data in the OpenSense format used for microwave-link rain research.

It does not measure rain. At 5 GHz, rain on the path costs about 0.01 dB per km at 10 mm/h, far too little to map rainfall. The effects it looks for may also turn out too small to separate from equipment temperature, wind and interference; a well-documented negative result is a result too.

It needs a [ridgewarden](https://github.com/Londopy/ridgewarden) core running on the same box, ideally the hub core that crawls the backbone. The core is the only part that talks to the nodes; this app reads its event feed. Spec sheet 09 of the AREDN app suite.

## What it does

- Reads every RF link level from the core's node polls: the received signal (RSL) each node reports for each neighbor, its noise floor and SNR. Where a node reports only SNR and noise, their sum stands in for the signal. Levels outside -130 to 0 dBm are dropped as broken reports.
- Works with whatever sampling it gets. Minute samples are best; with the core's default 15-minute crawl, ducting still shows but the onset of a wet-antenna drop is harder to see. The front page shows each link's sample interval.
- Takes the daily cycle out: each hour is rolled up as a median, and the baseline for a day is, per UTC hour of day, the median of that hour over the previous 21 days (once at least 7 have data). The residual is each sample minus its baseline, in 10-minute medians, with the two ends of a link averaged.
- Runs detection every 10 minutes, on its own, and keeps every candidate with its evidence after the raw samples behind it are gone.
- Finds wet-antenna candidates: a sudden drop of a few dB on several links ending at one site that recovers over hours, while the rest of the mesh stays put. Each one is checked against the rain reports of the stations near that site, and against lightning strikes logged by [ridgewarden-lightning](https://github.com/Londopy/ridgewarden-lightning) within 30 km.
- Finds widespread drops: when a site's links drop together but so do more than half of the mesh's other links, it is too widespread for one site's antennas. A rain front wetting several sites looks like this, and so does anything hitting every link's measurements; it is listed as its own kind of candidate rather than thrown away. A wet site whose drops all fall inside one gives way to it; one with only some of its drops inside (the leading edge of a front, say) is kept as well.
- Finds ducting and fading candidates: at least 5 links reporting and at least 3 of them (30%) 4 dB or more from their baseline, for at least half an hour. Each is shown with every station's nearest sounding: the inversion and the steepest refractivity gradient.
- A live marine-layer indicator on the front page: calm, disturbed, ducting and fading under way, or a widespread drop, from the newest complete residual bins and the latest sounding, with the evidence and a confidence that drops when few links report, the sounding is old, or the two disagree. `/api/now` has it as JSON.
- Publishes every candidate to the mesh as a signed `weather.candidate` event, so other apps can allow for weather, and the rain readings it fetches as `weather.rain` telemetry (see below).
- A results page for the project's success measure: rain hours against dry hours per link and over all links, night against day for the size of departures, and how many wet candidates rain confirmed, each with a 95% interval and a plain "clear" or "not clear".
- Wind: APRS reports carry the wind and the peak gust, kept with each reading. A candidate whose sites' stations reported a gust of 30 mph or more (`gust_mph`) is flagged, since wind moving a dish or mount swings a link much as weather in the path does; the indicator's confidence drops too.
- Link lengths from the nodes' reported positions on the front page, to see which links are long enough to cross the marine layer.
- Rain from APRS weather stations heard over APRS-IS (most are CWOP members), read-only, and from NWS observations on api.weather.gov. Upper-air soundings from the University of Wyoming archive.
- A residual chart per link with rain, sounding and candidate markers, the level and baseline for each direction, and the noise floor (a rising noise floor is interference, not weather).
- A methods page that states the physics and every rule with the thresholds in use.
- Exports the link data as a NetCDF file in the OpenSense CML conventions, from the page or the command line.
- `/api/events` returns the candidates as JSON, `/api/now` the indicator.

The rules are fixed, explicit and tunable in the config; they are written out on the methods page and at the top of `src/detect.rs`.

## Install

On a Raspberry Pi or Linux box that already runs the [ridgewarden core](https://github.com/Londopy/ridgewarden):

```sh
curl -fsSL https://github.com/Londopy/ridgewarden-weather/releases/latest/download/install.sh | sudo sh
```

Or take the `.deb` from the [releases page](https://github.com/Londopy/ridgewarden-weather/releases/latest): `sudo apt install ./ridgewarden-weather_<version>_arm64.deb` (`_amd64` on a PC). The install script checks the download against the release key your core already trusts. Either way `/etc/ridgewarden/weather.toml` is written if missing, and the service starts once the core is set up. The page is on port 8801.

Run it at one hub site, not everywhere.

### Minute data from the backbone

The link levels come from the core's polls. A core polls its own node over the LAN; to sample the backbone links once a minute, one hub core crawls the backbone nodes. Agree this with the node owners first: it is more load than Link Doctor needs. In that core's `ridgewarden.toml`:

```toml
[crawl]
enabled = true
interval_s = 60
url_template = "http://{node}.local.mesh"
nodes = ["tst-cuesta", "tst-madonna", "tst-blackmtn"]
follow_links = false
```

Every core that syncs telemetry with the hub gets those polls too, so the app could run at any of them, but run it at one place.

Every release also has each download as a `.torrent`, with its GitHub download as the web seed, and signed `SHA256SUMS.txt` checksums.

The service runs as its own user, `ridgewarden-weather`, in the core's `ridgewarden` group, so it can sign events through the core but can't read the station's key. Run commands that touch the app's data as that user, so files keep the right owner: `sudo -u ridgewarden-weather ridgewarden-weather --config /etc/ridgewarden/weather.toml <command>`.

## Usage

```sh
ridgewarden-weather init --config /etc/ridgewarden/weather.toml
ridgewarden-weather run  --config /etc/ridgewarden/weather.toml
```

The pages are on `http://<pi>:8801/`: Now (the marine-layer indicator, recent candidates, each link's sampling, baseline and latest residual, the weather sources), Events, Results, Methods, and Dataset. Add it to the core's dashboard with an `[[app]]` entry in `ridgewarden.toml`.

Detection runs every 10 minutes and starts once links have 7 days of history. Candidates are kept after the raw samples behind them are pruned.

Other commands:

```sh
# the OpenSense export, last 30 days at one-minute steps
ridgewarden-weather export --days 30 --step 60 --out links.nc

# load saved weather data: a Wyoming sounding page, an api.weather.gov
# observations response, or an APRS log ("<RFC 3339 time> <TNC2 packet>" per line)
ridgewarden-weather import sounding 72393-2026111812.html
ridgewarden-weather import nws ksbp.json --station KSBP
ridgewarden-weather import aprs aprs.log
```

## Configuration

`/etc/ridgewarden/weather.toml` (`init` writes a commented one):

```toml
core_url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"   # for publishing candidates
publish = true         # weather.candidate events; one site only
data_dir = "/var/lib/ridgewarden-weather"
listen = "0.0.0.0:8801"
keep_days = 90         # raw samples; hourly medians are kept 400 days
utc_offset_h = -8      # local standard time, for saying when events happen
gust_mph = 30.0        # gusts this strong near a site flag its candidates (dish movement)
links = []             # e.g. [["tst-cuesta", "tst-madonna"]]; empty means every RF link

[baseline]
days = 21
min_days = 7
bin_min = 10

[wet]
drop_db = 2.0          # sudden fall that counts as a drop
max_drop_db = 10.0     # bigger is an outage or obstruction
onset_min = 30         # the level before ends this long before the drop,
pre_min = 60           # and is the median over this long: a fall within about an hour counts
hold_min = 20          # shorter dips are ignored
recover_db = 1.0       # recovered when back within this of the level before
max_recover_h = 12     # not back by then: a step change, not water
min_links = 2          # links at one site dropping together
min_frac = 0.5         # and at least this share of that site's links
max_other_frac = 0.5   # more of the other links dropping at once is a widespread drop
rain_before_h = 2
strike_km = 30.0       # lightning this close to a site supports a wet candidate

[duct]
departure_db = 4.0
min_links = 3
min_frac = 0.3
min_with_data = 5      # bins with fewer links reporting don't count
min_duration_min = 30

[[site]]
node = "tst-cuesta"
label = "Cuesta Peak"
aprs = ["CW0000"]      # APRS weather stations near the site
nws = ["KSBP"]         # NWS observation stations near the site
frequency_mhz = 5745.0 # for the export
antenna_alt_m = 790.0  # for the export

[aprs]
server = "rotate.aprs2.net:14580"
login = "N0CALL"       # read-only login, no passcode

[nws]
user_agent = "ridgewarden-weather (you@example.org)"
interval_min = 30

[soundings]
stations = ["72393"]   # Vandenberg; 72493 is Oakland
max_height_m = 1500  # the lowest part, where the marine layer and the sites are

[export]
title = "AREDN backbone link levels"
author = "N0CALL (you@example.org)"
licence = "CC-BY-4.0"
frequency_mhz = 5745.0
```

A site with a single link can't be a wet-antenna candidate under the default `min_links = 2`: one link dropping can't say which end got wet.

### Weather sources

All three are optional, and every fetch has a timeout.

- **APRS-IS.** One read-only connection with a budlist filter (`b/CW0000/...`), so the server sends only the configured stations. The `r` field of an APRS weather report (APRS 1.0.1, chapter 12) is rain in the last hour in hundredths of an inch. Parsing is shared with ridgewarden-lightning.
- **NWS.** `GET /stations/{id}/observations?start=...` on api.weather.gov every 30 minutes, with your `user_agent` (the API refuses requests without one). Each observation's `precipitationLastHour` (mm, on the hourly reports) and `textDescription` ("Light Rain") are kept.
- **Soundings.** `GET https://weather.uwyo.edu/wsgi/sounding?datetime=2026-11-18 12:00:00&id=72393&type=TEXT:LIST`. Soundings fly at 00 and 12 UTC and not every station flies every time, so each time is tried 2, 6 and 24 hours after launch and then given up. Only pressure, height, temperature and dew point are read, from the fixed-width table.

## Published to the mesh

With `publish = true` (the default) and the core's token readable, the app logs two kinds of signed event through the core. Run it, and so the publishing, at one site only.

### weather.candidate

A main-log event per candidate: once when it is found, again under the same `id` when it ends, and once more with `"withdrawn": true` if a later scan no longer finds it (it turned out to be part of a widespread drop, say). Readers keep the latest event per `id`. Only candidates overlapping the last 14 days are published, at most 20 events per round. Link Doctor reads the same candidates from this app's `/api/events` today; the events let any app read them from its own core instead.

The payload is the candidate exactly as `/api/events` lists it, plus `v`, `id`, `summary`, `withdrawn` and `evidence`. A wet antenna, version 1:

```json
{
  "v": 1,
  "id": "34906e8318c844af",
  "kind": "wet",
  "site": "tst-hub",
  "start": 1795096800000,
  "end": 1795102800000,
  "drops": [
    {"a": "tst-hub", "b": "tst-n1", "start": 1795096800000, "end": 1795102800000, "last_low": 1795102200000, "depth": 3.25}
  ],
  "site_links": 3,
  "others": 2,
  "others_dropping": 0,
  "summary": "Wet antenna? 3 of Hub Peak's 3 links dropped at 2026-11-19 14:00 UTC.",
  "withdrawn": false,
  "evidence": {
    "rain_stations": ["CW0001"],
    "rain": {"verdict": "rain", "station": "CW0001", "source": "aprs", "at_ms": 1795098000000, "mm_1h": 5.08, "text": null},
    "strikes": {"count": 1, "nearest_km": 3.8, "radius_km": 30.0},
    "soundings": []
  }
}
```

- `kind` is `wet` (one site's antennas), `front` (a widespread drop) or `duct` (ducting or fading).
- `wet`: `site`, `start`, `end`, `drops` (each `a`, `b`, `start`, `end`, `last_low`, `depth` in dB), `site_links`, `others`, `others_dropping`.
- `front`: `start`, `end`, `drops`, `sites` (every site with a dropping link), `with_data`.
- `duct`: `start`, `end`, `last` (end of the last departing bin so far), `links` (`[a, b]` pairs), `peak_at`, `peak_departing`, `peak_with_data`, `peak_up`, `peak_down`, `max_abs` (dB).
- Node names in `drops` and `links` are in order (`a` < `b`). Times are unix milliseconds. `end` is null while a candidate is still going; a withdrawn one gets the time it was withdrawn.
- `evidence.wind` is `{"station", "at_ms", "gust_mph", "windy"}` for the strongest gust the stations near the candidate's sites reported (from an hour before to an hour after it), or null with no gust reports; `windy` is true at `gust_mph` (30 by default) or more, meaning wind may have moved a dish.
- `evidence.rain` is `{"verdict": "rain", ...}`, `{"verdict": "dry", "reports": n}`, `{"verdict": "none"}` (no reports for that time), or null (no station set, or ducting). `strikes` is null for ducting and when no site involved has a known position, and a count of 0 can also mean no lightning receivers cover the area. `soundings` (ducting) lists `station`, `at_ms`, `inversion_base_m`, `inversion_top_m`, `inversion_c`, `gradient_n_per_km`, `gradient_lo_m` and `gradient_hi_m`.

### weather.rain

Telemetry events (pruned with the station's other telemetry) carrying the rain readings fetched from APRS-IS and NWS, so other apps (ridgewarden-lightning's dry-lightning check, say) can use them:

```json
{"v": 1, "readings": [
  {"station": "CW0001", "source": "aprs", "at_ms": 1795098000000, "rain_1h_mm": 5.08, "rain_24h_mm": 31.75, "text": null, "wind_dir_deg": 160.0, "wind_mph": 5.0, "gust_mph": 12.0},
  {"station": "KSBP", "source": "nws", "at_ms": 1795089360000, "rain_1h_mm": 0.0, "rain_24h_mm": null, "text": "Cloudy", "wind_dir_deg": null, "wind_mph": null, "gust_mph": null}
]}
```

At most 200 readings per event, each sent once, and again if a later fetch changes it. Only readings from the last two days go out, so an imported log or turning publishing on later doesn't flood the mesh. `rain_1h_mm` and `rain_24h_mm` are null when the report didn't carry them (NWS reports have no 24-hour figure); `text` is the NWS observer's description. `wind_dir_deg` (where the wind blows from), `wind_mph` (sustained) and `gust_mph` (peak in the last five minutes) come from APRS reports only. A report with only a gust and no rain figure is sent too. Readings are sent every 10 minutes, with the analysis round.

## The dataset

The export follows the [OpenSense data format conventions](https://github.com/OpenSenseAction/OS_data_format_conventions) for commercial microwave link (CML) data (`netCDF_CML.adoc` and `netCDF_global_attributes.adoc`), and is written as a real NetCDF classic file (64-bit offset format), not CSV.

- Dimensions `cml_id`, `sublink_id`, `time`. Each RF link is one CML, `cml_id` "node-a|node-b" with the names in order. `sublink_1` runs from site 0 to site 1 (its RSL is what site 1's node hears), `sublink_2` the other way.
- `time` is seconds since 1970-01-01 00:00:00 UTC on a regular axis; each value is the last sample in (time - step, time].
- `site_0_lat`, `site_0_lon`, `site_1_lat`, `site_1_lon` from the nodes' own reported positions; `site_0_alt`, `site_1_alt` and `frequency` from the config; `length` from the positions.
- `rsl` (dBm, `sampling = "instantaneous"`), and the AREDN extras `noise_level` (dBm) and `snr` (dB). Missing values are the NetCDF default fill.
- There is no `tsl`: AREDN nodes don't report transmit power, a fixed setting, and the conventions allow leaving out a constant level. `polarisation` is left out because it isn't known.
- Global attributes `title`, `file_author(s)`, `institution`, `date`, `source`, `version`, `history`, `naming convention` ("OpenSense-CML"), `licence`, `reference`, `comment`.

The writer is tested by reading files back with its own parser and by matching a tiny file byte for byte against the format specification; files from a live run also open in xarray. To check one yourself:

```sh
ncdump -h links.nc
python -c "import xarray; print(xarray.open_dataset('links.nc'))"
```

Rain gauge data is not part of the export (OpenSense has a separate format for gauges).

## Limits

Not done from the spec, and why:

- The paper with a faculty advisor: not software.
- Feeding the personal weather model: that model doesn't exist as an app, so candidates are published as `weather.candidate` events any app can read instead.
- AREDN's Prometheus metrics as a cheaper source than the crawl: the core would have to poll them (a core change, not this app's); the crawl's load is listed under Install so it can be agreed with node owners.
- Publishing the dataset somewhere public: a person has to upload the file (Zenodo, say); the app writes it.

Other limits:

- Experimental. The candidates are leads, not measurements. Radio temperature, sun on a dish, wind moving a mount, rate changes and interference all move link levels; the daily part of those is taken out, the rest is why everything is a candidate.
- Nothing is found until links have 7 days of history, and a site's wet-antenna check needs weather stations set for it. The marine-layer indicator says "can't tell" until then.
- The results page's intervals assume independent hours, which neighbouring hours aren't, so they are too narrow; and with many links, about one in twenty will look clear by chance. "Clear" there means worth a closer look, not proof.
- Lightning only counts where ridgewarden-lightning's receivers cover the area, and only strikes logged in the core this app follows.
- Hour-of-day is UTC, so the baseline is unaffected by daylight saving; `utc_offset_h` only labels times.
- The sounding parser reads the University of Wyoming text list; another source needs its own parser.

## Building from source

This repo builds against the core's shared client crate and ridgewarden-lightning's APRS parser, so check them out next to it:

```sh
git clone https://github.com/Londopy/ridgewarden
git clone https://github.com/Londopy/ridgewarden-lightning
git clone https://github.com/Londopy/ridgewarden-weather
cd ridgewarden-weather
cargo build --release
cargo test
```

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md), and add a line under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
