# Changelog

All notable changes to ridgewarden-weather are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is what appears on its GitHub release page. Validated in CI by [patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

### Added

- Radar rain at each site's own position: the NWS MRMS radar-only estimate of the last hour on a 1 km grid, in steps of 0.1 in, fetched hourly from NOAA's map service and again every few minutes while the hour is late. A reading of 0.1 in or more confirms candidates, keeps the hour out of the baseline and counts as a rain hour in the results; a 0 can still be drizzle, so dry hours still come from gauges. Set `[radar]` to turn it on.
- New links are listed on the front page for two weeks after they first turn up, the 25 longest first, and ones of `long_km` (20 km) or more are flagged as long. With `links` set, the list says when a new link isn't studied.
- A map layer of the links at `/api/links.geojson`, and each link's data as JSON at `/api/link`, with the hours it rained or was dry near either end, linked from its page.
- `channel_width_mhz` per site and under `[export]`, exported as `channel_width`.
- The methods page works out the rain loss on the path from ITU-R P.838-3 for the configured channel.

### Changed

- The baseline is a dry baseline: hours it rained at either end of a link, and the 3 after while the antennas dry (`baseline.dry_after_h`), are left out of it, so a rainy spell no longer drags the usual level down and hides the next wet antenna. An hour of day it rained at on so many days that fewer than `min_days` dry ones are left falls back to every day.
- A link's baseline shows as ready only once every hour of day has `min_days` of history, not once any day has.
- The results page no longer counts the drying hours after rain as dry, reads one link at a time, and works out each period at most every 10 minutes, one request at a time: a year of history no longer needs twice its size in memory.

### Fixed

- Rain on the path at AREDN's 5.7 to 5.9 GHz channels costs about 0.02 dB per km at 10 mm/h, not 0.01 (the 5.0 GHz figure), by ITU-R P.838-3.
- The README and the export said AREDN nodes don't report transmit power; 4.x nodes do, it's the core's polls that don't carry it.
- The export's `time` has the `_FillValue` the OpenSense conventions list.

## [0.1.0] - 2026-11-20

### Added

- Runs as its own system user, `ridgewarden-weather`, in the core's `ridgewarden` group: it can sign events through the core but not read the station key.
- Experimental. Reads every RF link's received level, noise floor and SNR from the core's node polls, at whatever sampling the core gives (one minute with a 60-second crawl of the backbone).
- Takes the daily cycle out of each link: an hour-of-day baseline (median over the previous 21 days) and the residual against it.
- Wet-antenna candidates: a sudden drop of a few dB on several links at one site that recovers over hours while the rest of the mesh stays put, checked against rain reports near that site.
- Ducting and fading candidates: many links far from their baseline at once for half an hour or more, shown with the nearest sounding's inversion and refractivity gradient.
- Rain from APRS weather stations over APRS-IS (read-only) and NWS observations; soundings from the University of Wyoming archive. `import` loads saved files of each.
- A residual chart per link with rain, sounding and candidate markers; a candidates page with the evidence; a methods page with the physics and every rule.
- OpenSense-format export of the link data as a NetCDF classic file, from the page or `export`.
- Detection runs on its own every 10 minutes and keeps candidates after the raw data is pruned.
- Widespread-drop candidates: what the site check turns down as mesh-wide is listed as its own kind instead of being discarded, and a wet candidate inside one gives way to it.
- The ducting rule needs at least 5 links reporting, so three links out of three no longer count.
- A ducting episode with every departing link below its baseline that overlaps a wet or widespread drop is left to the drop.
- Samples that reach the core late (a stalled sync, a core catching up) are scanned again.
- Lightning strikes from ridgewarden-lightning near a site count as supporting evidence for wet and widespread drops.
- A live marine-layer indicator with its evidence and a confidence, on the front page and at `/api/now`.
- Candidates are published to the mesh as signed `weather.candidate` events (the same shape as `/api/events`), when found, when they end, and if a later scan withdraws them.
- Rain readings from APRS-IS and NWS are published as `weather.rain` telemetry, with APRS 24-hour rain now kept too.
- A results page: rain against dry hours and night against day, with 95% intervals, and how many wet candidates rain confirmed.
- Link lengths on the front page; each station's nearest sounding for ducting candidates.
- Wind from APRS reports is kept with each reading; candidates whose sites' stations reported gusts of 30 mph or more are flagged as possibly wind moving a dish, and the indicator's confidence drops.

[Unreleased]: https://github.com/Londopy/ridgewarden-weather/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Londopy/ridgewarden-weather/releases/tag/v0.1.0
