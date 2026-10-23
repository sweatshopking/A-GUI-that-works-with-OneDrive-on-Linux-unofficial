# Sync badges for Nemo and Caja (Linux OneDrive GUI). The same file works in both: Nemo loads it
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

BUS = "io.github.sweatshopking.LinuxOnedriveGui.Badges"
PATH = "/io/github/sweatshopking/LinuxOnedriveGui/Badges"
IFACE = BUS
EMBLEMS = {
    "synced": "emblem-linux-onedrive-gui-synced",
    "syncing": "emblem-linux-onedrive-gui-syncing",
    "error": "emblem-linux-onedrive-gui-error",
    "paused": "emblem-linux-onedrive-gui-paused",
}


class Badges(GObject.GObject, FM.InfoProvider):
    def __init__(self):
        super().__init__()
        self.shown = {}  # uri -> file object we gave a badge (or asked about), to refresh later
        try:
            self.bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
            self.bus.signal_subscribe(BUS, IFACE, "Changed", PATH, None,
                                      Gio.DBusSignalFlags.NONE, self.changed)
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
        state = self.ask([path])[0]
        self.shown[file.get_uri()] = file
        emblem = EMBLEMS.get(state)
        if emblem:
            file.add_emblem(emblem)
        return FM.OperationResult.COMPLETE

    def changed(self, *_args):
        # ask again for every file we've looked at; the file manager calls update_file_info
        shown, self.shown = self.shown, {}
        for f in shown.values():
            f.invalidate_extension_info()
