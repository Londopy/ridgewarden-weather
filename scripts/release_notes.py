"""Build the GitHub release page for a ridgewarden-weather tag.

Usage: release_notes.py <tag> <dist-dir>
Writes body.md and title.txt in the current directory. Exits 1 if CHANGELOG.md has no
section for the version, so a release never goes out with empty notes.
"""

import hashlib
import json
import urllib.parse
import re
import subprocess
import sys
from pathlib import Path

REPO = "Londopy/ridgewarden-weather"



# in SHA256SUMS.txt, but not rows of their own in the download table
HIDDEN = re.compile(r"(\.torrent|\.magnet\.png)$")
# no torrents for these: tiny, and install.sh fetches the latest release anyway
NO_TORRENT = re.compile(r"(\.torrent|\.png|\.txt|\.sig|^install(-all)?\.sh)$")
# public trackers help magnet links find peers; the web seed covers no-peer cases
TRACKERS = [
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
]
PIECE = 1 << 20


def bencode(x):
    if isinstance(x, int):
        return b"i%de" % x
    if isinstance(x, str):
        x = x.encode()
    if isinstance(x, bytes):
        return b"%d:" % len(x) + x
    if isinstance(x, list):
        return b"l" + b"".join(bencode(i) for i in x) + b"e"
    if isinstance(x, dict):
        return b"d" + b"".join(bencode(k) + bencode(x[k]) for k in sorted(x)) + b"e"
    raise TypeError(type(x))


def make_torrent(path: Path, url: str, comment: str) -> tuple[bytes, str, int]:
    """Single-file torrent with `url` as its web seed: one file per torrent, because
    GitHub redirects each download to its CDN and that only works cleanly per file."""
    pieces, size = [], 0
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(PIECE), b""):
            pieces.append(hashlib.sha1(chunk).digest())
            size += len(chunk)
    info = {"name": path.name, "length": size, "piece length": PIECE, "pieces": b"".join(pieces)}
    meta = {
        "info": info,
        "url-list": [url],
        "announce": TRACKERS[0],
        "announce-list": [[t] for t in TRACKERS],
        "created by": "scripts/release_notes.py",
        "comment": comment,
    }
    return bencode(meta), hashlib.sha1(bencode(info)).hexdigest(), size


def magnet_link(infohash: str, name: str, size: int, url: str) -> str:
    parts = [f"xt=urn:btih:{infohash}", f"dn={urllib.parse.quote(name)}", f"xl={size}"]
    parts += [f"tr={urllib.parse.quote(t, safe='')}" for t in TRACKERS]
    parts.append(f"ws={urllib.parse.quote(url, safe='')}")
    return "magnet:?" + "&".join(parts)


def torrents(tag: str, dist: Path) -> None:
    """Write a .torrent and a magnet QR code next to each download, and torrents.json
    for the release page. Runs before SHA256SUMS.txt is made and signed, so the
    checksums cover the torrents too."""
    import segno

    url = f"https://github.com/{REPO}/releases/download/{tag}"
    made = []
    for p in sorted(q for q in dist.iterdir() if q.is_file()):
        if NO_TORRENT.search(p.name):
            continue
        data, infohash, size = make_torrent(p, f"{url}/{p.name}", f"{REPO} {tag}: the GitHub download is the web seed")
        (dist / f"{p.name}.torrent").write_bytes(data)
        magnet = magnet_link(infohash, p.name, size, f"{url}/{p.name}")
        segno.make(magnet, error="m").save(str(dist / f"{p.name}.magnet.png"), scale=3, border=2)
        made.append({"file": p.name, "magnet": magnet})
    Path("torrents.json").write_text(json.dumps(made, indent=1), encoding="utf-8")


def torrent_section(url: str) -> str:
    path = Path("torrents.json")
    if not path.exists():
        return ""
    made = json.loads(path.read_text(encoding="utf-8"))
    if not made:
        return ""
    rows = [f'| `{t["file"]}` | [`{t["file"]}.torrent`]({url}/{t["file"]}.torrent) | '
            f'<img src="{url}/{t["file"]}.magnet.png" width="200" alt="QR code of the magnet link for {t["file"]}"> |'
            for t in made]
    magnets = [f'# {t["file"]}{chr(10)}{t["magnet"]}{chr(10)}' for t in made]
    return ("## Torrents" + chr(10) * 2
            + "Every download is also a `.torrent` with its GitHub download as the web seed: a BitTorrent client "
              "gets it from GitHub when nobody is seeding, and from peers when they are. The magnet links carry "
              "the same web seed, but need one peer to fetch the file list first." + chr(10) * 2
            + "| File | Torrent | Magnet (QR) |" + chr(10) + "|---|---|---|" + chr(10)
            + chr(10).join(rows) + chr(10) * 2
            + "<details><summary>Magnet links</summary>" + chr(10) * 2 + "```" + chr(10)
            + chr(10).join(magnets) + "```" + chr(10) * 2 + "</details>" + chr(10) * 2)


def changelog_section(version: str) -> str:
    text = Path("CHANGELOG.md").read_text(encoding="utf-8")
    m = re.search(rf"^## \[{re.escape(version)}\][^\n]*\n(.*?)(?=^## \[|^\[[^\]]+\]: )", text, re.M | re.S)
    if not m or not m.group(1).strip():
        sys.exit(f"CHANGELOG.md has no section for {version}")
    return m.group(1).strip()


def size(n: int) -> str:
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024 or unit == "GB":
            return f"{n:.0f} {unit}" if unit == "B" else f"{n:.1f} {unit}"
        n /= 1024


def previous_tag(tag: str) -> str | None:
    try:
        out = subprocess.run(["git", "describe", "--tags", "--abbrev=0", f"{tag}^"],
                             capture_output=True, text=True, check=True)
        return out.stdout.strip() or None
    except subprocess.CalledProcessError:
        return None


def describe(name: str) -> tuple[str, str]:
    if name.endswith(".deb"):
        arch = "Raspberry Pi (arm64)" if "arm64" in name else "x86_64"
        return arch, "Debian / Raspberry Pi OS package (needs the ridgewarden package)"
    if name.endswith(".tar.gz"):
        arch = "Raspberry Pi (aarch64)" if "aarch64" in name else "x86_64"
        return arch, "Static binary, systemd unit, README and license"
    if name == "install.sh":
        return "Linux", "Script install: picks the build, checks it with your installed core"
    if name == "SHA256SUMS.txt":
        return "all", "Checksums of every file"
    if name == "SHA256SUMS.txt.sig":
        return "all", "Release signature over the checksums"
    return "", ""


def main() -> None:
    if sys.argv[1] == "torrents":
        torrents(sys.argv[2], Path(sys.argv[3]))
        return
    tag, dist = sys.argv[1], Path(sys.argv[2])
    version = tag.lstrip("v")
    url = f"https://github.com/{REPO}/releases/download/{tag}"
    files = sorted(p for p in dist.iterdir() if p.is_file() and not HIDDEN.search(p.name))
    order = {".deb": 0, ".gz": 1, ".sh": 2, ".txt": 3, ".sig": 4}
    files.sort(key=lambda p: (order.get(p.suffix, 9), p.name))

    rows = []
    for p in files:
        target, what = describe(p.name)
        rows.append(f"| [`{p.name}`]({url}/{p.name}) | {target} | {size(p.stat().st_size)} | {what} |")

    sums = (dist / "SHA256SUMS.txt").read_text(encoding="utf-8").strip()
    prev = previous_tag(tag)
    compare = (f"https://github.com/{REPO}/compare/{prev}...{tag}" if prev
               else f"https://github.com/{REPO}/commits/{tag}")

    body = f"""**ridgewarden-weather** is a ridgewarden app. An experiment: wet-antenna and ducting signals from link data, joined with rain and sounding data, exported as an open dataset. It runs next to the [ridgewarden core](https://github.com/Londopy/ridgewarden). See the [README](https://github.com/{REPO}#readme).

## What's new in {version}

{changelog_section(version)}

## Downloads

| File | For | Size | Notes |
|---|---|---|---|
{chr(10).join(rows)}

## Install

On a box that already runs the ridgewarden core:

```sh
curl -fsSL https://github.com/{REPO}/releases/latest/download/install.sh | sudo sh
```

Debian package: `sudo apt install ./ridgewarden-weather_{version}_arm64.deb` (or `_amd64`). It writes `/etc/ridgewarden/weather.toml` if there isn't one and starts the service once the core is set up. The page is on port 8801.

## Verify a download

```sh
sha256sum -c SHA256SUMS.txt --ignore-missing
ridgewarden verify-release SHA256SUMS.txt
```

The second command checks the release signature with the key your installed core trusts, the same key that signs core releases.

<details><summary>Checksums</summary>

```
{sums}
```

</details>

{torrent_section(url)}---

**Full changelog:** {compare}

[README](https://github.com/{REPO}#readme) | [Changelog](https://github.com/{REPO}/blob/main/CHANGELOG.md) | [Report a problem](https://github.com/{REPO}/issues)
"""
    Path("body.md").write_text(body, encoding="utf-8")
    Path("title.txt").write_text(f"ridgewarden-weather {version}\n", encoding="utf-8")


if __name__ == "__main__":
    main()
