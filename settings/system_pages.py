"""the system pages for network bluetooth sound and power that act right away"""

import json
import os
import socket
import subprocess
import threading

from gi.repository import GLib, Gtk


# results gathered off the ui thread so run can answer from here
_prefetched = {}


def run(*cmd, timeout=8, input=None):
    if input is None and cmd in _prefetched:
        return _prefetched[cmd]
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, input=input)
    except (OSError, subprocess.TimeoutExpired) as err:
        return subprocess.CompletedProcess(cmd, 1, "", str(err))


def nm_edit(target, commands):
    """change a connection thru nmclis editor so passwords never show up on a command line"""
    # a do u still want to save question can follow so yes answers it
    lines = [c.replace("\n", " ") for c in commands] + ["save persistent", "yes", "quit"]
    result = run("nmcli", "connection", "edit", *target, timeout=20, input="\n".join(lines) + "\n")
    errors = [line for line in result.stdout.splitlines() + result.stderr.splitlines()
              if "Error" in line]
    if errors:
        return subprocess.CompletedProcess(result.args, 1, "", errors[-1])
    return result


def prefetch(page):
    """run the commands a page needs off the ui thread so it builds fast"""
    results = {}

    def get(*cmd, timeout=8):
        results[cmd] = run(*cmd, timeout=timeout)
        return results[cmd]
    if page == "network":
        if get("nmcli", "-v").returncode == 0:
            get("nmcli", "radio", "wifi")
            for line in get("nmcli", "-t", "-f", "DEVICE,TYPE,STATE,CONNECTION",
                            "device").stdout.splitlines():
                dev = terse_split(line)[0]
                get("nmcli", "-t", "-f", "GENERAL.HWADDR,IP4.ADDRESS,IP4.GATEWAY,IP4.DNS",
                    "device", "show", dev)
            get("nmcli", "-t", "-f", "UUID", "connection", "show", "--active")
            get("nmcli", "-t", "-f", "NAME,UUID,TYPE,AUTOCONNECT", "connection", "show")
    elif page == "bluetooth":
        if "Controller" in get("bluetoothctl", "show", timeout=4).stdout:
            for line in get("bluetoothctl", "devices", "Paired", timeout=4).stdout.splitlines():
                parts = line.split(" ", 2)
                if len(parts) == 3 and parts[0] == "Device":
                    get("bluetoothctl", "info", parts[1], timeout=4)
    elif page == "sound":
        for what in ("sinks", "sources", "sink-inputs", "cards"):
            get("pactl", "-f", "json", "list", what)
        get("pactl", "get-default-sink")
        get("pactl", "get-default-source")
    elif page == "power":
        get("powerprofilesctl", "get")
        get("powerprofilesctl", "list")
    return results


def in_background(work, done):
    """run work off the ui thread then call done back on it"""
    def target():
        result = work()
        GLib.idle_add(lambda: done(result) and False)
    threading.Thread(target=target, daemon=True).start()


def terse_split(line):
    """split one line of nmcli -t output on unescaped colons"""
    fields, current, escaped = [], "", False
    for ch in line:
        if escaped:
            current += ch
            escaped = False
        elif ch == "\\":
            escaped = True
        elif ch == ":":
            fields.append(current)
            current = ""
        else:
            current += ch
    fields.append(current)
    return fields


def sevenwm_action(action):
    """send a keybind action to the running sevenwm"""
    path = os.environ.get("SEVENWM_SOCK")
    if not path:
        return False
    try:
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(2)
            sock.connect(path)
            sock.sendall((json.dumps({"action": action}) + "\n").encode())
        return True
    except OSError:
        return False


def clear(box):
    for child in box.get_children():
        box.remove(child)


def label(text, markup=False, dim=False, xalign=0.0, **kw):
    lab = Gtk.Label(xalign=xalign, **kw)
    if markup:
        lab.set_markup(text)
    else:
        lab.set_text(text)
    if dim:
        lab.get_style_context().add_class("dim-label")
    return lab


def bold(text):
    return f"<b>{GLib.markup_escape_text(text)}</b>"


class SystemPages:
    """mixed into the settings window which has stack and say"""

    SYSTEM_PAGES = [("network", "Network"), ("bluetooth", "Bluetooth"),
                    ("sound", "Sound"), ("power", "Power")]

    def build_system_pages(self):
        self.system_boxes = {}
        for name, title in self.SYSTEM_PAGES:
            box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8, margin=20)
            scroller = Gtk.ScrolledWindow()
            scroller.add(box)
            self.stack.add_titled(scroller, name, title)
            self.system_boxes[name] = box

    def is_system_page(self, name):
        return name in dict(self.SYSTEM_PAGES)

    def refresh_system_page(self, name=None):
        """rebuild a system page w its tools run on another thread so the window never freezes"""
        name = name or self.stack.get_visible_child_name()
        if not self.is_system_page(name):
            return
        box = self.system_boxes[name]
        if not box.get_children():
            box.pack_start(label("Loading…", dim=True), False, False, 0)
            box.show_all()
        if not hasattr(self, "system_refresh"):
            self.system_refresh = {}
        token = object()
        self.system_refresh[name] = token

        def done(results):
            # a newer refresh of this page started so let it uhh win
            if self.system_refresh.get(name) is not token:
                return
            _prefetched.clear()
            _prefetched.update(results)
            try:
                clear(box)
                getattr(self, f"fill_{name}")(box)
                box.show_all()
            finally:
                _prefetched.clear()
        in_background(lambda: prefetch(name), done)

    # helpers

    def section(self, box, title, *extra):
        row = Gtk.Box(spacing=8, margin_top=12 if box.get_children() else 0)
        row.pack_start(label(bold(title), markup=True), False, False, 0)
        for widget in reversed(extra):
            row.pack_end(widget, False, False, 0)
        box.pack_start(row, False, False, 0)

    def listbox(self, box):
        frame = Gtk.Frame()
        lb = Gtk.ListBox(selection_mode=Gtk.SelectionMode.NONE)
        frame.add(lb)
        box.pack_start(frame, False, False, 0)
        return lb

    def list_row(self, lb, title, subtitle=None, widgets=(), markup=False):
        row = Gtk.Box(spacing=10, margin=8)
        text = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        text.pack_start(label(title, markup=markup), False, False, 0)
        if subtitle:
            sub = label(subtitle, dim=True)
            sub.set_line_wrap(True)
            text.pack_start(sub, False, False, 0)
        row.pack_start(text, True, True, 0)
        for widget in widgets:
            widget.set_valign(Gtk.Align.CENTER)
            row.pack_start(widget, False, False, 0)
        lb.add(row)
        return row

    def button(self, text, callback, *args, danger=False):
        btn = Gtk.Button(label=text)
        if danger:
            btn.get_style_context().add_class("destructive-action")
        btn.connect("clicked", lambda _b: callback(*args))
        return btn

    def confirm(self, question, detail=None):
        dialog = Gtk.MessageDialog(transient_for=self, modal=True,
                                   message_type=Gtk.MessageType.QUESTION,
                                   buttons=Gtk.ButtonsType.OK_CANCEL, text=question)
        if detail:
            dialog.format_secondary_text(detail)
        answer = dialog.run()
        dialog.destroy()
        return answer == Gtk.ResponseType.OK

    def act(self, cmd, done_text=None, refresh=True, background=False, work=None):
        """run a command then report if it failed and refresh the page"""
        def finish(result):
            if result.returncode != 0:
                message = (result.stderr or result.stdout).strip().splitlines()
                self.say(f"{' '.join(cmd[:3])}: {message[-1] if message else 'failed'}", error=True)
            elif done_text:
                self.say(done_text)
            if refresh:
                self.refresh_system_page()
        work = work or (lambda: run(*cmd, timeout=40))
        if background:
            in_background(work, finish)
        else:
            finish(work())

    # network

    def fill_network(self, box):
        if run("nmcli", "-v").returncode != 0:
            box.pack_start(label("NetworkManager (nmcli) isn't available."), False, False, 0)
            return
        radio = run("nmcli", "radio", "wifi").stdout.strip() == "enabled"
        wifi = Gtk.Switch(active=radio)
        wifi.connect("notify::active", lambda s, _: self.act(
            ["nmcli", "radio", "wifi", "on" if s.get_active() else "off"]))
        self.section(box, "Wi-Fi", wifi)

        self.section(box, "Devices")
        lb = self.listbox(box)
        for line in run("nmcli", "-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "device").stdout.splitlines():
            dev, kind, state, conn = (terse_split(line) + [""] * 4)[:4]
            if kind in ("loopback", "wifi-p2p", "bridge", "tun") or dev.startswith("veth"):
                continue
            details = {}
            for field in run("nmcli", "-t", "-f", "GENERAL.HWADDR,IP4.ADDRESS,IP4.GATEWAY,IP4.DNS",
                             "device", "show", dev).stdout.splitlines():
                key, _, value = field.partition(":")
                if value:
                    details.setdefault(key.split("[")[0], []).append(value.replace("\\:", ":"))
            parts = [f"{kind} · {state}" + (f" · {conn}" if conn else "")]
            for key, name in (("IP4.ADDRESS", "IP"), ("IP4.GATEWAY", "gateway"),
                              ("IP4.DNS", "DNS"), ("GENERAL.HWADDR", "MAC")):
                if details.get(key):
                    parts.append(f"{name} {', '.join(details[key])}")
            widgets = []
            if state.startswith("connected"):
                widgets.append(self.button("Disconnect", self.act, ["nmcli", "device", "disconnect", dev]))
            elif state == "disconnected":
                widgets.append(self.button("Connect", self.act, ["nmcli", "device", "connect", dev],
                                           None, True, True))
            self.list_row(lb, bold(dev), "\n".join(parts), widgets, markup=True)

        self.section(box, "Saved connections")
        lb = self.listbox(box)
        active = set(run("nmcli", "-t", "-f", "UUID", "connection", "show", "--active").stdout.split())
        kinds = {"802-11-wireless": "Wi-Fi", "802-3-ethernet": "Ethernet", "vpn": "VPN",
                 "wireguard": "WireGuard", "bluetooth": "Bluetooth"}
        for line in run("nmcli", "-t", "-f", "NAME,UUID,TYPE,AUTOCONNECT",
                        "connection", "show").stdout.splitlines():
            name, uuid, kind, auto = (terse_split(line) + [""] * 4)[:4]
            if kind in ("loopback", "bridge", "tun"):
                continue
            is_active = uuid in active
            autoconnect = Gtk.Switch(active=auto == "yes", tooltip_text="Connect automatically")
            autoconnect.connect("notify::active", lambda s, _, u=uuid: self.act(
                ["nmcli", "connection", "modify", u, "connection.autoconnect",
                 "yes" if s.get_active() else "no"], refresh=False))
            toggle = (self.button("Disconnect", self.act, ["nmcli", "connection", "down", uuid])
                      if is_active else
                      self.button("Connect", self.act, ["nmcli", "connection", "up", uuid],
                                  None, True, True))
            widgets = [autoconnect, toggle,
                       self.button("Edit", self.edit_connection, uuid, name, kind),
                       self.button("Forget", self.forget_connection, uuid, name, danger=True)]
            subtitle = kinds.get(kind, kind) + (" · connected" if is_active else "")
            self.list_row(lb, name, subtitle, widgets)

        self.section(box, "Hidden network")
        row = Gtk.Box(spacing=8)
        ssid = Gtk.Entry(placeholder_text="Network name")
        password = Gtk.Entry(placeholder_text="Password", visibility=False)
        row.pack_start(ssid, False, False, 0)
        row.pack_start(password, False, False, 0)
        def connect_hidden():
            name, secret = ssid.get_text().strip(), password.get_text()
            if not name:
                return

            def work():
                commands = [f"set connection.id {name}", f"set wifi.ssid {name}",
                            "set wifi.hidden yes"]
                if secret:
                    commands += ["set wifi-sec.key-mgmt wpa-psk", f"set wifi-sec.psk {secret}"]
                made = nm_edit(["type", "wifi"], commands)
                if made.returncode != 0:
                    return made
                return run("nmcli", "connection", "up", "id", name, timeout=40)
            self.act(["nmcli", "connection", "add"], f"Connected to {name}.",
                     background=True, work=work)
        row.pack_start(self.button("Connect", connect_hidden), False, False, 0)
        box.pack_start(row, False, False, 0)

    def forget_connection(self, uuid, name):
        if self.confirm(f"Forget “{name}”?", "Its saved password and settings are deleted."):
            self.act(["nmcli", "connection", "delete", uuid], f"Forgot {name}.")

    def edit_connection(self, uuid, name, kind):
        fields = ["ipv4.method", "ipv4.addresses", "ipv4.gateway", "ipv4.dns", "ipv4.ignore-auto-dns"]
        values = run("nmcli", "-g", ",".join(fields), "connection", "show", uuid).stdout.splitlines()
        current = dict(zip(fields, [v.replace("\\:", ":") for v in values] + [""] * len(fields)))

        dialog = Gtk.Dialog(title=f"Edit {name}", transient_for=self, modal=True)
        dialog.add_buttons("Cancel", Gtk.ResponseType.CANCEL, "Apply", Gtk.ResponseType.OK)
        grid = Gtk.Grid(column_spacing=12, row_spacing=8, margin=16)
        dialog.get_content_area().add(grid)

        def add(row, text, widget):
            grid.attach(label(text), 0, row, 1, 1)
            grid.attach(widget, 1, row, 1, 1)

        method = Gtk.ComboBoxText()
        for key, text in (("auto", "Automatic (DHCP)"), ("manual", "Manual")):
            method.append(key, text)
        method.set_active_id("manual" if current["ipv4.method"] == "manual" else "auto")
        address = Gtk.Entry(text=current["ipv4.addresses"], placeholder_text="192.168.1.20/24")
        gateway = Gtk.Entry(text=current["ipv4.gateway"], placeholder_text="192.168.1.1")
        dns = Gtk.Entry(text=current["ipv4.dns"], placeholder_text="1.1.1.1,9.9.9.9 (blank: automatic)")
        only_mine = Gtk.Switch(active=current["ipv4.ignore-auto-dns"] == "yes", halign=Gtk.Align.START)
        add(0, "IPv4", method)
        add(1, "Address", address)
        add(2, "Gateway", gateway)
        add(3, "DNS servers", dns)
        add(4, "Only these DNS servers", only_mine)
        password = None
        if kind == "802-11-wireless":
            password = Gtk.Entry(visibility=False, placeholder_text="unchanged")
            add(5, "Password", password)

        def sync(*_):
            manual = method.get_active_id() == "manual"
            address.set_sensitive(manual)
            gateway.set_sensitive(manual)
        method.connect("changed", sync)
        sync()
        dialog.show_all()
        answer = dialog.run()
        if answer == Gtk.ResponseType.OK:
            cmd = ["nmcli", "connection", "modify", uuid, "ipv4.method", method.get_active_id()]
            if method.get_active_id() == "manual":
                cmd += ["ipv4.addresses", address.get_text(), "ipv4.gateway", gateway.get_text()]
            else:
                cmd += ["ipv4.addresses", "", "ipv4.gateway", ""]
            cmd += ["ipv4.dns", dns.get_text().replace(" ", ""),
                    "ipv4.ignore-auto-dns", "yes" if only_mine.get_active() else "no"]
            result = run(*cmd)
            if result.returncode == 0 and password is not None and password.get_text():
                # thru the editor so its never on a command line
                result = nm_edit([uuid], [f"set wifi-sec.psk {password.get_text()}"])
            if result.returncode != 0:
                self.say(result.stderr.strip() or "Couldn't change the connection.", error=True)
            else:
                # settings apply on the next connect so reconnect if its up rn
                up = uuid in run("nmcli", "-t", "-f", "UUID", "connection", "show", "--active").stdout
                if up:
                    self.act(["nmcli", "connection", "up", uuid], f"Saved {name} and reconnected.",
                             background=True)
                else:
                    self.say(f"Saved {name}.")
        dialog.destroy()

    # bluetooth

    def fill_bluetooth(self, box):
        show = run("bluetoothctl", "show", timeout=4)
        if show.returncode != 0 or "Controller" not in show.stdout:
            box.pack_start(label("No Bluetooth adapter found (is bluetooth.service running?)."),
                           False, False, 0)
            return
        info = dict(line.strip().split(": ", 1) for line in show.stdout.splitlines()[1:]
                    if ": " in line)
        powered = info.get("Powered") == "yes"
        power = Gtk.Switch(active=powered)
        power.connect("notify::active", lambda s, _: self.act(
            ["bluetoothctl", "power", "on" if s.get_active() else "off"]))
        self.section(box, "Bluetooth", power)
        box.pack_start(label(f"This computer appears as “{info.get('Alias', '?')}”.", dim=True),
                       False, False, 0)
        if not powered:
            return
        visible = Gtk.Switch(active=info.get("Discoverable") == "yes", halign=Gtk.Align.START)
        visible.connect("notify::active", lambda s, _: self.act(
            ["bluetoothctl", "discoverable", "on" if s.get_active() else "off"], refresh=False))
        row = Gtk.Box(spacing=10)
        row.pack_start(label("Visible to other devices"), False, False, 0)
        row.pack_start(visible, False, False, 0)
        box.pack_start(row, False, False, 0)

        self.section(box, "My devices")
        lb = self.listbox(box)
        paired = self.bt_devices("Paired")
        for mac, name in paired:
            details = {}
            for line in run("bluetoothctl", "info", mac, timeout=4).stdout.splitlines():
                key, _, value = line.strip().partition(": ")
                details[key] = value
            connected = details.get("Connected") == "yes"
            parts = ["connected" if connected else "not connected"]
            battery = details.get("Battery Percentage", "")
            if "(" in battery:
                parts.append(f"battery {battery.split('(')[1].rstrip(')')}%")
            parts.append(mac)
            trust = Gtk.Switch(active=details.get("Trusted") == "yes",
                               tooltip_text="Trusted: may connect without asking")
            trust.connect("notify::active", lambda s, _, m=mac: self.act(
                ["bluetoothctl", "trust" if s.get_active() else "untrust", m], refresh=False))
            toggle = (self.button("Disconnect", self.act, ["bluetoothctl", "disconnect", mac])
                      if connected else
                      self.button("Connect", self.act, ["bluetoothctl", "connect", mac],
                                  None, True, True))
            self.list_row(lb, name, " · ".join(parts), [
                trust, toggle, self.button("Remove", self.remove_device, mac, name, danger=True)])
        if not paired:
            self.list_row(lb, "No paired devices yet.")

        scan = self.button("Search", self.bt_scan)
        self.section(box, "Other devices", scan)
        self.bt_found = self.listbox(box)
        self.list_row(self.bt_found, "Put the device in pairing mode, then Search.")

    def bt_devices(self, *filters):
        devices = []
        for line in run("bluetoothctl", "devices", *filters, timeout=4).stdout.splitlines():
            parts = line.split(" ", 2)
            if len(parts) == 3 and parts[0] == "Device":
                devices.append((parts[1], parts[2]))
        return devices

    def remove_device(self, mac, name):
        if self.confirm(f"Remove “{name}”?", "You'll need to pair it again to use it."):
            self.act(["bluetoothctl", "remove", mac], f"Removed {name}.")

    def bt_scan(self):
        found = self.bt_found
        clear(found)
        self.list_row(found, "Searching…")
        found.show_all()

        def done(_result):
            if found.get_parent() is None:
                return
            clear(found)
            paired = {mac for mac, _ in self.bt_devices("Paired")}
            others = [(m, n) for m, n in self.bt_devices() if m not in paired]
            for mac, name in others:
                # devices without a name show their address so list them last
                self.list_row(found, name, mac if name.replace("-", ":") != mac else None,
                              [self.button("Pair", self.pair_device, mac, name)])
            if not others:
                self.list_row(found, "Nothing found. Is the device in pairing mode?")
            found.show_all()
        in_background(lambda: run("bluetoothctl", "--timeout", "10", "scan", "on", timeout=15), done)

    def pair_device(self, mac, name):
        self.say(f"Pairing with {name}…")

        def work():
            for cmd in (["pair", mac], ["trust", mac], ["connect", mac]):
                result = run("bluetoothctl", "--timeout", "20", *cmd, timeout=25)
                if result.returncode != 0 or "Failed" in result.stdout:
                    return result
            return result

        def done(result):
            if result.returncode != 0 or "Failed" in result.stdout:
                self.say(f"Couldn't pair with {name}.", error=True)
            else:
                self.say(f"Paired with {name}.")
            self.refresh_system_page("bluetooth")
        in_background(work, done)

    # sound

    def pactl_json(self, *what):
        try:
            return json.loads(run("pactl", "-f", "json", "list", *what).stdout or "[]")
        except json.JSONDecodeError:
            return []

    def fill_sound(self, box):
        sinks = self.pactl_json("sinks")
        sources = [s for s in self.pactl_json("sources") if not s["name"].endswith(".monitor")]
        if not sinks and not sources:
            box.pack_start(label("No sound server found (pipewire-pulse / pactl)."), False, False, 0)
            return
        pavucontrol = self.button("Open pavucontrol",
                                  lambda: subprocess.Popen(["pavucontrol"], start_new_session=True))
        self.section(box, "Output", pavucontrol)
        self.device_list(box, sinks, run("pactl", "get-default-sink").stdout.strip(), "sink")
        self.section(box, "Input")
        self.device_list(box, sources, run("pactl", "get-default-source").stdout.strip(), "source")

        streams = self.pactl_json("sink-inputs")
        self.section(box, "Applications")
        lb = self.listbox(box)
        for stream in streams:
            props = stream.get("properties", {})
            app = props.get("application.name") or props.get("application.process.binary") or "?"
            media = props.get("media.name", "")
            move = Gtk.ComboBoxText(tooltip_text="Play through")
            for sink in sinks:
                move.append(str(sink["index"]), sink["description"])
            move.set_active_id(str(stream["sink"]))
            move.connect("changed", lambda c, i=stream["index"]: self.act(
                ["pactl", "move-sink-input", str(i), c.get_active_id()], refresh=False))
            widgets = self.volume_widgets("sink-input", str(stream["index"]), stream) + [move]
            self.list_row(lb, app, media if media != app else None, widgets)
        if not streams:
            self.list_row(lb, "Nothing is playing.")

        self.section(box, "Device profiles")
        lb = self.listbox(box)
        for card in self.pactl_json("cards"):
            profiles = card.get("profiles", {})
            combo = Gtk.ComboBoxText()
            for key, profile in profiles.items():
                if profile.get("available", True) or key == card.get("active_profile"):
                    combo.append(key, profile.get("description", key))
            combo.set_active_id(card.get("active_profile"))
            combo.connect("changed", lambda c, n=card["name"]: self.act(
                ["pactl", "set-card-profile", n, c.get_active_id()]))
            name = card.get("properties", {}).get("device.description", card["name"])
            self.list_row(lb, name, None, [combo])

    def device_list(self, box, devices, default, kind):
        lb = self.listbox(box)
        group = None
        for dev in devices:
            radio = Gtk.RadioButton(group=group, tooltip_text="Use this one")
            group = group or radio
            radio.set_active(dev["name"] == default)
            radio.connect("toggled", lambda r, n=dev["name"]: r.get_active() and self.act(
                ["pactl", f"set-default-{kind}", n], refresh=False))
            widgets = [radio] + self.volume_widgets(kind, dev["name"], dev)
            ports = [p for p in dev.get("ports", []) if p.get("availability") != "not available"]
            if len(ports) > 1:
                port = Gtk.ComboBoxText()
                for p in ports:
                    port.append(p["name"], p["description"])
                port.set_active_id(dev.get("active_port"))
                port.connect("changed", lambda c, n=dev["name"]: self.act(
                    ["pactl", f"set-{kind}-port", n, c.get_active_id()], refresh=False))
                widgets.append(port)
            row = self.list_row(lb, dev["description"], None, widgets[1:])
            row.pack_start(radio, False, False, 0)
            row.reorder_child(radio, 0)

    def volume_widgets(self, kind, target, info):
        levels = [int(ch["value_percent"].rstrip("%")) for ch in info.get("volume", {}).values()]
        volume = max(levels) if levels else 0
        scale = Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, 150, 1)
        scale.set_value(volume)
        scale.set_size_request(200, -1)
        scale.set_value_pos(Gtk.PositionType.RIGHT)
        scale.add_mark(100, Gtk.PositionType.BOTTOM, None)
        scale.connect("value-changed", lambda s: run(
            "pactl", f"set-{kind}-volume", target, f"{int(s.get_value())}%"))
        mute = Gtk.ToggleButton(label="Mute", active=bool(info.get("mute")))
        mute.connect("toggled", lambda b: run(
            "pactl", f"set-{kind}-mute", target, "1" if b.get_active() else "0"))
        return [scale, mute]

    # power

    def fill_power(self, box):
        current = run("powerprofilesctl", "get").stdout.strip()
        if current:
            self.section(box, "Power mode")
            row = Gtk.Box(spacing=8)
            group = None
            for key, text in (("power-saver", "Power saver"), ("balanced", "Balanced"),
                              ("performance", "Performance")):
                if key not in run("powerprofilesctl", "list").stdout:
                    continue
                radio = Gtk.RadioButton(label=text, group=group)
                group = group or radio
                radio.set_active(key == current)
                radio.connect("toggled", lambda r, k=key: r.get_active() and self.act(
                    ["powerprofilesctl", "set", k], refresh=False))
                row.pack_start(radio, False, False, 0)
            box.pack_start(row, False, False, 0)

        self.section(box, "Idle and lock")
        # the timeouts live w the lock screen settings if sevenshell is installed or on general if not
        page, title = (("lock", "Lock & idle") if self.has_shell() else ("general", "General"))
        link = Gtk.LinkButton(label=f"Screen-off, lock and sleep timeouts are on the {title} page",
                              uri=f"settings:{page}", halign=Gtk.Align.START)
        link.connect("activate-link", lambda _l: (self.stack.set_visible_child_name(page), True)[1])
        box.pack_start(link, False, False, 0)

        self.section(box, "Session")
        row = Gtk.Box(spacing=8)
        actions = [
            ("Lock", None, lambda: subprocess.Popen(["sevenshell", "lock"], start_new_session=True)),
            ("Suspend", None, lambda: run("systemctl", "suspend")),
            ("Sign out", "Sign out of sevenwm? Unsaved work in open windows is lost.",
             lambda: sevenwm_action("quit") or self.say("sevenwm isn't running here.", error=True)),
            ("Restart", "Restart the computer?", lambda: run("systemctl", "reboot")),
            ("Shut down", "Shut down the computer?", lambda: run("systemctl", "poweroff")),
        ]
        for text, question, action in actions:
            def clicked(_b, question=question, action=action):
                if question is None or self.confirm(question):
                    action()
            btn = Gtk.Button(label=text)
            if question:
                btn.get_style_context().add_class("destructive-action")
            btn.connect("clicked", clicked)
            row.pack_start(btn, False, False, 0)
        box.pack_start(row, False, False, 0)
