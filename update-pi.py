#!/usr/bin/env python3
"""Update bman on the Pi over SSH, from your PC.

A backup for when bman can't update itself (no update offered, GitHub's
download links failing, or a version too old to fix itself). It downloads the
release build for the Pi on this PC, checks its SHA-256, and puts it in place
of ~/bman/bman over SSH. bman can keep running; restart it afterwards
(Settings > System > Restart bman) and it picks up the new version and carries
on playing.

    python3 update-pi.py bman@192.168.1.183            # latest release
    python3 update-pi.py bman@192.168.1.183 --tag v0.4.0
    python3 update-pi.py bman@192.168.1.183 --file target/aarch64-unknown-linux-gnu/release/bman
    python3 update-pi.py bman@192.168.1.183 --dir /home/bman/bman

Only standard Python 3 plus ssh. One SSH connection is shared by every step,
so a password is asked for once at most.
"""

import argparse
import hashlib
import json
import os
import shlex
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

REPO = "birdguy-cs/bman"
UA = "bman-update-pi"


def die(msg):
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def latest_tag():
    """The tag `releases/latest` redirects to (no API needed), else the API."""
    req = urllib.request.Request(f"https://github.com/{REPO}/releases/latest", method="HEAD",
                                 headers={"User-Agent": UA})
    try:
        urllib.request.build_opener(NoRedirect).open(req, timeout=20)
    except urllib.error.HTTPError as e:
        location = e.headers.get("Location", "")
        if "/releases/tag/" in location:
            return location.rsplit("/releases/tag/", 1)[1]
    except urllib.error.URLError as e:
        die(f"can't reach GitHub: {e.reason}")
    return api_release("latest")["tag_name"]


def api_release(which):
    url = f"https://api.github.com/repos/{REPO}/releases/" + ("latest" if which == "latest" else f"tags/{which}")
    req = urllib.request.Request(url, headers={"User-Agent": UA, "Accept": "application/vnd.github+json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        die(f"GitHub API: {which}: HTTP {e.code}" + (" (no such release)" if e.code == 404 else ""))
    except urllib.error.URLError as e:
        die(f"can't reach GitHub: {e.reason}")


def download(tag, name, assets):
    """A release file. Falls back to the API when github.com's download links
    fail on GitHub's side (they've been seen returning 504 for hours)."""
    url = f"https://github.com/{REPO}/releases/download/{tag}/{name}"
    try:
        with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": UA}), timeout=120) as r:
            return r.read()
    except urllib.error.HTTPError as e:
        if e.code < 500:
            die(f"{name}: HTTP {e.code}" + (f" (no {name} in {tag})" if e.code == 404 else ""))
        print(f"{name}: HTTP {e.code} (GitHub may be down), trying the GitHub API")
    except (urllib.error.URLError, TimeoutError) as e:
        print(f"{name}: {getattr(e, 'reason', e)}, trying the GitHub API")
    if not assets:
        assets.update({a["name"]: a["url"] for a in api_release(tag).get("assets", [])})
    if name not in assets:
        die(f"{name} isn't in release {tag}")
    req = urllib.request.Request(assets[name], headers={"User-Agent": UA, "Accept": "application/octet-stream"})
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            return r.read()
    except urllib.error.HTTPError as e:
        die(f"{name}: HTTP {e.code} from the API too" + (", GitHub may be down" if e.code >= 500 else ""))
    except urllib.error.URLError as e:
        die(f"{name}: {e.reason}")


class Ssh:
    """ssh/scp sharing one connection (ControlMaster), closed at the end."""

    def __init__(self, host):
        self.host = host
        self.tmp = tempfile.mkdtemp(prefix="bman-ssh-")
        self.opts = ["-o", "ControlMaster=auto", "-o", f"ControlPath={self.tmp}/c", "-o", "ControlPersist=60"]

    def run(self, script, data=None):
        r = subprocess.run(["ssh", *self.opts, self.host, script], input=data, capture_output=True)
        if r.returncode == 255:
            die(f"ssh to {self.host} failed: {r.stderr.decode(errors='replace').strip()}")
        return r.returncode, r.stdout.decode(errors="replace").strip(), r.stderr.decode(errors="replace").strip()

    def close(self):
        subprocess.run(["ssh", *self.opts, "-O", "exit", self.host], capture_output=True)
        try:
            os.rmdir(self.tmp)
        except OSError:
            pass


def asset_for(machine, bits):
    if machine == "x86_64":
        return "bman-x86_64-linux-gnu"
    if machine in ("aarch64", "arm64"):
        # A 64-bit kernel with a 32-bit system (older Raspberry Pi OS) runs the armv7 build.
        return "bman-aarch64-linux-gnu" if bits == "64" else "bman-armv7-linux-gnueabihf"
    if machine.startswith("armv7") or machine.startswith("armv8"):
        return "bman-armv7-linux-gnueabihf"
    die(f"no bman build for {machine} (armv6 Pis aren't supported)")


def main():
    ap = argparse.ArgumentParser(description="Update bman on the Pi over SSH.")
    ap.add_argument("host", metavar="USER@HOST", help="the Pi, as for ssh")
    ap.add_argument("--tag", help="release to install, e.g. v0.4.0 (default: the latest)")
    ap.add_argument("--file", help="install this binary instead of downloading a release")
    ap.add_argument("--dir", default="~/bman", help="bman's folder on the Pi (default: ~/bman)")
    opts = ap.parse_args()

    ssh = Ssh(opts.host)
    try:
        # $HOME-relative so ~ works inside the quotes.
        d = opts.dir
        rdir = '"$HOME"/' + shlex.quote(d[2:]) if d.startswith("~/") else shlex.quote(d)
        code, out, _ = ssh.run(f'uname -m; getconf LONG_BIT; {rdir}/bman --version 2>/dev/null || echo none')
        lines = out.splitlines() + ["", "", ""]
        machine, bits, current = lines[0], lines[1], lines[2]
        print(f"{opts.host}: {machine}, {bits}-bit, running {current if current != 'none' else 'no bman in ' + d}")

        if opts.file:
            binary = Path(opts.file).read_bytes()
            label = opts.file
        else:
            asset = asset_for(machine, bits)
            tag = opts.tag or latest_tag()
            label = f"{tag} ({asset})"
            if tag.lstrip("v") in current.split():
                print(f"Already on {tag}; installing it again anyway")
            assets = {}
            print(f"Downloading {label}")
            sha = download(tag, f"{asset}.sha256", assets).decode().split()[0].lower()
            binary = download(tag, asset, assets)
            if hashlib.sha256(binary).hexdigest() != sha:
                die("download corrupted (SHA-256 doesn't match)")
            print("SHA-256 OK")
        if not binary.startswith(b"\x7fELF"):
            die(f"{label} isn't a Linux program")

        digest = hashlib.sha256(binary).hexdigest()
        print(f"Sending {len(binary) // 1024} KB to {opts.host}:{d}/bman")
        # Written next to it and renamed over it: safe while bman is running.
        script = (
            f"set -e; mkdir -p {rdir}; tmp={rdir}/.bman-update.tmp; cat > \"$tmp\"; "
            f"if command -v sha256sum >/dev/null; then "
            f"  [ \"$(sha256sum \"$tmp\" | cut -d' ' -f1)\" = {digest} ] || {{ rm -f \"$tmp\"; echo 'checksum mismatch after copy' >&2; exit 3; }}; fi; "
            f"chmod 755 \"$tmp\"; mv -f \"$tmp\" {rdir}/bman; {rdir}/bman --version"
        )
        code, out, err = ssh.run(script, data=binary)
        if code != 0:
            die(f"install on the Pi failed: {err or out}")
        print(f"Installed: {out}")
        print("Restart bman to run it: Settings > System > Restart bman (it carries on playing).")
    finally:
        ssh.close()


if __name__ == "__main__":
    main()
