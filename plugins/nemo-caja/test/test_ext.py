# Loads the Nemo/Caja extension with a stand-in file object and checks the emblem it adds.
import importlib.util, sys
from gi.repository import Gio, GLib
spec = importlib.util.spec_from_file_location("ext", sys.argv[1])
ext = importlib.util.module_from_spec(spec); spec.loader.exec_module(ext)

class F:
    def __init__(self, p): self.p, self.emblems = p, []
    def get_location(self): return Gio.File.new_for_path(self.p)
    def get_uri(self): return "file://" + self.p
    def add_emblem(self, e): self.emblems.append(e)
    def invalidate_extension_info(self): self.invalidations = getattr(self, "invalidations", 0) + 1

b = ext.Badges(); bad = 0
for p, want in [("/t/OneDrive/a.txt", ["emblem-drivedeck-synced"]),
                ("/t/OneDrive/r (error).xlsx", ["emblem-drivedeck-error"]),
                ("/t/elsewhere/x", [])]:
    f = F(p); b.update_file_info(f); ok = f.emblems == want; bad += not ok
    print("ok  " if ok else "FAIL", p, f.emblems)

class Bus:
    def __init__(self): self.calls, self.changed = 0, {}
    def call(self, _name, _path, _iface, _method, params, _reply, _flags,
             _timeout, _cancel, callback, user_data):
        self.calls += 1
        paths = params.unpack()[0]
        result = GLib.Variant("(as)", ([self.changed.get(p, ext_state(p)) for p in paths],))
        callback(self, result, user_data)
    def call_finish(self, result): return result

def ext_state(p):
    if "/OneDrive" not in p: return ""
    for s in ("syncing", "error", "paused"):
        if s in p.rsplit("/", 1)[-1]: return s
    return "synced"

# Repeated queries use the cached answer. A Changed signal asks once for all shown paths and
# invalidates only the file whose answer changed.
bus = Bus(); b.bus = bus
f = next(f for f in [b.shown[u][0] for u in b.shown] if f.p.endswith("/a.txt"))
b.update_file_info(f)
bus.changed[f.p] = "error"
b.changed()
ok = bus.calls == 1 and f.invalidations == 1
bad += not ok
print("ok  " if ok else "FAIL", "Changed uses one batch call")
sys.exit(bad)
