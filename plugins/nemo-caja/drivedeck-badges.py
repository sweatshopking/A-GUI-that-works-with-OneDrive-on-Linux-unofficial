# Sync badges for Nemo and Caja (DriveDeck). The same file works in both: Nemo loads it
# from nemo-python/extensions, Caja from caja-python/extensions. It asks the app's tray, over the
# session bus, what state each file is in (docs/badges-design.md); no tray running = no badges.
import gi
from gi.repository import GObject, Gio, GLib

try:
    gi.require_version("Nemo", "3.0")
    from gi.repository import Nemo as FM
except (ValueError, ImportError):
    gi.require_version("Caja", "2.0")
    from gi.repository import Caja as FM

BUS = "io.github.sweatshopking.DriveDeck.Badges"
PATH = "/io/github/sweatshopking/DriveDeck/Badges"
IFACE = BUS
EMBLEMS = {
    "synced": "emblem-drivedeck-synced",
    "syncing": "emblem-drivedeck-syncing",
    "error": "emblem-drivedeck-error",
    "paused": "emblem-drivedeck-paused",
}


class Badges(GObject.GObject, FM.InfoProvider):
    def __init__(self):
        super().__init__()
        self.shown = {}  # uri -> (file object, path), bounded to the most recently seen files
        self.states = {}  # uri -> (path, state), refreshed together when the tray changes
        self.refreshing = False
        self.refresh_again = False
        try:
            self.bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
            self.bus.signal_subscribe(BUS, IFACE, "Changed", PATH, None,
                                      Gio.DBusSignalFlags.NONE, self.changed)
            self.bus.signal_subscribe("org.freedesktop.DBus", "org.freedesktop.DBus",
                                      "NameOwnerChanged", "/org/freedesktop/DBus", BUS,
                                      Gio.DBusSignalFlags.NONE, self.owner_changed)
        except GLib.Error:
            self.bus = None

    def ask(self, paths):
        if self.bus is None:
            return [""] * len(paths)
        try:
            reply = self.bus.call_sync(BUS, PATH, IFACE, "GetStatus",
                                       GLib.Variant("(as)", (paths,)), GLib.VariantType("(as)"),
                                       Gio.DBusCallFlags.NO_AUTO_START, 300, None)
            return reply.unpack()[0]
        except GLib.Error:
            return [""] * len(paths)

    def update_file_info(self, file):
        location = file.get_location()
        path = location.get_path() if location else None
        if not path:
            return FM.OperationResult.COMPLETE
        uri = file.get_uri()
        cached = self.states.get(uri)
        state = cached[1] if cached and cached[0] == path else self.ask([path])[0]
        self.shown[uri] = (file, path)
        self.states[uri] = (path, state)
        if len(self.shown) > 5000:  # forget the oldest; they're refreshed when shown again
            oldest = next(iter(self.shown))
            del self.shown[oldest]
            self.states.pop(oldest, None)
        emblem = EMBLEMS.get(state)
        if emblem:
            file.add_emblem(emblem)
        return FM.OperationResult.COMPLETE

    def changed(self, *_args):
        self.refresh_shown()

    def owner_changed(self, _bus, _sender, _path, _iface, _signal, parameters):
        name, _old_owner, new_owner = parameters.unpack()
        if name != BUS:
            return
        if new_owner:
            self.refresh_shown()
        else:
            self.apply_states(list(self.shown.items()), [""] * len(self.shown))

    def refresh_shown(self):
        if self.refreshing:
            self.refresh_again = True
            return
        entries = list(self.shown.items())
        if not entries:
            return
        if self.bus is None:
            self.apply_states(entries, [""] * len(entries))
            return
        self.refreshing = True
        paths = [path for _uri, (_file, path) in entries]
        try:
            self.bus.call(BUS, PATH, IFACE, "GetStatus",
                          GLib.Variant("(as)", (paths,)), GLib.VariantType("(as)"),
                          Gio.DBusCallFlags.NO_AUTO_START, 300, None,
                          self.refresh_done, entries)
        except GLib.Error:
            self.refreshing = False
            self.apply_states(entries, [""] * len(entries))

    def refresh_done(self, connection, result, entries):
        try:
            states = connection.call_finish(result).unpack()[0]
            if len(states) != len(entries):
                states = [""] * len(entries)
        except GLib.Error:
            states = [""] * len(entries)
        self.refreshing = False
        self.apply_states(entries, states)
        if self.refresh_again:
            self.refresh_again = False
            self.refresh_shown()

    def apply_states(self, entries, states):
        for (uri, (file, path)), state in zip(entries, states):
            current = (path, state)
            if self.states.get(uri) != current:
                self.states[uri] = current
                file.invalidate_extension_info()
