#!/usr/bin/env python3
"""Export your YouTube login from Firefox as a cookies.txt for bman.

bman's "YouTube likes" (Streaming > YouTube Music) needs the cookies of a
browser signed in to YouTube. This reads them straight out of Firefox's
cookie database (only youtube.com and google.com, nothing else) and writes
them in the Netscape cookies.txt format yt-dlp reads.

    python3 youtube-cookies.py --login --to bman@192.168.1.183   # best: a login just for bman
    python3 youtube-cookies.py                      # writes ./youtube-cookies.txt
    python3 youtube-cookies.py --to bman@192.168.1.183   # also copies it to ~/bman/ on the Pi
    python3 youtube-cookies.py --check              # only says whether you're signed in
    python3 youtube-cookies.py --profile NAME_OR_PATH

--login opens a fresh, empty Firefox profile at YouTube Music. Sign in, close
that window, and its login is exported and the profile deleted. Nothing else
ever uses that session, so the browser can't renew it out from under bman
(which is what logs out cookies copied from your everyday profile) and it
lasts until you sign it out at myaccount.google.com > Security > devices.

Only standard Python 3. Firefox can stay open (the database is copied first).
The file is a login to your Google account: keep it private.
"""

import argparse
import configparser
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

DOMAINS = ("youtube.com", "google.com")
# Cookies that are only there when signed in.
LOGIN_COOKIES = ("SAPISID", "__Secure-3PAPISID", "__Secure-3PSID", "LOGIN_INFO")
FIREFOX_DIRS = [
    "~/.config/mozilla/firefox",  # newer Firefox (XDG)
    "~/.mozilla/firefox",
    "~/snap/firefox/common/.mozilla/firefox",
    "~/.var/app/org.mozilla.firefox/.mozilla/firefox",  # Flatpak
    "~/.var/app/org.mozilla.firefox/config/mozilla/firefox",
    "~/Library/Application Support/Firefox",  # macOS
]


def die(msg):
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)


def find_profile(wanted):
    """The profile folder: --profile (path or name), else the one Firefox opens by default."""
    if wanted and Path(wanted, "cookies.sqlite").expanduser().is_file():
        return Path(wanted).expanduser()
    for d in FIREFOX_DIRS:
        root = Path(d).expanduser()
        ini = root / "profiles.ini"
        if not ini.is_file():
            continue
        cfg = configparser.RawConfigParser()
        cfg.read(ini)
        profiles = {}
        default = None
        for section in cfg.sections():
            path = cfg.get(section, "Path", fallback=None)
            if not path:
                continue
            full = root / path if cfg.get(section, "IsRelative", fallback="1") == "1" else Path(path)
            if section.startswith("Profile"):
                profiles[cfg.get(section, "Name", fallback=path)] = full
                if cfg.get(section, "Default", fallback="0") == "1" and default is None:
                    default = full
            elif section.startswith("Install"):
                default = full  # what this Firefox install actually opens
        if wanted:
            for name, full in profiles.items():
                if wanted in (name, full.name):
                    return full
            continue
        if default and (default / "cookies.sqlite").is_file():
            return default
        for full in profiles.values():
            if (full / "cookies.sqlite").is_file():
                return full
    if wanted:
        die(f"no Firefox profile called {wanted}")
    die("couldn't find a Firefox profile (try --profile /path/to/profile)")


def read_cookies(profile):
    # Firefox keeps the database locked and recent changes in the -wal file:
    # copy both and read the copy.
    with tempfile.TemporaryDirectory() as tmp:
        for suffix in ("", "-wal"):
            src = profile / f"cookies.sqlite{suffix}"
            if src.is_file():
                shutil.copy2(src, Path(tmp) / f"cookies.sqlite{suffix}")
        db = sqlite3.connect(Path(tmp) / "cookies.sqlite")
        try:
            where = " OR ".join("host = ? OR host LIKE ?" for _ in DOMAINS)
            args = [a for d in DOMAINS for a in (d, f"%.{d}")]
            rows = db.execute(
                "SELECT host, path, isSecure, expiry, name, value, isHttpOnly, originAttributes "
                f"FROM moz_cookies WHERE {where}",
                args,
            ).fetchall()
        finally:
            db.close()
    # Normal tabs only: skip containers and partitioned (third-party) copies.
    return [r[:7] for r in rows if r[7] == ""]


def find_firefox(cmd):
    if cmd:
        return cmd.split()
    for name in ("firefox", "firefox-esr", "librewolf"):
        path = shutil.which(name)
        if path:
            return [path]
    if shutil.which("flatpak") and subprocess.run(
        ["flatpak", "info", "org.mozilla.firefox"], capture_output=True
    ).returncode == 0:
        return ["flatpak", "run", "org.mozilla.firefox"]
    die("couldn't find Firefox (try --firefox COMMAND)")


def login(firefox_cmd):
    """Sign in in a throwaway profile; return its cookies once Firefox is closed."""
    # Under home, so a Flatpak/Snap Firefox can write to it.
    profile = Path(tempfile.mkdtemp(prefix=".bman-youtube-login-", dir=Path.home()))
    try:
        # Don't restore tabs or ask anything on the way out.
        (profile / "user.js").write_text(
            'user_pref("browser.shell.checkDefaultBrowser", false);\n'
            'user_pref("browser.aboutwelcome.enabled", false);\n'
            'user_pref("datareporting.policy.dataSubmissionEnabled", false);\n'
            'user_pref("browser.sessionstore.resume_from_crash", false);\n'
        )
        print("A new Firefox window opens. Sign in to YouTube Music there, then close it (quit Firefox in that window).")
        cmd = find_firefox(firefox_cmd) + ["--no-remote", "--new-instance", "--profile", str(profile),
                                          "https://accounts.google.com/ServiceLogin?service=youtube"
                                          "&continue=https://music.youtube.com/"]
        subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if not (profile / "cookies.sqlite").is_file():
            die("Firefox didn't save any cookies (was the window closed before signing in?)")
        return read_cookies(profile)
    finally:
        shutil.rmtree(profile, ignore_errors=True)


def to_netscape(rows):
    now = time.time()
    lines = ["# Netscape HTTP Cookie File", "# YouTube login exported from Firefox for bman. Keep it private.", ""]
    for host, path, secure, expiry, name, value, http_only in rows:
        if expiry > 10**11:  # Firefox 13x+ stores milliseconds
            expiry //= 1000
        if expiry and expiry < now:
            continue
        lines.append("\t".join([
            ("#HttpOnly_" if http_only else "") + host,
            "TRUE" if host.startswith(".") else "FALSE",
            path,
            "TRUE" if secure else "FALSE",
            str(int(expiry)),
            name,
            value,
        ]))
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser(description="Export your YouTube login from Firefox for bman.")
    ap.add_argument("-o", "--output", default="youtube-cookies.txt", help="file to write (default: ./youtube-cookies.txt)")
    ap.add_argument("--to", metavar="USER@HOST", help="also copy it to ~/bman/youtube-cookies.txt there with scp, then delete the local file")
    ap.add_argument("--profile", help="Firefox profile name or folder (default: the one Firefox opens)")
    ap.add_argument("--check", action="store_true", help="only say whether Firefox is signed in to YouTube")
    ap.add_argument("--login", action="store_true",
                    help="sign in in a fresh Firefox profile used only for bman (lasts much longer)")
    ap.add_argument("--firefox", metavar="COMMAND", help="how to start Firefox for --login (default: found on PATH)")
    opts = ap.parse_args()

    if opts.login:
        rows = login(opts.firefox)
        where = "the bman login"
    else:
        profile = find_profile(opts.profile)
        rows = read_cookies(profile)
        where = "this profile"
        print(f"Firefox profile: {profile}")
    names = {r[4] for r in rows}
    signed_in = any(n in names for n in LOGIN_COOKIES)
    print(f"{len(rows)} youtube.com / google.com cookies, signed in: {'yes' if signed_in else 'NO'}")
    if not signed_in:
        die(f"not signed in to YouTube in {where} (sign in at youtube.com, or pick another --profile)")
    if opts.check:
        return

    out = Path(opts.output)
    fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(to_netscape(rows))
    os.chmod(out, 0o600)
    print(f"Wrote {out}")

    if not opts.login:
        print("Note: your everyday Firefox keeps renewing this login, which logs the copy out after a while.")
        print("      --login makes one just for bman that lasts.")

    if opts.to:
        dest = f"{opts.to}:~/bman/youtube-cookies.txt"
        if subprocess.run(["scp", "-q", str(out), dest]).returncode != 0:
            die(f"scp to {dest} failed; {out} is still here")
        out.unlink()
        print(f"Copied to {dest} (local copy deleted)")
    else:
        print("Copy it to the bman folder on the Pi, e.g.:")
        print(f"  scp {out} bman@<pi>:~/bman/youtube-cookies.txt && rm {out}")


if __name__ == "__main__":
    main()
