# Fake badge service for tests: answers by file name (synced/syncing/error/paused in the name).
from gi.repository import Gio, GLib
XML = """<node><interface name="io.github.sweatshopking.DriveDeck.Badges">
<method name="GetStatus"><arg type="as" direction="in"/><arg type="as" direction="out"/></method>
<signal name="Changed"><arg type="as"/></signal></interface></node>"""
def state(p):
    if "/OneDrive" not in p: return ""
    for s in ("syncing", "error", "paused"):
        if s in p.rsplit("/", 1)[-1]: return s
    return "synced"
def call(conn, sender, path, iface, method, params, inv):
    inv.return_value(GLib.Variant("(as)", ([state(p) for p in params.unpack()[0]],)))
def got_bus(conn, name):
    conn.register_object("/io/github/sweatshopking/DriveDeck/Badges",
                         Gio.DBusNodeInfo.new_for_xml(XML).interfaces[0], call, None, None)
Gio.bus_own_name(Gio.BusType.SESSION, "io.github.sweatshopking.DriveDeck.Badges",
                 Gio.BusNameOwnerFlags.NONE, got_bus, None, None)
GLib.MainLoop().run()
