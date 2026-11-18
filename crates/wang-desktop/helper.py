"""wang desktop helper: drives GNOME apps for wang.

Speaks JSON lines on stdin/stdout: {"id", "op", ...} -> {"id", "ok", "result"|"error"}.

- Reading and acting on windows uses the accessibility tree (AT-SPI): targeted at one element,
  never "whatever has focus".
- Pixels (screenshot, click at x,y, key presses) use the xdg-desktop-portal RemoteDesktop +
  ScreenCast session. GNOME asks the user once ("Allow remote interaction"); the restore token
  is saved so later sessions start without asking.
- Key presses go to the focused window, so they're refused unless the window wang means to
  type into is the active one.
"""

import json
import os
import subprocess
import sys
import tempfile
import time

import gi

gi.require_version("Atspi", "2.0")
gi.require_version("Gio", "2.0")
gi.require_version("GLib", "2.0")
from gi.repository import Atspi, Gio, GLib  # noqa: E402

MAX_REFS = 150
MAX_LINES = 260
INTERACTIVE = {
    "push button", "toggle button", "check box", "radio button", "menu item", "check menu item",
    "radio menu item", "link", "entry", "password text", "text", "combo box", "list item", "page tab",
    "slider", "spin button", "tree item", "table cell", "menu", "icon", "button", "switch", "editbar",
}
CONTEXT = {"label", "heading", "static", "paragraph", "section", "status bar", "notification", "alert"}
SKIP = {"scroll bar", "separator", "filler", "redundant object", "unknown", "image"}
TERMINALS = ("ptyxis", "gnome-terminal", "kgx", "console", "konsole", "xterm", "alacritty", "kitty", "tilix", "terminator", "wezterm")

refs = []       # index n-1 -> Atspi.Accessible from the last view
windows = []    # index n-1 -> (app, window) from the last windows/view
state_dir = os.environ.get("WANG_DESKTOP_STATE", os.path.expanduser("~/.local/share/wang/desktop"))


def role(a):
    try:
        return a.get_role_name() or ""
    except Exception:
        return ""


def name_of(a):
    try:
        n = a.get_name() or ""
        if not n:
            n = a.get_description() or ""
        return " ".join(n.split())[:120]
    except Exception:
        return ""


def states(a):
    try:
        s = a.get_state_set()
    except Exception:
        return set()
    out = set()
    for st, label in [
        (Atspi.StateType.ACTIVE, "active"), (Atspi.StateType.FOCUSED, "focused"), (Atspi.StateType.CHECKED, "checked"),
        (Atspi.StateType.PRESSED, "pressed"), (Atspi.StateType.SELECTED, "selected"), (Atspi.StateType.EXPANDED, "expanded"),
        (Atspi.StateType.EDITABLE, "editable"), (Atspi.StateType.SHOWING, "showing"), (Atspi.StateType.VISIBLE, "visible"),
        (Atspi.StateType.ENABLED, "enabled"), (Atspi.StateType.SENSITIVE, "sensitive"),
    ]:
        if s.contains(st):
            out.add(label)
    return out


def text_of(a):
    try:
        n = Atspi.Text.get_character_count(a)
        return " ".join((Atspi.Text.get_text(a, 0, min(n, 400)) or "").split())
    except Exception:
        return ""


def action_names(a):
    try:
        return [Atspi.Action.get_action_name(a, i) for i in range(Atspi.Action.get_n_actions(a))]
    except Exception:
        return []


def do_action(a, i):
    Atspi.Action.do_action(a, i)
    # Apps handle activations on their main loop; back-to-back ones can be dropped.
    time.sleep(0.15)


def apps():
    d = Atspi.get_desktop(0)
    for i in range(d.get_child_count()):
        a = d.get_child_at_index(i)
        if a is not None:
            yield a


def list_windows():
    global windows
    windows = []
    for app in apps():
        an = name_of(app)
        if an in ("gnome-shell", "gjs", "xdg-desktop-portal-gtk", "python3", "ibus-extension-gtk3"):
            continue
        try:
            n = app.get_child_count()
        except Exception:
            continue
        for j in range(n):
            w = app.get_child_at_index(j)
            if w is None:
                continue
            st = states(w)
            if "showing" not in st and "visible" not in st and "active" not in st:
                continue
            windows.append((app, w))
    out = []
    for i, (app, w) in enumerate(windows):
        out.append({"n": i + 1, "app": name_of(app), "title": name_of(w), "active": "active" in states(w)})
    return out


def active_window():
    list_windows()
    for app, w in windows:
        if "active" in states(w):
            return app, w
    return None


def pick_window(n):
    if n:
        if not windows:
            list_windows()
        if not 1 <= n <= len(windows):
            raise ValueError(f"no window {n}: run windows first")
        return windows[n - 1]
    aw = active_window()
    if not aw:
        raise ValueError("no active window (is the screen locked?)")
    return aw


def is_terminal(app):
    n = name_of(app).lower()
    return any(t in n for t in TERMINALS)


def view(n=None):
    """Compact view of one window: numbered interactive elements plus nearby text."""
    global refs
    app, w = pick_window(n)
    refs = []
    lines = []
    seen_text = set()

    def walk(a, depth):
        if len(lines) >= MAX_LINES or depth > 40:
            return
        r = role(a)
        st = states(a)
        if r in SKIP:
            return
        if "showing" not in st and r not in ("frame", "window", "dialog") and depth > 0:
            return
        nm = name_of(a)
        if r in INTERACTIVE and len(refs) < MAX_REFS and ("enabled" in st or "sensitive" in st or r in ("text", "entry")):
            refs.append(a)
            extra = []
            if r in ("text", "entry", "password text", "spin button"):
                val = "" if r == "password text" else text_of(a)
                if val and val != nm:
                    extra.append(f'value="{val[:80]}"')
            for s in ("checked", "selected", "expanded", "focused"):
                if s in st:
                    extra.append(s)
            if "editable" in st and r not in ("text", "entry", "password text"):
                extra.append("editable")
            lines.append(f'{"  " * min(depth, 6)}[{len(refs)}] {r} "{nm}"' + (f" ({', '.join(extra)})" if extra else ""))
        elif r in CONTEXT or (r not in INTERACTIVE and nm and a.get_child_count() == 0):
            t = nm or text_of(a)
            if t and t not in seen_text:
                seen_text.add(t)
                lines.append(f'{"  " * min(depth, 6)}{t[:160]}')
        try:
            k = a.get_child_count()
        except Exception:
            return
        for i in range(min(k, 400)):
            c = a.get_child_at_index(i)
            if c is not None:
                walk(c, depth + 1)

    walk(w, 0)
    head = f'window: {name_of(w)} — {name_of(app)}' + ("" if "active" in states(w) else " (not the active window)")
    more = f"\n(showing the first {MAX_REFS} controls)" if len(refs) >= MAX_REFS else ""
    return head + "\n" + "\n".join(lines) + more


def ref(n):
    if not refs:
        raise ValueError("no view yet: call view first")
    if not 1 <= n <= len(refs):
        raise ValueError(f"no element [{n}] in the last view")
    a = refs[n - 1]
    try:
        a.get_role_name()
    except Exception:
        raise ValueError(f"element [{n}] is gone (the window changed): view again")
    return a


def info(n):
    """What the guard needs to classify an action on [n]."""
    a = ref(n)
    app = a.get_application()
    hint = " ".join([name_of(a), a.get_description() or ""]).lower()
    r = role(a)
    return {
        "role": r,
        "text": name_of(a),
        "app": name_of(app) if app else "",
        "terminal": bool(app and is_terminal(app)),
        "password": r == "password text",
        "search": "search" in hint or r == "search" or "find" in hint.split(),
    }


def click(n):
    a = ref(n)
    names = action_names(a)
    if names:
        for pref in ("click", "press", "activate", "jump", "toggle", "select", "open"):
            if pref in names:
                do_action(a, names.index(pref))
                return f'{pref} on "{name_of(a)}"'
        do_action(a, 0)
        return f'{names[0]} on "{name_of(a)}"'
    try:
        if Atspi.Component.grab_focus(a):
            return f'focused "{name_of(a)}" (it has no click action)'
    except Exception:
        pass
    raise ValueError(f'[{n}] "{name_of(a)}" can\'t be clicked through accessibility; try screenshot + click_at')


def type_text(n, text, submit, replace):
    a = ref(n)
    if role(a) == "password text":
        raise ValueError("wang never types into password fields")
    try:
        if replace:
            Atspi.EditableText.set_text_contents(a, text)
        else:
            count = Atspi.Text.get_character_count(a)
            pos = Atspi.Text.get_caret_offset(a)
            Atspi.EditableText.insert_text(a, pos if pos >= 0 else count, text, len(text.encode()))
    except Exception as e:
        raise ValueError(f'[{n}] "{name_of(a)}" isn\'t an editable text field ({e})')
    time.sleep(0.1)
    msg = f'typed {len(text)} chars into "{name_of(a)}"'
    if submit:
        names = action_names(a)
        if "activate" in names:
            do_action(a, names.index("activate"))
        else:
            try:
                Atspi.Component.grab_focus(a)
            except Exception:
                pass
            try:
                portal_key("Return", expect=a)
            except ValueError as e:
                return msg + f", but couldn't press Enter: {e}"
        msg += " and pressed Enter"
    return msg


def scroll(n, direction):
    if not n:
        raise ValueError("scroll needs a ref (an element inside the area to scroll to)")
    a = ref(n)
    try:
        Atspi.Component.scroll_to(a, Atspi.ScrollType.TOP_EDGE if direction == "up" else Atspi.ScrollType.ANYWHERE)
        return f'scrolled to "{name_of(a)}"'
    except Exception as e:
        raise ValueError(f"can't scroll that element ({e})")


def select(n, option):
    a = ref(n)
    for i in range(a.get_child_count()):
        c = a.get_child_at_index(i)
        if c is not None and name_of(c).lower() == option.lower():
            try:
                Atspi.Selection.select_child(a, i)
            except Exception:
                names = action_names(c)
                if not names:
                    raise ValueError(f'can\'t select "{option}"')
                do_action(c, 0)
            return f'selected "{name_of(c)}"'
    raise ValueError(f'no option "{option}" in [{n}]')


# ---------- apps ----------

def app_list(query):
    q = (query or "").lower()
    out = []
    for info_ in Gio.AppInfo.get_all():
        if not info_.should_show():
            continue
        nm = info_.get_display_name() or ""
        aid = info_.get_id() or ""
        if q and q not in nm.lower() and q not in aid.lower():
            continue
        out.append({"id": aid, "name": nm})
    out.sort(key=lambda x: x["name"].lower())
    return out[:60]


def launch(argv):
    """Start an app in its own systemd scope, so it outlives wang (and wang's restarts)."""
    if subprocess.run(["which", "systemd-run"], capture_output=True).returncode == 0:
        argv = ["systemd-run", "--user", "--quiet", "--collect", "--scope", "--slice=app.slice"] + argv
    subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)


def wait_new_window(before, launched):
    from collections import Counter
    deadline = time.time() + 15
    while time.time() < deadline:
        time.sleep(0.5)
        now = list_windows()
        counts = Counter((w["app"], w["title"]) for w in now)
        grew = {k for k, c in counts.items() if c > before.get(k, 0)}
        new = [w for w in now if (w["app"], w["title"]) in grew]
        if new:
            # The active one, else the newest of that kind.
            return {"opened": launched, "window": next((w for w in new if w["active"]), new[-1])}
    return {"opened": launched, "window": None, "note": "no new window yet (it may already have been open, or still starting)"}


def window_counts():
    from collections import Counter
    return Counter((w["app"], w["title"]) for w in list_windows())


def open_target(target):
    t = target.strip()
    before = window_counts()
    if "://" in t or t.startswith("/") or t.startswith("~"):
        uri = t if "://" in t else Gio.File.new_for_path(os.path.expanduser(t)).get_uri()
        launch(["gio", "open", uri])
        return wait_new_window(before, uri)
    cands = app_list(t)
    exact = [c for c in cands if c["name"].lower() == t.lower() or c["id"].lower() in (t.lower(), t.lower() + ".desktop")]
    pick = (exact or cands or [None])[0]
    if pick is None:
        raise ValueError(f'no app matching "{t}" (apps lists them)')
    launch(["gtk-launch", pick["id"]])
    return wait_new_window(before, pick["name"])


def launch_argv(argv, label):
    """Open an app with a specific command line (a profile, a folder)."""
    before = window_counts()
    launch(list(argv))
    return wait_new_window(before, label)


# ---------- portal: screenshot, pointer, keys ----------

class Portal:
    def __init__(self):
        self.bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        self.session = None
        self.stream = None
        self.size = None
        self.loop = GLib.MainLoop()
        self.counter = 0

    def _request(self, iface, method, args_fn, wait=120):
        self.counter += 1
        token = f"wang{os.getpid()}_{self.counter}"
        sender = self.bus.get_unique_name()[1:].replace(".", "_")
        path = f"/org/freedesktop/portal/desktop/request/{sender}/{token}"
        result = {}

        def on_resp(conn, snd, obj, ifc, sig, params, _):
            code, res = params.unpack()
            result["code"] = code
            result["res"] = res
            self.loop.quit()

        sub = self.bus.signal_subscribe("org.freedesktop.portal.Desktop", "org.freedesktop.portal.Request", "Response", path, None, Gio.DBusSignalFlags.NO_MATCH_RULE, on_resp, None)
        self.bus.call_sync("org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", iface, method, args_fn(token), None, Gio.DBusCallFlags.NONE, -1, None)
        GLib.timeout_add_seconds(wait, self.loop.quit)
        self.loop.run()
        self.bus.signal_unsubscribe(sub)
        if result.get("code") != 0:
            raise ValueError(f"{method} was cancelled or refused (code {result.get('code')}): the remote-desktop permission wasn't given")
        return result["res"]

    def start(self, interactive=False):
        """interactive: allowed to show GNOME's permission dialog (only `wang setup screen`, at
        the laptop). Otherwise only a saved permission is used, so a remote user is never left
        waiting on a dialog nobody can click."""
        if self.session:
            return
        tok_file = os.path.join(state_dir, "portal-restore-token")
        restore = open(tok_file).read().strip() if os.path.exists(tok_file) else ""
        if not restore and not interactive:
            raise ValueError("screen control (screenshots, clicks at x,y, key presses) isn't set up: run `wang setup screen` once at the laptop")
        wait = 120 if interactive else 15
        rd = "org.freedesktop.portal.RemoteDesktop"
        res = self._request(rd, "CreateSession", lambda t: GLib.Variant("(a{sv})", ({"handle_token": GLib.Variant("s", t), "session_handle_token": GLib.Variant("s", t + "s")},)))
        self.session = res["session_handle"]
        opts = {"handle_token": None, "types": GLib.Variant("u", 3), "persist_mode": GLib.Variant("u", 2)}
        if restore:
            opts["restore_token"] = GLib.Variant("s", restore)
        self._request(rd, "SelectDevices", lambda t: GLib.Variant("(oa{sv})", (self.session, {**{k: v for k, v in opts.items() if v is not None}, "handle_token": GLib.Variant("s", t)})))
        self._request("org.freedesktop.portal.ScreenCast", "SelectSources", lambda t: GLib.Variant("(oa{sv})", (self.session, {"handle_token": GLib.Variant("s", t), "types": GLib.Variant("u", 1), "multiple": GLib.Variant("b", False)})))
        try:
            res = self._request(rd, "Start", lambda t: GLib.Variant("(osa{sv})", (self.session, "", {"handle_token": GLib.Variant("s", t)})), wait=wait)
        except ValueError:
            self.session = None
            if not interactive:
                raise ValueError("the saved screen-control permission didn't work (GNOME asked again): run `wang setup screen` at the laptop")
            raise
        if res.get("restore_token"):
            os.makedirs(state_dir, exist_ok=True)
            with open(tok_file, "w") as f:
                f.write(res["restore_token"])
            os.chmod(tok_file, 0o600)
        streams = res.get("streams") or []
        if not streams:
            raise ValueError("the portal gave no screen to share")
        self.stream, props = streams[0]
        self.size = props.get("size")
        self.pos = props.get("position") or (0, 0)

    def call(self, method, sig, args):
        self.start()
        self.bus.call_sync("org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.RemoteDesktop", method, GLib.Variant(sig, args), None, Gio.DBusCallFlags.NONE, -1, None)

    def screenshot(self, path, max_w):
        self.start()
        ret, fds = self.bus.call_with_unix_fd_list_sync("org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.ScreenCast", "OpenPipeWireRemote", GLib.Variant("(oa{sv})", (self.session, {})), None, Gio.DBusCallFlags.NONE, -1, None, None)
        fd = fds.get(ret.unpack()[0])
        try:
            pipeline = ["gst-launch-1.0", "-q", "pipewiresrc", f"fd={fd}", f"path={self.stream}", "num-buffers=1", "do-timestamp=true", "!",
                        "videoconvert", "!", "videoscale", "!", f"video/x-raw,width={min(max_w, (self.size or (max_w, 0))[0])},pixel-aspect-ratio=1/1", "!",
                        "jpegenc", "quality=80", "!", "filesink", f"location={path}"]
            subprocess.run(pipeline, pass_fds=(fd,), timeout=15, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        finally:
            os.close(fd)
        return {"path": path, "screen": list(self.size) if self.size else None}


portal = None


def get_portal():
    global portal
    if portal is None:
        portal = Portal()
    return portal


KEYSYMS = {
    "return": 0xff0d, "enter": 0xff0d, "tab": 0xff09, "escape": 0xff1b, "esc": 0xff1b, "backspace": 0xff08, "delete": 0xffff,
    "up": 0xff52, "down": 0xff54, "left": 0xff51, "right": 0xff53, "home": 0xff50, "end": 0xff57, "pageup": 0xff55, "pagedown": 0xff56,
    "space": 0x20, "ctrl": 0xffe3, "control": 0xffe3, "shift": 0xffe1, "alt": 0xffe9, "super": 0xffeb, "f5": 0xffc2, "f11": 0xffc8,
    "playpause": 0x1008ff14, "next": 0x1008ff17, "prev": 0x1008ff16, "volumeup": 0x1008ff13, "volumedown": 0x1008ff11, "mute": 0x1008ff12,
}


def keysym(k):
    k = k.strip().lower()
    if k in KEYSYMS:
        return KEYSYMS[k]
    if len(k) == 1:
        return ord(k)
    raise ValueError(f'unknown key "{k}"')


def portal_key(combo, expect=None, window=None):
    """Press keys (e.g. "ctrl+l", "Return"). Refused unless the intended window is active."""
    aw = active_window()
    if aw is None:
        raise ValueError("no active window to send keys to")
    app, w = aw
    if is_terminal(app):
        raise ValueError("wang doesn't send keys to terminal windows (use the shell tool)")
    if window:
        want = windows[window - 1][1] if 1 <= window <= len(windows) else None
        if want is None or name_of(want) != name_of(w):
            raise ValueError(f'the active window is "{name_of(w)}", not the one asked for; focus it first')
    if expect is not None:
        e_app = expect.get_application()
        if e_app is None or name_of(e_app) != name_of(app):
            raise ValueError(f'keys would go to "{name_of(w)}", not the field\'s window')
    p = get_portal()
    p.start()
    syms = [keysym(k) for k in combo.split("+")]
    for s in syms:
        p.call("NotifyKeyboardKeysym", "(oa{sv}iu)", (p.session, {}, s, 1))
    for s in reversed(syms):
        p.call("NotifyKeyboardKeysym", "(oa{sv}iu)", (p.session, {}, s, 0))
    return f'pressed {combo} in "{name_of(w)}"'


def click_at(x, y, button):
    p = get_portal()
    p.start()
    p.call("NotifyPointerMotionAbsolute", "(oa{sv}udd)", (p.session, {}, p.stream, float(x), float(y)))
    b = {"left": 272, "right": 273, "middle": 274}[button or "left"]
    p.call("NotifyPointerButton", "(oa{sv}iu)", (p.session, {}, b, 1))
    p.call("NotifyPointerButton", "(oa{sv}iu)", (p.session, {}, b, 0))
    return f"{button or 'left'}-clicked at {x},{y}"


def screenshot(max_w):
    fd, path = tempfile.mkstemp(prefix="wang-shot-", suffix=".jpg")
    os.close(fd)
    r = get_portal().screenshot(path, max_w)
    return r


# ---------- main loop ----------

def handle(req):
    op = req.get("op")
    if op == "ping":
        return "pong"
    if op == "windows":
        return list_windows()
    if op == "view":
        return view(req.get("window"))
    if op == "info":
        return info(int(req["ref"]))
    if op == "click":
        return click(int(req["ref"]))
    if op == "type":
        return type_text(int(req["ref"]), req.get("text", ""), bool(req.get("submit")), req.get("replace", True))
    if op == "select":
        return select(int(req["ref"]), req.get("option", ""))
    if op == "scroll":
        return scroll(req.get("ref"), req.get("direction", "down"))
    if op == "apps":
        return app_list(req.get("query"))
    if op == "open":
        return open_target(req["target"])
    if op == "launch":
        return launch_argv(req["argv"], req.get("label", ""))
    if op == "key":
        return portal_key(req["keys"], window=req.get("window"))
    if op == "screenshot":
        return screenshot(int(req.get("max_width", 1280)))
    if op == "click_at":
        return click_at(req["x"], req["y"], req.get("button"))
    if op == "portal":
        get_portal().start(interactive=True)
        return {"ok": True, "screen": list(portal.size) if portal.size else None}
    if op == "window_info":
        app, w = pick_window(req.get("window"))
        return {"app": name_of(app), "title": name_of(w), "terminal": is_terminal(app), "active": "active" in states(w)}
    raise ValueError(f"unknown op {op}")


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except Exception as e:
            print(json.dumps({"id": None, "ok": False, "error": f"bad request: {e}"}), flush=True)
            continue
        try:
            out = {"id": req.get("id"), "ok": True, "result": handle(req)}
        except Exception as e:
            out = {"id": req.get("id"), "ok": False, "error": str(e) or type(e).__name__}
        print(json.dumps(out), flush=True)


if __name__ == "__main__":
    main()
