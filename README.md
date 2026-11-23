# ridgewarden-weather

[![CI](https://github.com/Londopy/ridgewarden-weather/actions/workflows/ci.yml/badge.svg)](https://github.com/Londopy/ridgewarden-weather/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Londopy/ridgewarden-weather)](https://github.com/Londopy/ridgewarden-weather/releases/latest)
[![changelog](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Londopy/ridgewarden-weather/gh-pages/changelog-badge.json)](CHANGELOG.md)
[![License](https://img.shields.io/github/license/Londopy/ridgewarden-weather)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange)](https://www.rust-lang.org)
[![Linux](https://img.shields.io/badge/platform-Linux%20(Pi%20%7C%20x86__64)-555)](#building-from-source)

**Experimental: use the mesh's own link levels as a weather sensor, and say plainly what they can and can't show.**

A ridgewarden app for AREDN meshes, and a research project rather than something every site installs. Mountaintop links report their signal levels around the clock, and weather moves those numbers. This app records them, takes the daily cycle out of each link, and looks for two effects a 5 GHz link can really show: water on a site's antennas during rain, and coastal ducting and fading. It lists what it finds as candidate events with the evidence, checks them against rain reports and upper-air soundings, and exports the link data in the OpenSense format used for microwave-link rain research.

It does not measure rain. At the 5.7 to 5.9 GHz channels AREDN uses, rain on the path costs about 0.02 dB per km at 10 mm/h (ITU-R P.838-3 with horizontal polarisation; about 0.015 with vertical), so a 30 km path loses about half a dB in steady heavy rain: far too little to map rainfall. The effects it looks for may also turn out too small to separate from equipment temperature, wind and interference; a well-documented negative result is a result too.

It needs a [ridgewarden](https://github.com/Londopy/ridgewarden) core running on the same box, ideally the hub core that crawls the backbone. The core is the only part that talks to the nodes; this app reads its event feed. Spec sheet 09 of the AREDN app suite.

## What it does

- Reads every RF link level from the core's node polls: the received signal (RSL) each node reports for each neighbor, its noise floor and SNR. Where a node reports only SNR and noise, their sum stands in for the signal. Levels outside -130 to 0 dBm are dropped as broken reports.
- Works with whatever sampling it gets. Minute samples are best; with the core's default 15-minute crawl, ducting still shows but the onset of a wet-antenna drop is harder to see. The front page shows each link's sample interval.
- Takes the daily cycle out: each hour is rolled up as a median, and the baseline for a day is, per UTC hour of day, the median of that hour over the previous 21 days (once at least 7 have data). It is a dry baseline: the hours it rained at either end of the link, by the gauges and radar set for the two sites, and the 3 hours after while the antennas dry, are left out of it. An hour of day it rained at on so many days that fewer than 7 dry ones are left takes every day instead, wet ones too. A link's baseline is ready once every hour of day has its 7 days. The residual is each sample minus its baseline, in 10-minute medians, with the two ends of a link averaged.
- The 21-day window rolls, so the baseline follows the season. A fixed baseline per season would need a year of data before it worked and would carry last year's dish alignments and radios over; the cost of the rolling one is that a pattern lasting weeks, such as a summer of nightly ducting, becomes the usual level and leaves the residual.
- Runs detection every 10 minutes, on its own, and keeps every candidate with its evidence after the raw samples behind it are gone.
- Finds wet-antenna candidates: a sudden drop of a few dB on several links ending at one site that recovers over hours, while the rest of the mesh stays put. Each one is checked against the rain reports of the stations near that site, and against lightning strikes logged by [ridgewarden-lightning](https://github.com/Londopy/ridgewarden-lightning) within 30 km.
- Finds widespread drops: when a site's links drop together but so do more than half of the mesh's other links, it is too widespread for one site's antennas. A rain front wetting several sites looks like this, and so does anything hitting every link's measurements; it is listed as its own kind of candidate rather than thrown away. A wet site whose drops all fall inside one gives way to it; one with only some of its drops inside (the leading edge of a front, say) is kept as well.
- Finds ducting and fading candidates: at least 5 links reporting and at least 3 of them (30%) 4 dB or more from their baseline, for at least half an hour. Each is shown with every station's nearest sounding: the inversion and the steepest refractivity gradient.
- A live marine-layer indicator on the front page: calm, disturbed, ducting and fading under way, or a widespread drop, from the newest complete residual bins and the latest sounding, with the evidence and a confidence that drops when few links report, the sounding is old, or the two disagree. `/api/now` has it as JSON.
- Publishes every candidate to the mesh as a signed `weather.candidate` event, so other apps can allow for weather, and the rain readings it fetches as `weather.rain` telemetry (see below).
- A results page for the project's success measure: rain hours against dry hours per link and over all links, night against day for the size of departures, and how many wet candidates rain confirmed, each with a 95% interval and a plain "clear" or "not clear". Each period is worked out at most every 10 minutes, one request at a time, a link at a time: a year of history takes seconds on a Pi.
- Wind: APRS reports carry the wind and the peak gust, kept with each reading. A candidate whose sites' stations reported a gust of 30 mph or more (`gust_mph`) is flagged, since wind moving a dish or mount swings a link much as weather in the path does; the indicator's confidence drops too.
- Link lengths from the nodes' reported positions on the front page, to see which links are long enough to cross the marine layer.
- New links: a link first heard at least a day after the app's first data is listed on the front page for two weeks, the 25 longest first (the rest are in the map layer), and one of 20 km or more (`long_km`) is flagged as long, since long paths near the coast are the ones the marine layer bends. With `links` set, the list says when a new link isn't studied yet.
- Rain from APRS weather stations heard over APRS-IS (most are CWOP members), read-only, from NWS observations on api.weather.gov, and from radar: the NWS MRMS radar-only estimate of the last hour at each site's own position, on a 1 km grid in steps of 0.1 in (2.5 mm). Gauges sit mostly in towns and valleys; radar is the one source for the peak a site is on. Radar marks rain hours only: its 0 can still be light rain or drizzle, so the dry hours come from gauges. Upper-air soundings from the University of Wyoming archive.
- A residual chart per link with rain, sounding and candidate markers, the level and baseline for each direction, and the noise floor (a rising noise floor is interference, not weather).
- A methods page that states the physics and every rule with the thresholds in use, with the rain loss worked out from ITU-R P.838-3 for the channel in the config.
- Exports the link data as a NetCDF file in the OpenSense CML conventions, from the page or the command line, with each site's frequency and channel width from the config.
- A map layer: `/api/links.geojson` has every link as a line between its nodes' reported positions, with its length, whether it is new, long and studied, its residual (when the latest sample is under half an hour old) and the kinds of candidate it was part of in the last day, and every node as a point.
- A feed per link: `/api/link?a=<node>&b=<node>&days=2|7|30` is one link's data as JSON, the same as its page draws: level, baseline, residual and noise floor for each direction in 10-minute bins (hourly for 30 days), the rain sources near either end and each hour they called wet or dry (drying hours after rain are neither), and the candidates it was part of. Each link's page links to it.
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

The pages are on `http://<pi>:8801/`: Now (the marine-layer indicator, recent candidates, each link's sampling, baseline and latest residual, new links, the weather sources), Events, Results, Methods, and Dataset. Add it to the core's dashboard with an `[[app]]` entry in `ridgewarden.toml`.

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
long_km = 20.0         # new links this long are flagged: the marine layer bends long paths

[baseline]
days = 21
min_days = 7
bin_min = 10
dry_after_h = 3        # hours of rain at either end, and this many after, stay out of it

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
frequency_mhz = 5745.0 # for the export and the rain physics on the methods page
channel_width_mhz = 10.0  # for the export
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

[radar]
product = "conus_QPE_01H"  # or alaska_QPE_01H, hawaii_QPE_01H, carib_QPE_01H

[export]
title = "AREDN backbone link levels"
author = "N0CALL (you@example.org)"
licence = "CC-BY-4.0"
frequency_mhz = 5745.0    # for sites without their own
channel_width_mhz = 10.0  # for sites without their own
```

A site with a single link can't be a wet-antenna candidate under the default `min_links = 2`: one link dropping can't say which end got wet.

### Weather sources

All four are optional, and every fetch has a timeout.

- **APRS-IS.** One read-only connection with a budlist filter (`b/CW0000/...`), so the server sends only the configured stations. The `r` field of an APRS weather report (APRS 1.0.1, chapter 12) is rain in the last hour in hundredths of an inch. Parsing is shared with ridgewarden-lightning.
- **NWS.** `GET /stations/{id}/observations?start=...` on api.weather.gov every 30 minutes, with your `user_agent` (the API refuses requests without one). Each observation's `precipitationLastHour` (mm, on the hourly reports) and `textDescription` ("Light Rain") are kept.
- **Radar.** NOAA's map service for the NWS MRMS radar-only rain estimates, `GET https://mapservices.weather.noaa.gov/raster/rest/services/obs/mrms_qpe/ImageServer/getSamples` with the position of every node at either end of a studied link (as the nodes report it, 50 points per request) and the newest `conus_QPE_01H` raster, ten minutes past each hour. The answer is the rain in the hour, in inches, at each point on a 1 km grid, in steps of 0.1 in (2.5 mm), -3 where no radar covers it. Each reading is kept as a rain report from source `mrms` with the node's name as the station. One of 0.1 in or more is a rain hour at the site: it confirms candidates, keeps that hour out of the baseline and counts as a rain hour in the results. A 0 can still be light rain or drizzle, so it is never a dry hour: dry hours, for the results and for calling a candidate dry, come from gauges. NOAA posts each hour some minutes after it ends, and the service keeps only the latest, so while the newest raster is still the hour before (or, on a fresh start, no studied site has been heard yet), the app asks again every 4 minutes, and after a failed fetch every 15, until 50 past; hours it misses altogether (down, or no internet) are gone. Radar-only estimates miss rain falling from below the beam, which marine-layer drizzle often does, and they cover the US only.
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
- `evidence.rain` is `{"verdict": "rain", ...}`, `{"verdict": "dry", "reports": n}`, `{"verdict": "none"}` (no reports for that time), or null (no station or radar set, or ducting). `rain_stations` lists the gauges and, with radar on, the sites themselves. `strikes` is null for ducting and when no site involved has a known position, and a count of 0 can also mean no lightning receivers cover the area. `soundings` (ducting) lists `station`, `at_ms`, `inversion_base_m`, `inversion_top_m`, `inversion_c`, `gradient_n_per_km`, `gradient_lo_m` and `gradient_hi_m`.

### weather.rain

Telemetry events (pruned with the station's other telemetry) carrying the rain readings fetched from APRS-IS, NWS and radar, so other apps (ridgewarden-lightning's dry-lightning check, say) can use them:

```json
{"v": 1, "readings": [
  {"station": "CW0001", "source": "aprs", "at_ms": 1795098000000, "rain_1h_mm": 5.08, "rain_24h_mm": 31.75, "text": null, "wind_dir_deg": 160.0, "wind_mph": 5.0, "gust_mph": 12.0},
  {"station": "KSBP", "source": "nws", "at_ms": 1795089360000, "rain_1h_mm": 0.0, "rain_24h_mm": null, "text": "Cloudy", "wind_dir_deg": null, "wind_mph": null, "gust_mph": null}
]}
```

At most 200 readings per event, each sent once, and again if a later fetch changes it. Only readings from the last two days go out, so an imported log or turning publishing on later doesn't flood the mesh. `source` is `aprs`, `nws` or `mrms`; a radar (`mrms`) reading's `station` is the node it was read at, and ridgewarden-lightning skips those, since it reads gauges by callsign. Radar comes in steps of 0.1 in (2.54 mm), so its 0 doesn't mean dry. `rain_1h_mm` and `rain_24h_mm` are null when the report didn't carry them (NWS and radar have no 24-hour figure); `text` is the NWS observer's description. `wind_dir_deg` (where the wind blows from), `wind_mph` (sustained) and `gust_mph` (peak in the last five minutes) come from APRS reports only. A report with only a gust and no rain figure is sent too. Readings are sent every 10 minutes, with the analysis round.

## The dataset

The export follows the [OpenSense data format conventions](https://github.com/OpenSenseAction/OS_data_format_conventions) for commercial microwave link (CML) data (`netCDF_CML.adoc` and `netCDF_global_attributes.adoc`), and is written as a real NetCDF classic file (64-bit offset format), not CSV.

- Dimensions `cml_id`, `sublink_id`, `time`. Each RF link is one CML, `cml_id` "node-a|node-b" with the names in order. `sublink_1` runs from site 0 to site 1 (its RSL is what site 1's node hears), `sublink_2` the other way.
- `time` is seconds since 1970-01-01 00:00:00 UTC on a regular axis; each value is the last sample in (time - step, time]. It carries the `_FillValue` the conventions list, though no time is ever missing.
- `site_0_lat`, `site_0_lon`, `site_1_lat`, `site_1_lon` from the nodes' own reported positions; `site_0_alt`, `site_1_alt` and `frequency` from the config; `length` from the positions.
- `rsl` (dBm, `sampling = "instantaneous"`), and the extras `noise_level` (dBm) and `snr` (dB) as the nodes report them, and `channel_width` (MHz, per sublink, from the config like `frequency`): the noise floor rises 3 dB each time the channel width doubles, so noise and SNR only compare between links of the same width. Missing values are the NetCDF default fill.
- There is no `tsl`: the core's polls don't carry transmit power (AREDN 4.x nodes report it, see Limits), and the conventions allow leaving out a level that is constant, which a set transmit power is. `polarisation` is left out because it isn't known.
- Global attributes `title`, `file_author(s)`, `institution`, `date`, `source`, `version`, `history`, `naming convention` ("OpenSense-CML"), `licence`, `reference`, `comment`.
- `time` is a fixed-length dimension rather than the unlimited one the conventions suggest: an export is written once, never appended to, and readers treat the two alike.

The tests check the writer in three ways: a tiny file matches the format specification byte for byte; every export is read back with the app's own reader; and an export is checked against the parts of the NetCDF User Guide and the CF conventions that readers such as xarray rely on (names, a `_FillValue` of each variable's own type, coordinate variables that rise along their own dimension, strings as character arrays with the length last, time units that parse, `coordinates` naming variables with matching dimensions) and against the OpenSense table's dimensions, units and long names. The file doesn't claim to be CF: OpenSense's own units (`dBm`, `degrees_in_WGS84_projection`) aren't CF units. Opening a file in xarray isn't part of the tests. To check one yourself:

```sh
ncdump -h links.nc
python -c "import xarray; print(xarray.open_dataset('links.nc'))"
```

Rain gauge data is not part of the export (OpenSense has a separate format for gauges).

## Limits

Not tested on real hardware. Everything here has run only against the core's simulated mesh (ridgewarden-sim) and in the tests on simulated link data and saved weather files: it has never seen a real backbone, real AREDN nodes' link levels in rain or under the marine layer, or a Pi in the field. The rules and thresholds are reasoned from the literature and checked on simulated data only. The outside sources are read in the formats their documentation and live answers showed when the code was written, kept as test fixtures.

Not done from the spec, and why:

- The paper with a faculty advisor: not software.
- Feeding the personal weather model: that model doesn't exist as an app, so candidates are published as `weather.candidate` events any app can read instead.
- AREDN's Prometheus metrics as a cheaper source than the crawl: the core would have to poll them (a core change, not this app's); the crawl's load is listed under Install so it can be agreed with node owners.
- Publishing the dataset somewhere public: a person has to upload the file (Zenodo, say); the app writes it.
- Channel width, frequency and transmit power from the nodes themselves: AREDN reports all three in sysinfo's `meshrf` (transmit power on 4.x only), but the core's polls don't carry them yet, a core change. Until then frequency and channel width come from the config, there is no `tsl`, and a node whose channel width or power is changed shows a step in its levels that the app can't tell from anything else (the wet-antenna rule ignores steps that don't recover; the baseline takes up to 11 days to catch up).
- Radar history: NOAA's map service has only the latest hour, so radar can't be filled in for the past or for hours the app was down. The full MRMS archive is GRIB2, which would need a GRIB2 and deflate decoder written into the app.

Left to the owner, since they need people or the real mesh:

- A hub core crawling the backbone once a minute, agreed with the node owners, and a week of history before anything is found.
- The success measure: a clear effect or a documented negative result needs months of real data.
- Which links are longest and best placed across the marine layer: the front page shows lengths and flags new long links, but the choice needs the real mesh.
- Whether Vandenberg soundings come often enough to use: the app fetches and shows what the archive has.
- A faculty advisor in EE or atmospheric science, and the paper.
- Uploading the dataset.

Other limits:

- Experimental. The candidates are leads, not measurements. Radio temperature, sun on a dish, wind moving a mount, rate changes and interference all move link levels; the daily part of those is taken out, the rest is why everything is a candidate.
- Nothing is found until links have 7 days of history, and a site's wet-antenna check needs weather stations or radar for it. The marine-layer indicator says "can't tell" until then.
- The dry baseline is only as good as the rain sources: a link with no gauge or radar at either end keeps every hour in its baseline, and rain nobody reported stays in.
- At an hour of day it rains at on most days (afternoon storms through a wet month, say), the baseline falls back to every day, wet ones too, so a wet antenna then is measured against a partly wet level and looks smaller than it is.
- Radar can't say an hour was dry, so a link with radar and no gauge has no dry hours: the results page's rain test needs gauges near at least one end.
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
