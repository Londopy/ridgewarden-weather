# Changelog

All notable changes to ridgewarden-weather are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is what appears on its GitHub release page. Validated in CI by [patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

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
