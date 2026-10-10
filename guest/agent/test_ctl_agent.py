#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Unit tests for conduit-ctl-agent. Headless: no display, no virtio port; a
socketpair stands in for the channel and fixture directories for /proc, the
XDG data dirs, icon themes and Steam.

    python3 -m unittest -v test_ctl_agent        (in guest/agent)
"""

import base64
import hashlib
import importlib.machinery
import importlib.util
import json
import os
import shutil
import socket
import struct
import subprocess
import tempfile
import threading
import time
import unittest
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
_loader = importlib.machinery.SourceFileLoader(
    "conduit_ctl_agent", os.path.join(HERE, "conduit-ctl-agent"))
_spec = importlib.util.spec_from_loader(_loader.name, _loader)
ca = importlib.util.module_from_spec(_spec)
_loader.exec_module(ca)
ca._quiet = True
ca.USE_SYSTEMD_RUN = False


def make_png(w, h):
    def chunk(t, d):
        c = struct.pack(">I", len(d)) + t + d
        return c + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    raw = b"".join(b"\0" + b"\x10\x20\x30\xff" * w for _ in range(h))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


SVG = b'<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" width="48" height="48"><rect width="48" height="48"/></svg>'
JPEG = b"\xff\xd8\xff\xe0" + b"\0" * 64


def rd(path):
    with open(path, "rb") as f:
        return f.read()


def b64(b):
    return base64.b64encode(b).decode()


def write(path, data, mode=None):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb" if isinstance(data, bytes) else "w") as f:
        f.write(data)
    if mode:
        os.chmod(path, mode)
    return path


def desktop(name, exec_="true", **kw):
    lines = ["[Desktop Entry]", "Type=Application", "Name=" + name, "Exec=" + exec_]
    lines += ["%s=%s" % kv for kv in kw.items()]
    return "\n".join(lines) + "\n"


class Fixture(unittest.TestCase):
    """An Agent over a private home, XDG tree and fake /proc."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="ctl-test-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        T = self.tmp
        self.home = os.path.join(T, "home")
        self.bin = os.path.join(T, "bin")
        self.share = os.path.join(T, "share")
        self.proc = os.path.join(T, "proc")
        self.x11 = os.path.join(T, "x11")
        self.runroot = os.path.join(T, "runroot")
        self.rt = os.path.join(self.runroot, str(os.getuid()))
        self.flatpak = os.path.join(T, "var/lib/flatpak/exports/share")
        self.snap = os.path.join(T, "var/lib/snapd/desktop")
        for d in (self.home, self.bin, self.share, self.proc, self.x11, self.rt):
            os.makedirs(d)
        write(os.path.join(self.rt, "wayland-0"), b"")
        self.environ = {"PATH": self.bin, "XDG_RUNTIME_DIR": self.rt,
                        "XDG_DATA_DIRS": self.share, "XDG_CURRENT_DESKTOP": "GNOME",
                        "WAYLAND_DISPLAY": "wayland-0", "HOME": self.home}
        self.calls = []
        self.run_results = {}
        self.agent = self.make_agent()

    def fake_run(self, argv, env=None, timeout=3.0, stdin=None):
        self.calls.append(argv)
        for key, out in self.run_results.items():
            if argv[:len(key)] == list(key):
                return out(argv) if callable(out) else out
        return None

    def make_agent(self, **kw):
        a = ca.Agent(home=self.home, environ=dict(self.environ), uid=os.getuid(),
                     user="tester", proc=self.proc, runtime_root=self.runroot,
                     x11_dir=self.x11, system_dirs=False, run=self.fake_run)
        a.flatpak_exports = [self.flatpak, os.path.join(self.home, ".local/share/flatpak/exports/share")]
        a.snap_dirs = [self.snap]
        a.pixmaps = [os.path.join(self.share, "pixmaps")]
        a.extra_path = []
        return a

    def script(self, name, body):
        return write(os.path.join(self.bin, name), "#!/bin/sh\n" + body, 0o755)

    def call(self, op, agent=None, **fields):
        req = {"v": 1, "id": 7, "op": op}
        req.update(fields)
        line = (agent or self.agent).handle_line(json.dumps(req).encode())
        self.assertTrue(line.endswith(b"\n"))
        out = json.loads(line)
        self.assertEqual(out["id"], 7)
        return out

    def ok(self, op, **fields):
        out = self.call(op, **fields)
        self.assertTrue(out["ok"], out)
        return out

    def err(self, op, **fields):
        out = self.call(op, **fields)
        self.assertFalse(out["ok"], out)
        return out["error"]

    def wait_for(self, path, timeout=5.0):
        end = time.time() + timeout
        while time.time() < end:
            if os.path.exists(path) and os.path.getsize(path) > 0:
                with open(path) as f:
                    return f.read().strip()
            time.sleep(0.02)
        self.fail("%s never appeared" % path)


# ------------------------------------------------------------- pure parts

class LineReaderTest(unittest.TestCase):
    def test_split_and_partial(self):
        r = ca.LineReader()
        self.assertEqual(r.push(b'{"a":1}\n{"b"'), [("line", b'{"a":1}')])
        self.assertEqual(r.push(b':2}\n\n'), [("line", b'{"b":2}')])

    def test_oversize_resyncs_at_newline(self):
        r = ca.LineReader(max_line=16)
        out = r.push(b'{"id":5,"x":"' + b"a" * 100)
        self.assertEqual([k for k, _ in out], ["oversize"])
        self.assertIn(b'"id":5', out[0][1])
        self.assertEqual(r.push(b"bbbb\ngood\n"), [("line", b"good")])

    def test_exactly_max_is_kept(self):
        r = ca.LineReader(max_line=4)
        self.assertEqual(r.push(b"abcd\n"), [("line", b"abcd")])


class ExecTest(unittest.TestCase):
    def test_split_quotes(self):
        self.assertEqual(ca.split_exec('env FOO="a b" app'), ["env", "FOO=a b", "app"])
        self.assertEqual(ca.split_exec('"/opt/My App/run" --x=1  y'),
                         ["/opt/My App/run", "--x=1", "y"])
        self.assertEqual(ca.split_exec('sh -c "echo \\\\"hi\\\\""'), ["sh", "-c", 'echo "hi"'])
        self.assertEqual(ca.split_exec('a "" b'), ["a", "", "b"])
        self.assertEqual(ca.split_exec('a "x \\\\$HOME" '), ["a", "x $HOME"])

    def test_field_codes(self):
        def ex(s, **kw):
            return ca.expand_exec(ca.split_exec(s), **kw)
        self.assertEqual(ex("app %f"), ["app"])
        self.assertEqual(ex("app %u", files=["a b"]), ["app", "a b"])
        self.assertEqual(ex("app %F", files=["a", "b"]), ["app", "a", "b"])
        self.assertEqual(ex("app %U"), ["app"])
        self.assertEqual(ex("app --file=%f"), ["app", "--file="])
        self.assertEqual(ex("app %i", icon="ico"), ["app", "--icon", "ico"])
        self.assertEqual(ex("app %i"), ["app"])
        self.assertEqual(ex("app --name=%c %k", name="My App", path="/x.desktop"),
                         ["app", "--name=My App", "/x.desktop"])
        self.assertEqual(ex("app 100%% %d %D %n %N %v %m"), ["app", "100%"])
        self.assertEqual(ex('app ""'), ["app", ""])

    def test_takes_files(self):
        self.assertTrue(ca.exec_takes_files(["a", "%U"]))
        self.assertTrue(ca.exec_takes_files(["a", "--f=%f"]))
        self.assertFalse(ca.exec_takes_files(["a", "100%%f"]))

    def test_unescape(self):
        self.assertEqual(ca.unescape_value("a\\sb\\\\c\\n"), "a b\\c\n")

    def test_locales(self):
        self.assertEqual(ca.locale_candidates({"LANG": "nb_NO.UTF-8"}), ["nb_NO", "nb"])
        self.assertEqual(ca.locale_candidates({"LANG": "sr_RS@latin"}),
                         ["sr_RS@latin", "sr_RS", "sr@latin", "sr"])
        self.assertEqual(ca.locale_candidates({"LANG": "C"}), [])
        self.assertEqual(ca.locale_candidates({"LC_ALL": "de_DE.UTF-8", "LANG": "fr_FR"}), ["de_DE", "de"])


class VdfTest(unittest.TestCase):
    def test_nested_and_escapes(self):
        d = ca.parse_vdf('''
        // comment
        "AppState"
        {
            "appid"  "730"
            "name"   "A \\"quoted\\" game\\\\x"
            "Inner" { "k" "v" }
            "StateFlags" "4"
        }''')
        st = d["AppState"]
        self.assertEqual(st["appid"], "730")
        self.assertEqual(st["name"], 'A "quoted" game\\x')
        self.assertEqual(st["Inner"], {"k": "v"})

    def test_truncated_does_not_raise(self):
        self.assertEqual(ca.parse_vdf('"a" { "b"'), {"a": {}})
        self.assertEqual(ca.parse_vdf(""), {})


# ---------------------------------------------------------------- session

class SessionTest(Fixture):
    def disc(self, environ=None, run=None):
        return ca.discover_session(os.getuid(), "tester", environ if environ is not None else self.environ,
                                   self.proc, self.runroot, self.x11, run or self.fake_run)

    def test_own_env(self):
        s = self.disc()
        self.assertTrue(s["found"])
        self.assertEqual(s["source"], "env")
        self.assertEqual(s["env"]["WAYLAND_DISPLAY"], "wayland-0")
        self.assertEqual(s["env"]["DBUS_SESSION_BUS_ADDRESS"], "unix:path=%s/bus" % self.rt)
        self.assertEqual(s["env"]["XDG_SESSION_TYPE"], "wayland")

    def test_stale_wayland_socket_is_dropped(self):
        env = dict(self.environ, WAYLAND_DISPLAY="wayland-9")
        s = self.disc(env)
        self.assertEqual(s["env"].get("WAYLAND_DISPLAY"), "wayland-0")  # found by the socket scan
        self.assertEqual(s["source"], "sockets")

    def test_systemd_environment(self):
        os.unlink(os.path.join(self.rt, "wayland-0"))
        write(os.path.join(self.rt, "wayland-1"), b"")
        self.run_results[("systemctl", "--user", "show-environment")] = (
            "WAYLAND_DISPLAY=wayland-1\nXDG_CURRENT_DESKTOP=KDE\nFOO=$'a\\nb'\nbad line\n")
        env = {"XDG_RUNTIME_DIR": self.rt, "PATH": "/bin"}
        s = self.disc(env)
        self.assertTrue(s["user_manager"])
        self.assertEqual(s["env"]["WAYLAND_DISPLAY"], "wayland-1")
        self.assertEqual(s["env"]["XDG_CURRENT_DESKTOP"], "KDE")
        self.assertEqual(s["source"], "systemd")

    def _fake_session_procs(self):
        # leader (a session script) with a thin environ, its child the compositor.
        write(os.path.join(self.proc, "100/environ"), b"XDG_SESSION_ID=2\0")
        write(os.path.join(self.proc, "100/task/100/children"), "101 ")
        write(os.path.join(self.proc, "101/environ"),
              b"WAYLAND_DISPLAY=wayland-1\0HYPRLAND_INSTANCE_SIGNATURE=abc_123\0"
              b"XDG_CURRENT_DESKTOP=Hyprland\0DBUS_SESSION_BUS_ADDRESS=unix:path=/x/bus\0")
        write(os.path.join(self.rt, "wayland-1"), b"")

    def test_loginctl_leader_and_children(self):
        self._fake_session_procs()
        uid = str(os.getuid())
        self.run_results[("loginctl", "list-sessions")] = (
            " c9 %s other seat0 -\n 2 %s tester seat0 tty1\n" % (int(uid) + 1, uid))
        self.run_results[("loginctl", "show-session", "2")] = (
            "Type=wayland\nLeader=100\nDisplay=\nState=active\nActive=yes\nClass=user\nName=tester\n")
        s = self.disc({"XDG_RUNTIME_DIR": self.rt, "PATH": "/bin"})
        self.assertTrue(s["found"])
        self.assertEqual(s["source"], "loginctl")
        self.assertEqual(s["env"]["HYPRLAND_INSTANCE_SIGNATURE"], "abc_123")
        self.assertEqual(s["env"]["WAYLAND_DISPLAY"], "wayland-1")
        self.assertEqual(s["env"]["DBUS_SESSION_BUS_ADDRESS"], "unix:path=/x/bus")
        self.assertEqual(s["env"]["XDG_SESSION_TYPE"], "wayland")

    def test_loginctl_skips_greeter_and_picks_active(self):
        self._fake_session_procs()
        uid = str(os.getuid())
        self.run_results[("loginctl", "list-sessions")] = " c1 %s tester s -\n 2 %s tester s -\n" % (uid, uid)

        def show(argv):
            if argv[2] == "c1":
                return "Type=wayland\nLeader=999\nState=online\nActive=no\nClass=greeter\nName=tester\n"
            return "Type=wayland\nLeader=100\nState=active\nActive=yes\nClass=user\nName=tester\n"
        self.run_results[("loginctl", "show-session")] = show
        s = self.disc({"XDG_RUNTIME_DIR": self.rt})
        self.assertEqual(s["env"]["WAYLAND_DISPLAY"], "wayland-1")

    def test_children_without_task_children_file(self):
        write(os.path.join(self.proc, "100/environ"), b"")
        write(os.path.join(self.proc, "101/stat"), "101 (my (prog)) S 100 1 1 0 -1\n")
        write(os.path.join(self.proc, "102/stat"), "102 (other) S 5 1 1 0 -1\n")
        self.assertEqual(ca.children_of(100, self.proc), [101])

    def test_proc_scan_finds_compositor(self):
        write(os.path.join(self.proc, "300/comm"), "Hyprland\n")
        write(os.path.join(self.proc, "300/environ"), b"WAYLAND_DISPLAY=wayland-0\0SWAYSOCK=/s\0")
        write(os.path.join(self.proc, "301/comm"), "bash\n")
        s = self.disc({"XDG_RUNTIME_DIR": self.rt})
        self.assertEqual(s["source"], "proc")
        self.assertEqual(s["env"]["SWAYSOCK"], "/s")

    def test_x11_socket_scan(self):
        os.unlink(os.path.join(self.rt, "wayland-0"))
        write(os.path.join(self.x11, "X1"), b"")
        s = self.disc({"XDG_RUNTIME_DIR": self.rt})
        self.assertEqual(s["env"]["DISPLAY"], ":1")
        self.assertEqual(s["env"]["XDG_SESSION_TYPE"], "x11")
        self.assertEqual(s["source"], "sockets")

    def test_nothing_found_defaults(self):
        os.unlink(os.path.join(self.rt, "wayland-0"))
        s = self.disc({})
        self.assertFalse(s["found"])
        self.assertEqual(s["env"]["XDG_RUNTIME_DIR"], self.rt)
        self.assertEqual(s["env"]["DBUS_SESSION_BUS_ADDRESS"], "unix:path=%s/bus" % self.rt)

    def test_cache_ttl_and_invalidate(self):
        n = []
        c = ca.SessionCache(lambda: n.append(1) or {"env": {}, "found": True,
                                                    "user_manager": False, "source": ""}, ttl_found=60)
        c.get()
        c.get()
        self.assertEqual(len(n), 1)
        c.invalidate()
        c.get()
        self.assertEqual(len(n), 2)
        c.get(force=True)
        self.assertEqual(len(n), 3)

    def test_cache_survives_a_failing_discovery(self):
        def boom():
            raise RuntimeError("x")
        v = ca.SessionCache(boom).get()
        self.assertFalse(v["found"])

    def test_ping(self):
        out = self.ok("ping")
        self.assertEqual(out["proto"], 1)
        self.assertEqual(out["agent"], "conduit-ctl-agent " + ca.VERSION)
        self.assertEqual(out["os"], "linux")
        self.assertEqual(out["user"], "tester")
        self.assertTrue(out["session"])
        self.assertEqual(out["downloads"], os.path.join(self.home, "Downloads"))
        write(os.path.join(self.home, ".config/user-dirs.dirs"),
              'XDG_DOWNLOAD_DIR="$HOME/Hent"\n')
        self.assertEqual(self.ok("ping")["downloads"], os.path.join(self.home, "Hent"))


# ------------------------------------------------------------------- apps

class AppsTest(Fixture):
    def build(self):
        data_home = os.path.join(self.home, ".local/share")
        write(os.path.join(data_home, "applications/dupe.desktop"), desktop("User Dupe"))
        write(os.path.join(self.share, "applications/dupe.desktop"), desktop("System Dupe"))
        write(os.path.join(self.share, "applications/plain.desktop"),
              desktop("Plain", "plain %U", Icon="plainicon", **{"Name[nb]": "Enkel"}))
        write(os.path.join(self.share, "applications/hidden.desktop"), desktop("Hidden", NoDisplay="true"))
        write(os.path.join(self.share, "applications/gone.desktop"), desktop("Gone", Hidden="true"))
        write(os.path.join(self.share, "applications/kdeonly.desktop"), desktop("KdeOnly", OnlyShowIn="KDE;"))
        write(os.path.join(self.share, "applications/gnomeonly.desktop"), desktop("GnomeOnly", OnlyShowIn="X-Cinnamon;GNOME;"))
        write(os.path.join(self.share, "applications/notgnome.desktop"), desktop("NotGnome", NotShowIn="GNOME;"))
        write(os.path.join(self.share, "applications/tryexec.desktop"), desktop("TryMissing", TryExec="no-such-binary-xyz"))
        write(os.path.join(self.share, "applications/tryok.desktop"), desktop("TryOk", TryExec=os.path.join(self.bin, "tool")))
        self.script("tool", "true\n")
        write(os.path.join(self.share, "applications/link.desktop"), desktop("Link", Type="Link"))
        write(os.path.join(self.share, "applications/noname.desktop"), "[Desktop Entry]\nType=Application\nExec=x\n")
        write(os.path.join(self.share, "applications/sub/nested.desktop"), desktop("Nested", Icon="/abs/icon.png"))
        write(os.path.join(self.flatpak, "applications/org.fp.App.desktop"), desktop("Flat App", "flatpak run org.fp.App"))
        write(os.path.join(self.snap, "applications/snapapp_snapapp.desktop"), desktop("Snap App"))
        write(os.path.join(self.share, "applications/notes.txt"), "x")

    def names(self, apps):
        return {a["name"]: a for a in apps}

    def test_scan(self):
        self.build()
        apps = self.ok("apps")["apps"]
        n = self.names(apps)
        self.assertIn("User Dupe", n)
        self.assertNotIn("System Dupe", n)
        for bad in ("Hidden", "Gone", "KdeOnly", "NotGnome", "TryMissing", "Link"):
            self.assertNotIn(bad, n)
        for good in ("GnomeOnly", "TryOk", "Nested", "Plain"):
            self.assertIn(good, n)
        self.assertEqual(n["Plain"]["source"], "desktop")
        self.assertEqual(n["Plain"]["icon"], "plainicon")
        self.assertEqual(n["Plain"]["target"], os.path.join(self.share, "applications/plain.desktop"))
        self.assertEqual(n["Plain"]["args"], [])
        self.assertEqual(n["Nested"]["icon"], "/abs/icon.png")
        self.assertEqual(n["Flat App"]["source"], "flatpak")
        self.assertEqual(n["Snap App"]["source"], "snap")
        self.assertEqual(n["User Dupe"]["source"], "desktop")
        self.assertEqual([a["name"].casefold() for a in apps], sorted(a["name"].casefold() for a in apps))

    def test_locale_name(self):
        self.build()
        self.agent.environ["LANG"] = "nb_NO.UTF-8"
        n = self.names(self.ok("apps")["apps"])
        self.assertIn("Enkel", n)

    def test_unknown_desktop_shows_everything(self):
        self.build()
        self.agent.environ.pop("XDG_CURRENT_DESKTOP")
        n = self.names(self.ok("apps")["apps"])
        self.assertIn("KdeOnly", n)
        self.assertIn("NotGnome", n)

    def test_kde_desktop(self):
        self.build()
        self.agent.environ["XDG_CURRENT_DESKTOP"] = "KDE"
        n = self.names(self.ok("apps")["apps"])
        self.assertIn("KdeOnly", n)
        self.assertNotIn("GnomeOnly", n)

    def steam_fixture(self):
        root = os.path.join(self.home, ".local/share/Steam")
        os.makedirs(os.path.join(root, "steamapps"))
        os.makedirs(os.path.join(self.home, ".steam"))
        os.symlink(root, os.path.join(self.home, ".steam/steam"))
        lib2 = os.path.join(self.tmp, "lib2")
        write(os.path.join(root, "steamapps/libraryfolders.vdf"),
              '"libraryfolders"\n{\n "0" { "path" "%s" "apps" { "730" "1" } }\n'
              ' "1" { "path" "%s" }\n}\n' % (root, lib2))

        def acf(d, appid, name, flags="4"):
            write(os.path.join(d, "steamapps/appmanifest_%s.acf" % appid),
                  '"AppState"\n{\n"appid" "%s"\n"name" "%s"\n"StateFlags" "%s"\n}\n' % (appid, name.replace('"', '\\"'), flags))
        acf(root, "730", "Counter-Strike 2")
        acf(root, "228980", "Steamworks Common Redistributables")
        acf(root, "1493710", "Proton Experimental")
        acf(root, "1628350", "Steam Linux Runtime 3.0 (sniper)")
        acf(lib2, "440", 'Team "Fortress" 2')
        acf(lib2, "999", "Half Downloaded", "1026")
        return root

    def test_steam_games(self):
        self.steam_fixture()
        apps = self.ok("apps")["apps"]
        steam = {a["name"]: a for a in apps if a["source"] == "steam"}
        self.assertEqual(sorted(steam), ['Counter-Strike 2', 'Team "Fortress" 2'])
        cs = steam["Counter-Strike 2"]
        self.assertEqual(cs["target"], "steam://rungameid/730")
        self.assertEqual(cs["icon"], "steam:730")

    def test_flatpak_steam(self):
        root = os.path.join(self.home, ".var/app/com.valvesoftware.Steam/.local/share/Steam")
        write(os.path.join(root, "steamapps/appmanifest_10.acf"),
              '"AppState" { "appid" "10" "name" "Flat Game" }')
        steam = [a for a in self.ok("apps")["apps"] if a["source"] == "steam"]
        self.assertEqual([a["name"] for a in steam], ["Flat Game"])


# ------------------------------------------------------------------- icons

class IconTest(Fixture):
    def theme(self, name, dirs, inherits="", extra=""):
        root = os.path.join(self.share, "icons", name)
        idx = "[Icon Theme]\nName=%s\nInherits=%s\nDirectories=%s\n" % (name, inherits, ",".join(d[0] for d in dirs))
        for d, size, typ in dirs:
            idx += "\n[%s]\nSize=%d\nType=%s\n" % (d, size, typ)
        write(os.path.join(root, "index.theme"), idx)
        return root

    def settings(self, theme):
        write(os.path.join(self.home, ".config/gtk-3.0/settings.ini"), "[Settings]\ngtk-icon-theme-name=%s\n" % theme)

    def icon(self, key):
        out = self.ok("icon", key=key)
        return out["format"], base64.b64decode(out["data"])

    def test_inherits_and_size_choice(self):
        self.settings("Mine")
        self.theme("Mine", [("32x32/apps", 32, "Fixed")], inherits="Base")
        base = self.theme("Base", [("16x16/apps", 16, "Fixed"), ("48x48/apps", 48, "Fixed"),
                                   ("128x128/apps", 128, "Fixed"), ("256x256/apps", 256, "Fixed")])
        for sz in (16, 48, 128, 256):
            write(os.path.join(base, "%dx%d/apps/foo.png" % (sz, sz)), make_png(sz, sz))
        write(os.path.join(self.share, "icons/Mine/32x32/apps/own.png"), make_png(32, 32))
        self.assertTrue(self.agent.icon_file("foo").endswith("128x128/apps/foo.png"))
        self.assertTrue(self.agent.icon_file("own").endswith("Mine/32x32/apps/own.png"))
        fmt, data = self.icon("own")
        self.assertEqual((fmt, ca.png_size(data)), ("png", (32, 32)))
        fmt, data = self.icon("foo")
        self.assertEqual(fmt, "png")  # shrunk when a tool exists, else the 128 px file
        self.assertIn(ca.png_size(data), [(128, 128), (64, 64)])

    def test_largest_smaller_when_none_big_enough(self):
        base = self.theme("hicolor", [("16x16/apps", 16, "Fixed"), ("48x48/apps", 48, "Fixed")])
        for sz in (16, 48):
            write(os.path.join(base, "%dx%d/apps/small.png" % (sz, sz)), make_png(sz, sz))
        self.assertTrue(self.agent.icon_file("small").endswith("48x48/apps/small.png"))

    def test_hicolor_and_pixmaps_fallback_and_missing(self):
        self.theme("hicolor", [("64x64/apps", 64, "Fixed")])
        write(os.path.join(self.share, "icons/hicolor/64x64/apps/hi.png"), make_png(64, 64))
        write(os.path.join(self.share, "pixmaps/pix.png"), make_png(24, 24))
        self.assertTrue(self.agent.icon_file("hi").endswith("hicolor/64x64/apps/hi.png"))
        self.assertTrue(self.agent.icon_file("pix.png").endswith("pixmaps/pix.png"))
        self.assertIn("icon not found", self.err("icon", key="nope"))

    def test_index_less_theme_layouts(self):
        write(os.path.join(self.share, "icons/Flat/apps/48/a.png"), make_png(48, 48))
        write(os.path.join(self.share, "icons/Flat2/64x64/apps/b.png"), make_png(64, 64))
        self.settings("Flat")
        self.assertTrue(self.agent.icon_file("a").endswith("Flat/apps/48/a.png"))
        self.settings("Flat2")
        self.agent.theme_cache = (0.0, None)
        self.assertTrue(self.agent.icon_file("b").endswith("Flat2/64x64/apps/b.png"))

    def test_png_preferred_over_svg(self):
        base = self.theme("hicolor", [("32x32/apps", 32, "Fixed"), ("scalable/apps", 0, "Scalable")])
        write(os.path.join(base, "32x32/apps/both.png"), make_png(32, 32))
        write(os.path.join(base, "scalable/apps/both.svg"), SVG)
        self.assertTrue(self.agent.icon_file("both").endswith("both.png"))

    def test_kde_theme_source(self):
        self.agent.environ["XDG_CURRENT_DESKTOP"] = "KDE"
        write(os.path.join(self.home, ".config/kdeglobals"), "[Icons]\nTheme=breeze\n")
        self.assertEqual(self.agent.current_icon_theme(self.agent.environ), "breeze")

    def test_gsettings_theme(self):
        self.script("gsettings", "true\n")
        self.run_results[("gsettings", "get")] = "'Papirus-Dark'\n"
        self.assertEqual(self.agent.current_icon_theme(self.agent.environ), "Papirus-Dark")

    def test_absolute_path_and_home(self):
        p = write(os.path.join(self.home, "pic.png"), make_png(10, 10))
        self.assertEqual(self.icon(p)[0], "png")
        self.assertEqual(self.icon("~/pic.png")[0], "png")
        self.assertIn("no such icon file", self.err("icon", key="/nonexistent/x.png"))

    def test_svg_raw_or_rasterized(self):
        p = write(os.path.join(self.home, "v.svg"), SVG)
        fmt, data = self.icon(p)
        self.assertIn(fmt, ("svg", "png"))
        if fmt == "svg":
            self.assertEqual(data, SVG)

    def test_svg_with_rasterizer_tool(self):
        png = write(os.path.join(self.tmp, "out.png"), make_png(64, 64))
        self.script("rsvg-convert", "cat %s\n" % png)
        p = write(os.path.join(self.home, "v.svg"), SVG)
        fmt, data = self.icon(p)
        self.assertEqual((fmt, ca.png_size(data)), ("png", (64, 64)))

    def test_big_png_with_tool_is_shrunk(self):
        png = write(os.path.join(self.tmp, "out.png"), make_png(64, 64))
        self.script("magick", "cat >/dev/null; cat %s\n" % png)
        p = write(os.path.join(self.home, "big.png"), make_png(256, 256))
        fmt, data = self.icon(p)
        self.assertEqual(ca.png_size(data), (64, 64))

    def test_jpeg_converted_with_tool_or_error(self):
        p = write(os.path.join(self.home, "j.jpg"), JPEG)
        out = self.call("icon", key=p)
        if out["ok"]:
            self.assertEqual(out["format"], "png")
        else:
            self.assertIn("no tool", out["error"])
        png = write(os.path.join(self.tmp, "out.png"), make_png(32, 32))
        self.script("magick", "cat >/dev/null; cat %s\n" % png)
        self.agent.icon_cache.clear()
        self.assertEqual(self.icon(p)[0], "png")

    def test_oversize_icon_rejected(self):
        big = make_png(60, 60) + b"\0" * ca.MAX_ICON
        p = write(os.path.join(self.home, "huge.png"), big)
        self.assertIn("too large", self.err("icon", key=p))

    def test_not_an_image(self):
        p = write(os.path.join(self.home, "x.png"), b"hello")
        self.assertIn("unknown image", self.err("icon", key=p))

    def test_steam_icons(self):
        root = os.path.join(self.home, ".steam/steam")
        write(os.path.join(root, "steamapps/x"), b"")
        write(os.path.join(root, "appcache/librarycache/730_icon.jpg"), JPEG)
        h = "a" * 40
        write(os.path.join(root, "appcache/librarycache/440/%s.jpg" % h), JPEG)
        write(os.path.join(root, "appcache/librarycache/440/header.jpg"), JPEG)
        self.assertTrue(self.agent.icon_file("steam:730").endswith("730_icon.jpg"))
        self.assertTrue(self.agent.icon_file("steam:440").endswith(h + ".jpg"))
        self.assertIn("bad steam", self.err("icon", key="steam:../x"))
        self.assertIn("icon not found", self.err("icon", key="steam:999"))

    def test_missing_key(self):
        self.assertIn("missing icon key", self.err("icon"))


# ------------------------------------------------------------------- run

class RunTest(Fixture):
    def setUp(self):
        super().setUp()
        self.addCleanup(self.kill_all)

    def kill_all(self):
        for e in list(self.agent.launches.values()):
            try:
                os.killpg(e.pid, 9)
            except OSError:
                pass

    def test_executable_with_args_cwd_env(self):
        out = os.path.join(self.tmp, "out.txt")
        self.script("app", 'echo "$PWD|$FOO|$1|$2" > %s\n' % out)
        wd = os.path.join(self.tmp, "wd dir")
        os.makedirs(wd)
        r = self.ok("run", cmd="app", args=["a b", "c"], cwd=wd, env={"FOO": "bar"})
        self.assertGreater(r["pid"], 0)
        self.assertEqual(self.wait_for(out), "%s|bar|a b|c" % wd)

    def test_absolute_path_and_session_env(self):
        out = os.path.join(self.tmp, "out.txt")
        p = self.script("showenv", 'echo "$WAYLAND_DISPLAY|$XDG_RUNTIME_DIR" > %s\n' % out)
        self.ok("run", cmd=p)
        self.assertEqual(self.wait_for(out), "wayland-0|" + self.rt)

    def test_missing_executable(self):
        self.assertEqual(self.err("run", cmd="definitely-not-here"), "not found: definitely-not-here")
        self.assertEqual(self.err("run", cmd="/no/such/bin"), "not found: /no/such/bin")
        self.assertIn("empty", self.err("run", cmd=""))
        self.assertIn("no such directory", self.err("run", cmd="true", cwd="/no/such/dir"))

    def test_immediate_failure_is_reported(self):
        self.script("bad", "exit 3\n")
        self.assertIn("exited with status 3", self.err("run", cmd="bad"))

    def test_document_goes_to_opener(self):
        doc = write(os.path.join(self.home, "doc with space.pdf"), b"%PDF")
        self.assertEqual(self.err("run", cmd=doc), "not found: xdg-open")
        out = os.path.join(self.tmp, "open.txt")
        self.script("xdg-open", 'echo "$1" > %s\n' % out)
        self.ok("run", cmd=doc)
        self.assertEqual(self.wait_for(out), doc)

    def test_steam_url(self):
        out = os.path.join(self.tmp, "steam.txt")
        self.script("steam", 'echo "$1" > %s\n' % out)
        self.ok("run", cmd="steam://rungameid/730")
        self.assertEqual(self.wait_for(out), "steam://rungameid/730")

    def test_other_url_uses_xdg_open(self):
        out = os.path.join(self.tmp, "o.txt")
        self.script("xdg-open", 'echo "$1" > %s\n' % out)
        self.ok("run", cmd="https://example.org/x")
        self.assertEqual(self.wait_for(out), "https://example.org/x")
        os.unlink(os.path.join(self.bin, "xdg-open"))
        self.assertIn("not found", self.err("run", cmd="https://example.org/x"))

    def test_desktop_via_gio(self):
        out = os.path.join(self.tmp, "gio.txt")
        self.script("gio", 'echo "$@" > %s\n' % out)
        d = write(os.path.join(self.share, "applications/x.desktop"), desktop("X", "x"))
        self.ok("run", cmd=d)
        self.assertEqual(self.wait_for(out), "launch " + d)

    def test_desktop_falls_back_to_exec_when_gio_fails(self):
        out = os.path.join(self.tmp, "exec.txt")
        self.script("gio", "exit 1\n")
        self.script("realapp", 'echo "$1|$2|$PWD" > %s\n' % out)
        wd = os.path.join(self.tmp, "appdir")
        os.makedirs(wd)
        d = write(os.path.join(self.share, "applications/y.desktop"),
                  desktop("Y", "realapp --opt=%c %f", Path=wd))
        self.ok("run", cmd=d, args=["file one"])
        self.assertEqual(self.wait_for(out), "--opt=Y|file one|" + wd)

    def test_desktop_without_launchers_parses_exec(self):
        out = os.path.join(self.tmp, "p.txt")
        self.script("tool", 'echo "$1|$2|$3" > %s\n' % out)
        d = write(os.path.join(self.share, "applications/z.desktop"),
                  desktop("Z", 'tool "two words" %% %U'))
        self.ok("run", cmd=d, args=["x"])
        self.assertEqual(self.wait_for(out), "two words|%|x")

    def test_desktop_args_appended_without_field_code(self):
        out = os.path.join(self.tmp, "q.txt")
        self.script("tool2", 'echo "$1|$2" > %s\n' % out)
        d = write(os.path.join(self.share, "applications/q.desktop"), desktop("Q", "tool2 first"))
        self.ok("run", cmd=d, args=["second"])
        self.assertEqual(self.wait_for(out), "first|second")

    def test_desktop_terminal(self):
        out = os.path.join(self.tmp, "t.txt")
        self.script("xterm", 'echo "$@" > %s\n' % out)
        self.script("cli", "true\n")
        d = write(os.path.join(self.share, "applications/t.desktop"), desktop("T", "cli --x", Terminal="true"))
        self.ok("run", cmd=d)
        self.assertEqual(self.wait_for(out), "-e %s --x" % os.path.join(self.bin, "cli"))
        os.unlink(os.path.join(self.bin, "xterm"))
        self.assertIn("terminal emulator", self.err("run", cmd=d))

    def test_desktop_errors(self):
        self.assertEqual(self.err("run", cmd="/no/where.desktop"), "not found: /no/where.desktop")
        d = write(os.path.join(self.share, "applications/m.desktop"), desktop("M", "no-such-program-qq"))
        self.assertIn("not found: no-such-program-qq", self.err("run", cmd=d))
        d2 = write(os.path.join(self.share, "applications/n.desktop"), "[Desktop Entry]\nType=Application\nName=N\n")
        self.assertIn("no Exec", self.err("run", cmd=d2))

    def test_desktop_id_lookup(self):
        out = os.path.join(self.tmp, "id.txt")
        self.script("idapp", 'echo ok > %s\n' % out)
        write(os.path.join(self.share, "applications/idapp.desktop"), desktop("Id", "idapp"))
        self.ok("run", cmd="idapp.desktop")
        self.assertEqual(self.wait_for(out), "ok")

    def test_children_are_reaped(self):
        self.script("quick", "%s 0.1\n" % shutil.which("sleep"))
        pid = self.ok("run", cmd="quick")["pid"]
        e = self.agent.launches[pid]
        end = time.time() + 5
        while not e.exited and time.time() < end:
            time.sleep(0.02)
        self.assertTrue(e.exited)
        with self.assertRaises(ChildProcessError):
            os.waitpid(pid, os.WNOHANG)

    def test_stop_terminates_and_refuses_foreign(self):
        sleep = shutil.which("sleep")
        pid = self.ok("run", cmd=sleep, args=["60"])["pid"]
        e = self.agent.launches[pid]
        self.assertFalse(e.exited)
        self.ok("stop", pid=pid)
        end = time.time() + 5
        while not e.exited and time.time() < end:
            time.sleep(0.02)
        self.assertTrue(e.exited)
        self.ok("stop", pid=pid)  # already gone: fine
        self.assertIn("not started by this agent", self.err("stop", pid=os.getpid()))
        self.assertIn("not started by this agent", self.err("stop", pid=1))
        self.assertIn("bad pid", self.err("stop", pid=-3))
        self.assertIn("bad pid", self.err("stop"))

    def test_stop_kills_after_grace(self):
        ca.STOP_GRACE, old = 0.3, ca.STOP_GRACE
        self.addCleanup(setattr, ca, "STOP_GRACE", old)
        self.script("stubborn", "trap '' TERM\nwhile :; do sleep 0.1; done\n")
        pid = self.ok("run", cmd="stubborn")["pid"]
        e = self.agent.launches[pid]
        self.ok("stop", pid=pid)
        end = time.time() + 5
        while not e.exited and time.time() < end:
            time.sleep(0.02)
        self.assertTrue(e.exited)
        self.assertFalse(self.agent._group_alive(pid))


# ------------------------------------------------------------------ files

class FilesTest(Fixture):
    def put(self, path, pieces, size=None, sha=None, force=False, agent=None):
        """pieces: list of bytes; the last carries done."""
        off, last = 0, None
        for i, piece in enumerate(pieces):
            done = i == len(pieces) - 1
            fields = dict(path=path, offset=off, data=b64(piece), done=done, force=force)
            if size is not None and i == 0:
                fields["size"] = size
            if done:
                if size is not None:
                    fields["size"] = size
                if sha is not None:
                    fields["sha256"] = sha
            last = self.call("put", agent=agent, **fields)
            if not last["ok"]:
                return last
            off += len(piece)
        return last

    def test_put_roundtrip_unicode_spaces(self):
        d = os.path.join(self.home, "Mine Filer")
        os.makedirs(d)
        target = os.path.join(d, "Ære Ø 日本語.bin")
        data = os.urandom(300000)
        out = self.put(target, [data[:100000], data[100000:200000], data[200000:]],
                       size=len(data), sha=hashlib.sha256(data).hexdigest())
        self.assertTrue(out["ok"], out)
        self.assertEqual(out["path"], target)
        self.assertEqual(out["size"], len(data))
        self.assertEqual(out["sha256"], hashlib.sha256(data).hexdigest())
        with open(target, "rb") as f:
            self.assertEqual(f.read(), data)
        self.assertEqual(os.listdir(d), [os.path.basename(target)])
        self.assertEqual(os.stat(target).st_uid, os.getuid())

    def test_put_invalid_utf8_name(self):
        raw = os.path.join(os.fsencode(self.home), b"bad\xff\xfename")
        target = os.fsdecode(raw)
        out = self.put(target, [b"hi"])
        self.assertTrue(out["ok"], out)
        with open(raw, "rb") as f:
            self.assertEqual(f.read(), b"hi")
        listing = self.ok("ls", path=self.home)["entries"]
        self.assertTrue(any(e["name"].startswith("bad") for e in listing))
        # and the reply is valid JSON text for the host
        self.assertIsInstance(json.dumps(listing), str)

    def test_bare_name_goes_to_downloads(self):
        out = self.put("pic.png", [b"x"])
        self.assertTrue(out["ok"], out)
        self.assertEqual(out["path"], os.path.join(self.home, "Downloads", "pic.png"))
        write(os.path.join(self.home, ".config/user-dirs.dirs"), 'XDG_DOWNLOAD_DIR="$HOME/Hent"\n')
        out = self.put("b.txt", [b"y"])
        self.assertEqual(out["path"], os.path.join(self.home, "Hent", "b.txt"))

    def test_tilde_and_relative(self):
        out = self.put("~/t.txt", [b"x"])
        self.assertEqual(out["path"], os.path.join(self.home, "t.txt"))
        os.makedirs(os.path.join(self.home, "sub"))
        out = self.put("sub/r.txt", [b"x"])
        self.assertEqual(out["path"], os.path.join(self.home, "sub", "r.txt"))

    def test_empty_file(self):
        out = self.put(os.path.join(self.home, "empty"), [b""], size=0)
        self.assertTrue(out["ok"], out)
        self.assertEqual(os.path.getsize(os.path.join(self.home, "empty")), 0)

    def test_missing_parent(self):
        e = self.err("put", path="/no/such/dir/f", offset=0, data="", done=True)
        self.assertIn("no such directory", e)

    def test_existing_needs_force(self):
        t = write(os.path.join(self.home, "have"), b"old")
        out = self.put(t, [b"new"])
        self.assertFalse(out["ok"])
        self.assertIn("already exists", out["error"])
        self.assertEqual(rd(t), b"old")
        self.assertFalse(os.path.exists(t + ".conduit-part"))
        out = self.put(t, [b"new"], force=True)
        self.assertTrue(out["ok"], out)
        self.assertEqual(rd(t), b"new")

    def test_directory_target(self):
        d = os.path.join(self.home, "adir")
        os.makedirs(d)
        out = self.put(d, [b"x"], force=True)
        self.assertIn("is a directory", out["error"])

    def test_wrong_offset_cleans_up(self):
        t = os.path.join(self.home, "w")
        self.assertTrue(self.call("put", path=t, offset=0, data=b64(b"abcd"))["ok"])
        e = self.err("put", path=t, offset=9, data=b64(b"x"))
        self.assertEqual(e, "unexpected offset 9, expected 4")
        self.assertFalse(os.path.exists(t + ".conduit-part"))
        self.assertFalse(os.path.exists(t))
        # a continuation without a start is refused too
        e = self.err("put", path=t, offset=4, data=b64(b"x"))
        self.assertIn("unexpected offset 4, expected 0", e)

    def test_bad_sha_and_size_mismatch(self):
        t = os.path.join(self.home, "s")
        out = self.put(t, [b"abc"], sha="0" * 64)
        self.assertIn("sha256 mismatch", out["error"])
        self.assertFalse(os.path.exists(t))
        self.assertFalse(os.path.exists(t + ".conduit-part"))
        out = self.put(t, [b"abc"], size=5)
        self.assertIn("size mismatch", out["error"])
        self.assertFalse(os.path.exists(t))
        self.assertFalse(os.path.exists(t + ".conduit-part"))

    def test_bad_input(self):
        t = os.path.join(self.home, "b")
        self.assertIn("base64", self.err("put", path=t, offset=0, data="!!!"))
        self.assertIn("bad offset", self.err("put", path=t, offset=-1, data=""))
        self.assertIn("bad offset", self.err("put", path=t, offset="x", data=""))
        self.assertIn("missing path", self.err("put", offset=0, data=""))
        self.assertIn("larger", self.err("put", path=t, offset=0, data=b64(b"x" * (ca.CHUNK + 1))))
        self.assertIn("larger", self.err("put", path=t, offset=0, data="", size=ca.MAX_FILE + 1))

    def test_abandoned_partial_then_restart(self):
        t = os.path.join(self.home, "ab")
        self.assertTrue(self.call("put", path=t, offset=0, data=b64(b"stale stale stale"))["ok"])
        # the agent restarts; the host starts the file over
        fresh = self.make_agent()
        out = self.put(t, [b"fresh"], agent=fresh)
        self.assertTrue(out["ok"], out)
        self.assertEqual(rd(t), b"fresh")

    def test_foreign_part_file_is_never_deleted_or_clobbered(self):
        t = os.path.join(self.home, "f")
        part = write(t + ".conduit-part", b"user data")
        # an error before anything was written leaves it alone
        e = self.err("put", path=t, offset=4, data=b64(b"x"))
        self.assertIn("unexpected offset", e)
        self.assertEqual(rd(part), b"user data")
        # a fresh transfer refuses to replace a part file it did not make
        out = self.put(t, [b"new"])
        self.assertFalse(out["ok"])
        self.assertIn("not made by Conduit", out["error"])
        self.assertEqual(rd(part), b"user data")
        self.assertFalse(os.path.exists(t))

    def test_part_symlink_is_not_followed(self):
        t = os.path.join(self.home, "l")
        victim = write(os.path.join(self.home, "victim"), b"keep")
        os.symlink(victim, t + ".conduit-part")
        out = self.put(t, [b"evil"])
        self.assertFalse(out["ok"])
        self.assertEqual(rd(victim), b"keep")
        self.assertTrue(os.path.islink(t + ".conduit-part"))

    def test_own_part_is_deleted_on_error(self):
        t = os.path.join(self.home, "o")
        self.assertTrue(self.call("put", path=t, offset=0, data=b64(b"abc"))["ok"])
        self.assertTrue(os.path.exists(t + ".conduit-part"))
        self.err("put", path=t, offset=1, data=b64(b"x"))
        self.assertFalse(os.path.exists(t + ".conduit-part"))

    def test_put_does_not_replace_a_file_that_appeared_meanwhile(self):
        t = os.path.join(self.home, "race")
        self.assertTrue(self.call("put", path=t, offset=0, data=b64(b"abc"))["ok"])
        write(t, b"theirs")
        out = self.call("put", path=t, offset=3, data=b64(b"def"), done=True)
        self.assertFalse(out["ok"])
        self.assertIn("already exists", out["error"])
        self.assertEqual(rd(t), b"theirs")
        self.assertFalse(os.path.exists(t + ".conduit-part"))

    def test_continue_after_agent_restart(self):
        t = os.path.join(self.home, "cont")
        self.assertTrue(self.call("put", path=t, offset=0, data=b64(b"abc"))["ok"])
        fresh = self.make_agent()
        out = self.call("put", agent=fresh, path=t, offset=3, data=b64(b"def"), done=True,
                        sha256=hashlib.sha256(b"abcdef").hexdigest(), size=6)
        self.assertTrue(out["ok"], out)
        self.assertEqual(rd(t), b"abcdef")

    def test_get_pieces_eof_sha(self):
        data = os.urandom(ca.CHUNK + 1000)
        t = write(os.path.join(self.home, "g bin"), data)
        a = self.ok("get", path=t, offset=0, len=ca.CHUNK)
        self.assertEqual((a["size"], a["eof"], a["sha256"]), (len(data), False, None))
        b = self.ok("get", path=t, offset=ca.CHUNK, len=ca.CHUNK)
        self.assertTrue(b["eof"])
        self.assertEqual(b["sha256"], hashlib.sha256(data).hexdigest())
        got = base64.b64decode(a["data"]) + base64.b64decode(b["data"])
        self.assertEqual(got, data)
        # random access last piece (no sequential hash) still gets the hash
        c = self.ok("get", path=t, offset=len(data) - 10, len=ca.CHUNK)
        self.assertEqual((c["eof"], c["sha256"]), (True, hashlib.sha256(data).hexdigest()))

    def test_get_edge_cases(self):
        t = write(os.path.join(self.home, "small"), b"hello")
        self.assertEqual(self.ok("get", path=t, offset=0, len=0)["data"], b64(b"hello"))
        past = self.ok("get", path=t, offset=99, len=10)
        self.assertEqual((past["data"], past["eof"]), ("", True))
        e = write(os.path.join(self.home, "empty"), b"")
        r = self.ok("get", path=e, offset=0, len=10)
        self.assertEqual((r["eof"], r["size"], r["sha256"]),
                         (True, 0, hashlib.sha256(b"").hexdigest()))
        self.assertTrue(self.ok("get", path="~/small", len=2)["data"] == b64(b"he"))

    def test_get_errors(self):
        self.assertIn("cannot open", self.err("get", path=os.path.join(self.home, "missing")))
        self.assertIn("not a regular file", self.err("get", path=self.home))
        self.assertIn("missing path", self.err("get"))
        self.assertIn("bad offset", self.err("get", path="x", offset=-1))

    def test_ls(self):
        write(os.path.join(self.home, "b.txt"), b"12345")
        write(os.path.join(self.home, "A dir/inner"), b"")
        os.symlink(os.path.join(self.home, "A dir"), os.path.join(self.home, "linkdir"))
        os.symlink("/nonexistent", os.path.join(self.home, "dangling"))
        out = self.ok("ls", path="")
        self.assertEqual(out["path"], self.home)
        by = {e["name"]: e for e in out["entries"]}
        self.assertTrue(by["A dir"]["dir"])
        self.assertTrue(by["linkdir"]["dir"])
        self.assertFalse(by["b.txt"]["dir"])
        self.assertEqual(by["b.txt"]["size"], 5)
        self.assertGreater(by["b.txt"]["mtime"], 0)
        self.assertIn("dangling", by)
        self.assertEqual(self.ok("ls", path="~")["path"], self.home)
        self.assertIn("cannot list", self.err("ls", path="/no/such/dir"))
        self.assertIn("cannot list", self.err("ls", path=os.path.join(self.home, "b.txt")))

    def test_ls_cap(self):
        for i in range(10):
            write(os.path.join(self.home, "f%02d" % i), b"")
        ca.LS_CAP, old = 4, ca.LS_CAP
        self.addCleanup(setattr, ca, "LS_CAP", old)
        out = self.ok("ls", path=self.home)
        self.assertEqual([e["name"] for e in out["entries"]], ["f00", "f01", "f02", "f03"])


# ----------------------------------------------------- requests and channel

class HandlerTest(Fixture):
    def line(self, raw):
        return json.loads(self.agent.handle_line(raw))

    def test_malformed(self):
        for raw in (b"not json", b"[1,2]", b'"x"', b"\xff\xfe", b"{"):
            out = self.line(raw)
            self.assertFalse(out["ok"], raw)
            self.assertEqual(out["id"], 0)

    def test_missing_fields(self):
        self.assertEqual(self.line(b'{"v":1,"op":"ping"}')["error"], "missing id")
        out = self.line(b'{"id":3,"op":"ping"}')
        self.assertEqual((out["id"], out["error"]), (3, "missing protocol version"))
        out = self.line(b'{"v":1,"id":3}')
        self.assertEqual(out["id"], 3)
        self.assertIn("unknown op", out["error"])

    def test_unknown_op_and_version(self):
        out = self.line(b'{"v":1,"id":4,"op":"frobnicate"}')
        self.assertEqual((out["id"], out["ok"], out["error"]), (4, False, "unknown op: frobnicate"))
        out = self.line(b'{"v":1,"id":4,"op":"__class__"}')
        self.assertFalse(out["ok"])
        out = self.line(b'{"v":99,"id":5,"op":"ping"}')
        self.assertEqual((out["id"], out["ok"]), (5, False))
        self.assertIn("unsupported protocol version 99", out["error"])

    def test_extra_fields_ignored_and_ok_shape(self):
        out = self.line(b'{"v":1,"id":6,"op":"ping","future":[1]}')
        self.assertTrue(out["ok"])
        self.assertNotIn("error", out)

    def test_op_exception_becomes_error(self):
        def boom(req):
            raise RuntimeError("kaput")
        self.agent.op_ping = boom
        out = self.line(b'{"v":1,"id":8,"op":"ping"}')
        self.assertEqual(out["id"], 8)
        self.assertFalse(out["ok"])
        self.assertIn("kaput", out["error"])

    def test_reply_too_large(self):
        line = ca.Agent.reply(9, {"blob": "a" * (ca.MAX_LINE + 10)})
        out = json.loads(line)
        self.assertEqual((out["id"], out["ok"], out["error"]), (9, False, "reply too large"))
        self.assertLess(len(line), 200)

    def test_bad_types(self):
        self.assertIn("args", self.err("run", cmd="x", args="a"))
        self.assertIn("env", self.err("run", cmd="x", env=[1]))
        self.assertIn("cwd", self.err("run", cmd="x", cwd=5))


class Channel:
    """The host's end of a socketpair, speaking the line protocol."""

    def __init__(self, sock):
        self.sock = sock
        self.sock.settimeout(10)
        self.buf = b""

    def send_raw(self, data):
        self.sock.sendall(data)

    def recv(self):
        while b"\n" not in self.buf:
            d = self.sock.recv(1 << 16)
            if not d:
                raise EOFError
            self.buf += d
        line, _, self.buf = self.buf.partition(b"\n")
        return json.loads(line)

    def call(self, op, rid=1, **fields):
        req = {"v": 1, "id": rid, "op": op}
        req.update(fields)
        self.send_raw(json.dumps(req).encode() + b"\n")
        out = self.recv()
        assert out["id"] == rid, out
        return out


class ServerTest(Fixture):
    def start(self, fds=None, **kw):
        """Run a Server; `opener` hands out successive socketpair ends."""
        self.hosts = []
        self.queue = []
        self.opened = threading.Semaphore(0)

        def new_pair():
            a, b = socket.socketpair()
            self.hosts.append(b)
            return a.detach()

        def opener():
            if not self.queue:
                raise OSError(2, "no port")
            return self.queue.pop(0)()

        self.queue.append(new_pair)
        self.server = ca.Server(self.agent, opener, backoff=(0.01, 0.05), **kw)
        self.thread = threading.Thread(target=self.server.run, daemon=True)
        self.thread.start()
        self.addCleanup(self.shutdown)
        end = time.time() + 5
        while not self.hosts and time.time() < end:
            time.sleep(0.01)
        return Channel(self.hosts[0])

    def shutdown(self):
        self.server.stop.set()
        self.thread.join(5)
        for h in self.hosts:
            h.close()

    def test_every_op_over_the_channel(self):
        ch = self.start()
        self.assertTrue(ch.call("ping")["ok"])
        write(os.path.join(self.share, "applications/a.desktop"), desktop("Over Channel"))
        self.assertIn("Over Channel", [a["name"] for a in ch.call("apps")["apps"]])
        write(os.path.join(self.share, "icons/hicolor/32x32/apps/ic.png"), make_png(32, 32))
        out = ch.call("icon", key="ic")
        self.assertEqual(out["format"], "png")
        t = os.path.join(self.home, "up.txt")
        self.assertTrue(ch.call("put", path=t, offset=0, data=b64(b"abc"), done=True,
                                size=3, sha256=hashlib.sha256(b"abc").hexdigest())["ok"])
        got = ch.call("get", path=t, offset=0, len=10)
        self.assertEqual((base64.b64decode(got["data"]), got["eof"]), (b"abc", True))
        self.assertIn("up.txt", [e["name"] for e in ch.call("ls", path=self.home)["entries"]])
        pid = ch.call("run", cmd=shutil.which("sleep"), args=["60"])["pid"]
        self.assertTrue(ch.call("stop", pid=pid)["ok"])
        self.assertFalse(ch.call("stop", pid=os.getpid())["ok"])

    def test_bad_lines_then_good(self):
        ch = self.start()
        ch.send_raw(b"garbage\n")
        out = ch.recv()
        self.assertFalse(out["ok"])
        ch.send_raw(b'{"v":1,"id":2,"op":"nope"}\n')
        self.assertEqual(ch.recv()["error"], "unknown op: nope")
        self.assertTrue(ch.call("ping", rid=3)["ok"])

    def test_oversized_line_then_good(self):
        ch = self.start()
        big = b'{"v":1,"id":41,"op":"put","data":"' + b"A" * (ca.MAX_LINE + 1000) + b'"}\n'
        t = threading.Thread(target=ch.send_raw, args=(big,))
        t.start()
        out = ch.recv()
        t.join()
        self.assertEqual((out["id"], out["ok"]), (41, False))
        self.assertIn("too long", out["error"])
        self.assertTrue(ch.call("ping", rid=42)["ok"])

    def test_slow_op_does_not_block_ping(self):
        gate = threading.Event()
        orig = self.agent.op_apps
        self.agent.op_apps = lambda req: (gate.wait(5), orig(req))[1]
        ch = self.start()
        ch.send_raw(json.dumps({"v": 1, "id": 1, "op": "apps"}).encode() + b"\n")
        pong = ch.call("ping", rid=2)
        self.assertTrue(pong["ok"])
        gate.set()
        self.assertEqual(ch.recv()["id"], 1)

    def test_eof_then_reconnect(self):
        ch = self.start()
        self.assertTrue(ch.call("ping")["ok"])
        self.queue.append(lambda: (lambda p: (self.hosts.append(p[1]), p[0].detach())[1])(socket.socketpair()))
        ch.sock.close()  # the host went away (VM resumed, QEMU restarted)
        end = time.time() + 5
        while len(self.hosts) < 2 and time.time() < end:
            time.sleep(0.01)
        self.assertEqual(len(self.hosts), 2)
        ch2 = Channel(self.hosts[1])
        self.assertTrue(ch2.call("ping", rid=9)["ok"])
        self.assertGreaterEqual(self.server.connections, 2)

    def test_open_errors_are_retried(self):
        self.start()
        n = []

        def failing():
            n.append(1)
            raise OSError(16, "busy")
        self.queue.extend([failing, failing])
        self.hosts[0].close()
        end = time.time() + 5
        while len(n) < 2 and time.time() < end:
            time.sleep(0.01)
        self.assertGreaterEqual(len(n), 2)

    def test_reply_dropped_when_host_is_gone(self):
        gate = threading.Event()
        self.agent.op_ping = lambda req: (gate.wait(5), {"proto": 1})[1]
        ch = self.start()
        ch.send_raw(json.dumps({"v": 1, "id": 1, "op": "ping"}).encode() + b"\n")
        ch.sock.close()
        time.sleep(0.2)
        gate.set()
        time.sleep(0.2)  # the worker must survive writing to a dead channel
        self.assertTrue(self.thread.is_alive())

    def test_stalled_writer_gives_up(self):
        a, b = socket.socketpair()
        a.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
        os.set_blocking(a.fileno(), False)
        conn = ca.Conn(a.detach(), write_timeout=0.3)
        t0 = time.time()
        self.assertFalse(conn.send(b"x" * (8 << 20)))
        self.assertLess(time.time() - t0, 5)
        self.assertTrue(conn.dead)
        self.assertFalse(conn.send(b"y\n"))
        conn.close()
        b.close()

    def test_partial_writes_complete(self):
        a, b = socket.socketpair()
        a.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
        os.set_blocking(a.fileno(), False)
        conn = ca.Conn(a.detach(), write_timeout=10)
        payload = b"z" * (2 << 20)
        got = bytearray()

        def reader():
            while len(got) < len(payload):
                got.extend(b.recv(1 << 16))
        t = threading.Thread(target=reader)
        t.start()
        self.assertTrue(conn.send(payload))
        t.join(10)
        self.assertEqual(bytes(got), payload)
        conn.close()
        b.close()


class ModuleTest(unittest.TestCase):
    def test_compiles_and_has_main(self):
        subprocess.run(["python3", "-m", "py_compile", os.path.join(HERE, "conduit-ctl-agent")], check=True)
        r = subprocess.run(["python3", os.path.join(HERE, "conduit-ctl-agent"), "--version"],
                           capture_output=True, text=True, check=True)
        self.assertEqual(r.stdout.strip(), "conduit-ctl-agent " + ca.VERSION)

    def test_find_port_default(self):
        self.assertTrue(ca.find_port().startswith("/dev/"))


if __name__ == "__main__":
    unittest.main()
