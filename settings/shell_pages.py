"""the settings pages for sevenshell which save along w sevenwms config"""

import json
import os
import shutil
import subprocess
import tomllib

from gi.repository import Gdk, GdkPixbuf, GLib, Gtk

HERE = os.path.dirname(os.path.realpath(__file__))
CONFIG = os.path.join(
    os.environ.get("XDG_CONFIG_HOME", os.path.expanduser("~/.config")), "sevenshell", "config.toml")
RUNTIME = os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/tmp"), "sevenshell")

MODULES = {
    "desktop": "Arch logo (opens a terminal)",
    "clock": "Clock",
    "audio": "Volume",
    "network": "Network",
    "perf": "Power mode",
    "power": "Power menu",
    "battery": "Battery",
    "title": "Focused window title",
    "minimap": "Canvas map",
}
POSITIONS = ["top-left", "top", "top-right",
             "left", "center", "right",
             "bottom-left", "bottom", "bottom-right"]
BAR = 30            # bar height and top margins count from under it
AUTO_HEIGHT = 78    # how tall a normal notification is when height is 0
SNAP = 12           # how close in real px before edges and the center line snap


def screen_size():
    display = Gdk.Display.get_default()
    monitor = display.get_primary_monitor() or display.get_monitor(0)
    if monitor is None:
        return 1920, 1080
    rect = monitor.get_geometry()
    return rect.width, rect.height


class NotificationPlacer(Gtk.DrawingArea):
    """a small copy of ur screen w a notification u can drag and resize to place them"""

    PREVIEW_W = 480

    def __init__(self, values, wallpaper, on_change):
        super().__init__()
        self.sw, self.sh = screen_size()
        self.scale = self.PREVIEW_W / self.sw
        self.pw, self.ph = self.PREVIEW_W, round(self.sh * self.scale)
        self.set_size_request(self.pw, self.ph)
        self.on_change = on_change
        self.values = dict(values)
        self.rect = self.rect_from(self.values)   # real px x y w h
        self.drag = None      # mode start pointer and start rect
        self.guides = []      # snap lines to show while dragging
        self.add_events(Gdk.EventMask.BUTTON_PRESS_MASK | Gdk.EventMask.BUTTON_RELEASE_MASK
                        | Gdk.EventMask.POINTER_MOTION_MASK)
        self.connect("draw", self.draw)
        self.connect("button-press-event", self.pressed)
        self.connect("button-release-event", self.released)
        self.connect("motion-notify-event", self.moved)
        self.wallpaper = None
        if wallpaper:
            try:
                pix = GdkPixbuf.Pixbuf.new_from_file(os.path.expanduser(wallpaper))
                scale = max(self.pw / pix.get_width(), self.ph / pix.get_height())
                w, h = int(pix.get_width() * scale) + 1, int(pix.get_height() * scale) + 1
                pix = pix.scale_simple(w, h, GdkPixbuf.InterpType.BILINEAR)
                self.wallpaper = pix.new_subpixbuf((w - self.pw) // 2, (h - self.ph) // 2,
                                                   self.pw, self.ph)
            except GLib.Error:
                pass

    # config to rectangle

    def rect_from(self, v):
        w = v["width"]
        h = v["height"] or AUTO_HEIGHT
        pos = v["position"] if v["position"] in POSITIONS else "top-right"
        col, row = POSITIONS.index(pos) % 3, POSITIONS.index(pos) // 3
        x = [v["margin_x"], (self.sw - w) / 2, self.sw - w - v["margin_x"]][col]
        # layer surfaces sit in the space the bar leaves
        y = [BAR + v["margin_y"], BAR + (self.sh - BAR - h) / 2,
             self.sh - h - v["margin_y"]][row]
        return [x, y, w, h]

    def values_from(self, rect, auto_height):
        x, y, w, h = rect
        cx, cy = x + w / 2, y + h / 2
        if abs(cx - self.sw / 2) <= SNAP:
            col, mx = 1, self.values["margin_x"]
        elif cx < self.sw / 2:
            col, mx = 0, x
        else:
            col, mx = 2, self.sw - x - w
        usable_mid = BAR + (self.sh - BAR) / 2
        if abs(cy - usable_mid) <= SNAP:
            row, my = 1, self.values["margin_y"]
        elif cy < usable_mid:
            row, my = 0, y - BAR
        else:
            row, my = 2, self.sh - y - h
        return {"position": POSITIONS[row * 3 + col], "margin_x": max(0, round(mx)),
                "margin_y": max(0, round(my)), "width": round(w),
                "height": 0 if auto_height else round(h)}

    # pointer

    def real(self, event):
        return event.x / self.scale, event.y / self.scale

    def hit(self, px, py):
        """what pressing here does like move or an edge set to resize or none"""
        x, y, w, h = self.rect
        grab = 7 / self.scale
        if not (x - grab <= px <= x + w + grab and y - grab <= py <= y + h + grab):
            return None
        edges = ""
        if abs(px - x) <= grab:
            edges += "l"
        elif abs(px - (x + w)) <= grab:
            edges += "r"
        if abs(py - y) <= grab:
            edges += "t"
        elif abs(py - (y + h)) <= grab:
            edges += "b"
        return edges or "move"

    CURSORS = {"move": "grab", "l": "w-resize", "r": "e-resize", "t": "n-resize",
               "b": "s-resize", "lt": "nw-resize", "rt": "ne-resize", "lb": "sw-resize",
               "rb": "se-resize"}

    def set_cursor(self, mode):
        window = self.get_window()
        if window:
            name = "grabbing" if mode == "move" and self.drag else self.CURSORS.get(mode)
            window.set_cursor(Gdk.Cursor.new_from_name(self.get_display(), name)
                              if name else None)

    def pressed(self, _w, event):
        if event.button != 1:
            return
        mode = self.hit(*self.real(event))
        if mode:
            self.drag = (mode, self.real(event), list(self.rect))
            self.set_cursor(mode)

    def released(self, _w, event):
        if self.drag:
            self.drag = None
            self.guides = []
            self.set_cursor(self.hit(*self.real(event)))
            self.queue_draw()

    def moved(self, _w, event):
        px, py = self.real(event)
        if not self.drag:
            self.set_cursor(self.hit(px, py))
            return
        mode, (sx, sy), (x, y, w, h) = self.drag
        dx, dy = px - sx, py - sy
        min_w, min_h = 200, 40
        self.guides = []
        if mode == "move":
            x, y = x + dx, y + dy
            x, y = self.snap_move(x, y, w, h)
        else:
            if "l" in mode:
                new_x = min(x + dx, x + w - min_w)
                w, x = w + (x - new_x), new_x
            if "r" in mode:
                w = max(min_w, w + dx)
            if "t" in mode:
                new_y = min(y + dy, y + h - min_h)
                h, y = h + (y - new_y), new_y
            if "b" in mode:
                h = max(min_h, h + dy)
        # keep it on screen and under the bar
        w, h = min(w, self.sw), min(h, self.sh - BAR)
        x = min(max(x, 0), self.sw - w)
        y = min(max(y, BAR), self.sh - h)
        self.rect = [x, y, w, h]
        resized_height = mode != "move" and ("t" in mode or "b" in mode)
        auto = self.values["height"] == 0 and not resized_height
        self.values = self.values_from(self.rect, auto)
        self.on_change(dict(self.values))
        self.queue_draw()

    def snap_move(self, x, y, w, h):
        """uhhh snap to the screen edges and center lines"""
        gap = 8
        for target, guide in ((gap, ("v", 0)), (self.sw - w - gap, ("v", self.sw)),
                              ((self.sw - w) / 2, ("v", self.sw / 2))):
            if abs(x - target) <= SNAP:
                x = target
                self.guides.append(guide)
                break
        usable_mid = BAR + (self.sh - BAR) / 2
        for target, guide in ((BAR + gap, ("h", BAR)), (self.sh - h - gap, ("h", self.sh)),
                              (usable_mid - h / 2, ("h", usable_mid))):
            if abs(y - target) <= SNAP:
                y = target
                self.guides.append(guide)
                break
        return x, y

    def set_auto_height(self):
        self.values["height"] = 0
        self.rect = self.rect_from(self.values)
        self.on_change(dict(self.values))
        self.queue_draw()

    # drawing

    def draw(self, _w, cr):
        s = self.scale
        if self.wallpaper:
            Gdk.cairo_set_source_pixbuf(cr, self.wallpaper, 0, 0)
        else:
            cr.set_source_rgb(0.08, 0.08, 0.09)
        cr.rectangle(0, 0, self.pw, self.ph)
        cr.fill()
        cr.set_source_rgb(0, 0, 0)
        cr.rectangle(0, 0, self.pw, BAR * s)
        cr.fill()
        # snap guides
        cr.set_source_rgba(1, 1, 1, 0.6)
        cr.set_line_width(1)
        cr.set_dash([3, 3])
        for axis, at in self.guides:
            if axis == "v" and 0 < at < self.sw:
                cr.move_to(at * s + 0.5, BAR * s)
                cr.line_to(at * s + 0.5, self.ph)
            elif axis == "h" and BAR < at < self.sh:
                cr.move_to(0, at * s + 0.5)
                cr.line_to(self.pw, at * s + 0.5)
        cr.stroke()
        cr.set_dash([])
        # the notification as it looks
        x, y, w, h = (v * s for v in self.rect)
        cr.set_source_rgb(0, 0, 0)
        rounded(cr, x, y, w, h, 7 * s)
        cr.fill_preserve()
        cr.set_source_rgb(1, 1, 1)
        cr.set_line_width(1.5 if not self.drag else 2)
        cr.stroke()
        pad = 14 * s
        for i, (width, height) in enumerate(((0.25, 11), (0.5, 14), (0.75, 13))):
            ty = y + pad + i * 19 * s
            if ty + height * s > y + h - pad / 2:
                break
            cr.rectangle(x + pad, ty, (w - 2 * pad) * width, height * s * 0.55)
            cr.fill()
        # resize handles at the corners
        for hx, hy in ((x, y), (x + w, y), (x, y + h), (x + w, y + h)):
            cr.rectangle(hx - 3, hy - 3, 6, 6)
            cr.fill()
        cr.set_source_rgba(1, 1, 1, 0.5)
        cr.set_line_width(1)
        cr.rectangle(0.5, 0.5, self.pw - 1, self.ph - 1)
        cr.stroke()


def rounded(cr, x, y, w, h, r):
    import math
    cr.new_sub_path()
    cr.arc(x + w - r, y + r, r, -math.pi / 2, 0)
    cr.arc(x + w - r, y + h - r, r, 0, math.pi / 2)
    cr.arc(x + r, y + h - r, r, math.pi / 2, math.pi)
    cr.arc(x + r, y + r, r, math.pi, 3 * math.pi / 2)
    cr.close_path()


def shell_defaults_path():
    """sevenshell defaults from the running shell or else the repo"""
    for path in (os.path.join(RUNTIME, "defaults.toml"),
                 os.path.join(HERE, "..", "..", "sevenshell", "config.default.toml")):
        if os.path.exists(path):
            return path
    return None


def sevenshell_binary():
    for candidate in (shutil.which("sevenshell"),
                      os.path.join(HERE, "..", "..", "sevenshell", "target", "release", "sevenshell")):
        if candidate and os.path.exists(candidate):
            return candidate
    return None


class ShellPages:
    """mixed into the settings window and needs its helpers passed in"""

    def load_shell(self, merge):
        path = shell_defaults_path()
        self.shell_template = open(path).read() if path else None
        if not self.shell_template:
            return
        user = {}
        if os.path.exists(CONFIG):
            try:
                with open(CONFIG, "rb") as f:
                    user = tomllib.load(f)
            except (OSError, tomllib.TOMLDecodeError) as err:
                self.say(f"sevenshell's config has a mistake ({err}); showing defaults.", error=True)
        self.data["shell"] = merge(tomllib.loads(self.shell_template), user)

    def has_shell(self):
        return getattr(self, "shell_template", None) is not None

    def build_shell_pages(self, get, put):
        if not self.has_shell():
            return
        self.page_bar(get, put)
        self.page_notifications(get, put)
        self.page_lock(get, put)

    # bar

    def page_bar(self, get, put):
        g = self.page("bar", "Bar")
        self.heading(g, "Modules")
        note = Gtk.Label(xalign=0, wrap=True, max_width_chars=70)
        note.set_markup("<small>Left to right. The same bar is on every monitor.</small>")
        g.attach(note, 0, g.row, 2, 1)
        g.row += 1
        for section, title in (("left", "Left"), ("center", "Middle"), ("right", "Right")):
            self.module_list(g, f"shell.bar.{section}", title, get, put)

        self.heading(g, "Clock")
        entry = self.text(g, "shell.bar.clock_format", "Format", width=24,
                          hint="%H hour, %M minute, %m month, %d day, %y year, %A weekday, %B month name")
        preview = Gtk.Label(xalign=0)
        preview.get_style_context().add_class("dim-label")

        def update(*_):
            now = GLib.DateTime.new_now_local()
            preview.set_text(now.format(entry.get_text()) or "(not a valid format)")
        entry.connect("changed", update)
        update()
        self.row(g, "Shows", preview)

        self.heading(g, "Buttons")
        self.text(g, "shell.bar.terminal", "Arch logo starts")
        self.text(g, "shell.bar.tray", "Trays script",
                  hint="Run with audio, wifi, perf or power when those modules are clicked.")

    def module_list(self, g, path, title, get, put):
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        # one module per line in bar order
        row_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        box.pack_start(row_box, False, False, 0)
        add = Gtk.ComboBoxText(tooltip_text="Add a module", halign=Gtk.Align.START)
        box.pack_start(add, False, False, 0)

        def rebuild():
            for child in row_box.get_children():
                row_box.remove(child)
            modules = list(get(self.data, path, []))
            for i, name in enumerate(modules):
                chip = Gtk.Box(spacing=0)
                chip.get_style_context().add_class("linked")
                for text, tip, action in ((name, MODULES.get(name, name), None),
                                          ("↑", "Earlier (further left)", lambda i=i: move(i, -1)),
                                          ("↓", "Later (further right)", lambda i=i: move(i, 1)),
                                          ("×", "Remove", lambda i=i: remove(i))):
                    button = Gtk.Button(label=text, tooltip_text=tip)
                    if action:
                        button.connect("clicked", lambda _b, a=action: a())
                    else:
                        button.set_sensitive(False)
                    chip.pack_start(button, False, False, 0)
                chip.get_children()[0].set_size_request(90, -1)
                row_box.pack_start(chip, False, False, 0)
            if not modules:
                row_box.pack_start(Gtk.Label(label="(empty)", xalign=0), False, False, 0)
            add.remove_all()
            add.append("", "Add…")
            used = {m for part in ("left", "center", "right")
                    for m in get(self.data, f"shell.bar.{part}", [])}
            for name, text in MODULES.items():
                if name not in used:
                    add.append(name, text)
            add.set_active_id("")
            row_box.show_all()

        def change(modules):
            put(self.data, path, modules)
            # the add lists in other sections change too
            for other in self.module_rebuilds:
                other()

        def move(i, step):
            modules = list(get(self.data, path, []))
            j = i + step
            if 0 <= j < len(modules):
                modules[i], modules[j] = modules[j], modules[i]
                change(modules)

        def remove(i):
            modules = list(get(self.data, path, []))
            del modules[i]
            change(modules)

        def added(combo):
            name = combo.get_active_id()
            if name:
                change(list(get(self.data, path, [])) + [name])
        add.connect("changed", added)

        if not hasattr(self, "module_rebuilds") or path.endswith(".left"):
            self.module_rebuilds = []
        self.module_rebuilds.append(rebuild)
        rebuild()
        self.row(g, title, box)

    # lock screen and idle

    def image_picker(self, g, label, path_key, get, put, size, placeholder, hint=None):
        """a file picker for an image w a thumbnail"""
        chooser = Gtk.FileChooserButton(title=label, action=Gtk.FileChooserAction.OPEN)
        images = Gtk.FileFilter()
        images.set_name("Images")
        for mime in ("image/png", "image/jpeg", "image/webp"):
            images.add_mime_type(mime)
        chooser.add_filter(images)
        pictures = os.path.expanduser("~/Pictures")
        if os.path.isdir(pictures):
            chooser.set_current_folder(pictures)
        preview = Gtk.Image()

        def show(path):
            path = os.path.expanduser(path or "")
            if path and os.path.isfile(path):
                try:
                    preview.set_from_pixbuf(
                        GdkPixbuf.Pixbuf.new_from_file_at_scale(path, *size, True))
                    return
                except GLib.Error:
                    pass
            preview.set_from_icon_name("image-missing", Gtk.IconSize.DIALOG)
        current = get(self.data, path_key) or ""
        if current and os.path.isfile(os.path.expanduser(current)):
            chooser.set_filename(os.path.expanduser(current))
        show(current or placeholder)

        def picked(button):
            path = button.get_filename() or ""
            home = os.path.expanduser("~")
            put(self.data, path_key, "~" + path[len(home):] if path.startswith(home + "/") else path)
            show(path)
        chooser.connect("file-set", picked)
        reset = Gtk.Button(label="Default")

        def cleared(_):
            put(self.data, path_key, "")
            chooser.unselect_all()
            show(placeholder)
        reset.connect("clicked", cleared)
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        box.pack_start(preview, False, False, 0)
        row = Gtk.Box(spacing=6)
        row.pack_start(chooser, False, False, 0)
        row.pack_start(reset, False, False, 0)
        box.pack_start(row, False, False, 0)
        self.row(g, label, box, hint)

    def format_entry(self, g, path, label, get, put):
        entry = self.text(g, path, label, width=24,
                          hint="%H hour, %M minute, %I 12-hour, %p AM/PM, %A weekday, "
                               "%B month, %d day, %Y year")
        preview = Gtk.Label(xalign=0)
        preview.get_style_context().add_class("dim-label")

        def update(*_):
            preview.set_text(GLib.DateTime.new_now_local().format(entry.get_text())
                             or "(not a valid format)")
        entry.connect("changed", update)
        update()
        self.row(g, "", preview)

    def page_lock(self, get, put):
        g = self.page("lock", "Lock & idle")
        self.heading(g, "Lock screen")
        user = GLib.get_user_name()
        default_face = next((p for p in (os.path.expanduser("~/.face"),
                                          f"/usr/share/sddm/faces/{user}.face.icon",
                                          f"/var/lib/AccountsService/icons/{user}")
                             if os.path.isfile(p)), "")
        self.image_picker(g, "Wallpaper", "shell.lock.wallpaper", get, put, (320, 180), "",
                          hint="Shown behind the clock and the password box. Default: black.")
        self.image_picker(g, "Profile picture", "shell.lock.avatar", get, put, (96, 96),
                          default_face, hint="Default: ~/.face, else your SDDM picture.")
        self.format_entry(g, "shell.lock.clock_format", "Time format", get, put)
        self.format_entry(g, "shell.lock.date_format", "Date format", get, put)
        self.text(g, "shell.lock.message", "Hint", width=24,
                  hint="Shown at the bottom left until you press a key. Empty for none.")
        self.switch(g, "shell.lock.lock_before_sleep", "Lock when the computer sleeps")
        lock_now = Gtk.Button(label="Lock now")
        lock_now.connect("clicked", lambda _b: self.lock_now())
        self.row(g, "", lock_now, hint="Uses the saved settings: Save first to see changes.")

        self.idle_timeouts(g, get, put)

    def idle_timeouts(self, g, get, put):
        """sevenwm idle timeouts that live here if sevenshell is installed or on general if not"""
        self.heading(g, "When you're away")
        note = Gtk.Label(xalign=0, wrap=True)
        note.set_markup("<small>Counted from your last key press or mouse move. "
                        "Apps playing video hold all of these off.</small>")
        g.attach(note, 0, g.row, 2, 1)
        g.row += 1
        for key, label, default in (("lock_after", "Lock the screen after", 5),
                                    ("screen_off_after", "Turn the screen off after", 5),
                                    ("suspend_after", "Sleep after", 10)):
            self.minutes(g, f"idle.{key}", label, default, get, put)

    def minutes(self, g, path, label, default, get, put):
        seconds = int(get(self.data, path) or 0)
        on = Gtk.Switch(active=seconds > 0, valign=Gtk.Align.CENTER)
        adj = Gtk.Adjustment(value=(seconds / 60) if seconds else default, lower=1, upper=600,
                             step_increment=1, page_increment=10)
        spin = Gtk.SpinButton(adjustment=adj, numeric=True, sensitive=seconds > 0)

        def store(*_):
            spin.set_sensitive(on.get_active())
            put(self.data, path, int(round(spin.get_value() * 60)) if on.get_active() else 0)
        on.connect("notify::active", store)
        spin.connect("value-changed", store)
        box = Gtk.Box(spacing=10)
        box.pack_start(on, False, False, 0)
        box.pack_start(spin, False, False, 0)
        box.pack_start(Gtk.Label(label="minutes"), False, False, 0)
        self.row(g, label, box)

    def lock_now(self):
        binary = sevenshell_binary()
        if binary:
            subprocess.Popen([binary, "lock"], start_new_session=True)
        else:
            self.say("sevenshell isn't installed.", error=True)

    # notifications

    def page_notifications(self, get, put):
        g = self.page("notifications", "Notifications")
        self.heading(g, "Where they appear")
        hint = Gtk.Label(xalign=0, wrap=True)
        hint.set_markup("<small>Drag the notification where you want it; drag its edges or "
                        "corners to resize. It keeps to the nearest edge — or the middle — "
                        "at the distance you leave.</small>")
        g.attach(hint, 0, g.row, 2, 1)
        g.row += 1
        described = Gtk.Label(xalign=0, wrap=True, max_width_chars=28)

        def changed(values):
            for key, value in values.items():
                put(self.data, f"shell.notifications.{key}", value)
            pos = values["position"]
            col, row = POSITIONS.index(pos) % 3, POSITIONS.index(pos) // 3
            where = []
            if row != 1:
                where.append(f"{values['margin_y']} px from the {['top (under the bar)', '', 'bottom'][row]}")
            if col != 1:
                where.append(f"{values['margin_x']} px from the {['left', '', 'right'][col]}")
            if row == 1 and col == 1:
                where.append("centred on the screen")
            elif row == 1:
                where.append("centred top to bottom")
            elif col == 1:
                where.append("centred left to right")
            height = values["height"] or "fits the text"
            described.set_markup(
                f"<b>{values['width']} × {height}</b>\n" + GLib.markup_escape_text(",\n".join(where)))
        values = {key: get(self.data, f"shell.notifications.{key}")
                  for key in ("position", "margin_x", "margin_y", "width", "height")}
        placer = NotificationPlacer(values, get(self.data, "canvas.wallpaper"), changed)
        changed(placer.values)
        side = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8, valign=Gtk.Align.END)
        side.pack_start(described, False, False, 0)
        fit = Gtk.Button(label="Height: fit the text")
        fit.connect("clicked", lambda _b: placer.set_auto_height())
        side.pack_start(fit, False, False, 0)
        test = Gtk.Button(label="Save and send a test")
        test.connect("clicked", lambda _b: self.test_notification())
        side.pack_start(test, False, False, 0)
        box = Gtk.Box(spacing=16)
        box.pack_start(placer, False, False, 0)
        box.pack_start(side, False, False, 0)
        g.attach(box, 0, g.row, 2, 1)
        g.row += 1

        self.heading(g, "Popups")
        self.switch(g, "shell.notifications.do_not_disturb", "Do not disturb",
                    hint="Only critical notifications pop up; the rest go to the history below.")
        self.number(g, "shell.notifications.timeout", "Stay for (ms)", 500, 120000, 500,
                    hint="When the app doesn't say. Critical ones stay until dismissed.")
        self.number(g, "shell.notifications.history", "Keep in history", 0, 500, 10)

        self.heading(g, "Volume and brightness popup")
        self.number(g, "shell.osd.step", "Step per key press (%)", 1, 25)
        self.number(g, "shell.osd.duration", "Stay for (ms)", 200, 10000, 100)
        self.choice(g, "shell.osd.position", "Position", ["top", "bottom"])
        self.number(g, "shell.osd.max_volume", "Highest volume (%)", 100, 200, 5)

        self.heading(g, "History")
        clear = Gtk.Button(label="Clear")
        clear.connect("clicked", lambda _b: self.clear_history())
        self.row(g, "Past notifications", clear)
        self.history_list = Gtk.ListBox(selection_mode=Gtk.SelectionMode.NONE)
        frame = Gtk.Frame()
        frame.add(self.history_list)
        frame.set_hexpand(True)
        g.attach(frame, 0, g.row, 2, 1)
        g.row += 1
        self.refresh_history()

    def test_notification(self):
        self.save()
        if "Not saved" in self.status.get_text():
            return

        def send():
            subprocess.Popen(["notify-send", "-a", "Settings", "Test notification",
                              "Notifications will appear here."], start_new_session=True)
            GLib.timeout_add(500, lambda: self.refresh_history() and False)
            return False
        # sevenshell picks up the saved config within a sec
        GLib.timeout_add(1300, send)

    def refresh_history(self):
        lb = getattr(self, "history_list", None)
        if lb is None:
            return
        for child in lb.get_children():
            lb.remove(child)
        try:
            with open(os.path.join(RUNTIME, "notifications.json")) as f:
                history = json.load(f)
        except (OSError, json.JSONDecodeError):
            history = []
        for n in history:
            row = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2, margin=8)
            when = n.get("time", "")[11:16]
            head = Gtk.Label(xalign=0)
            app = GLib.markup_escape_text(n.get("app") or "")
            head.set_markup(f"<small>{app}  {when}</small>")
            head.get_style_context().add_class("dim-label")
            row.pack_start(head, False, False, 0)
            summary = Gtk.Label(xalign=0, wrap=True)
            summary.set_markup(f"<b>{GLib.markup_escape_text(n.get('summary', ''))}</b>")
            row.pack_start(summary, False, False, 0)
            if n.get("body"):
                body = Gtk.Label(xalign=0, wrap=True, max_width_chars=80)
                # bodies can have markup so fall back to plain text
                try:
                    from gi.repository import Pango
                    Pango.parse_markup(n["body"], -1, "\0")
                    body.set_markup(n["body"])
                except GLib.Error:
                    body.set_text(n["body"])
                row.pack_start(body, False, False, 0)
            lb.add(row)
        if not history:
            lb.add(Gtk.Label(label="Nothing yet.", xalign=0, margin=8))
        lb.show_all()

    def clear_history(self):
        binary = sevenshell_binary()
        if binary:
            subprocess.run([binary, "notifications", "clear"], timeout=5)
        GLib.timeout_add(200, lambda: self.refresh_history() and False)

    # saving

    def shell_text(self, write_config):
        """sevenshell config text or an error message"""
        if not self.has_shell():
            return None, None
        text = write_config(self.data["shell"], self.shell_template)
        binary = sevenshell_binary()
        if binary:
            import tempfile
            with tempfile.NamedTemporaryFile("w", suffix=".toml", delete=False) as tmp:
                tmp.write(text)
            check = subprocess.run([binary, "--check-config", tmp.name],
                                   capture_output=True, text=True, timeout=10)
            os.unlink(tmp.name)
            if check.returncode != 0:
                return None, "sevenshell: " + check.stdout.strip().removeprefix("error: ")
        return text, None

    def save_shell(self, text):
        os.makedirs(os.path.dirname(CONFIG), exist_ok=True)
        old = open(CONFIG).read() if os.path.exists(CONFIG) else None
        if old == text:
            return
        if old is not None:
            shutil.copy(CONFIG, CONFIG + ".bak")
        # write it next to the file then swap it in so the shell never reads half of it maybe
        with open(CONFIG + ".tmp", "w") as f:
            f.write(text)
        os.replace(CONFIG + ".tmp", CONFIG)
