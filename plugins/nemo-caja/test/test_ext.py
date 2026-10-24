# Loads the Nemo/Caja extension with a stand-in file object and checks the emblem it adds.
import importlib.util, sys
from gi.repository import Gio
spec = importlib.util.spec_from_file_location("ext", sys.argv[1])
ext = importlib.util.module_from_spec(spec); spec.loader.exec_module(ext)

class F:
    def __init__(self, p): self.p, self.emblems = p, []
    def get_location(self): return Gio.File.new_for_path(self.p)
    def get_uri(self): return "file://" + self.p
    def add_emblem(self, e): self.emblems.append(e)
    def invalidate_extension_info(self): pass

b = ext.Badges(); bad = 0
for p, want in [("/t/OneDrive/a.txt", ["emblem-drivedeck-synced"]),
                ("/t/OneDrive/r (error).xlsx", ["emblem-drivedeck-error"]),
                ("/t/elsewhere/x", [])]:
    f = F(p); b.update_file_info(f); ok = f.emblems == want; bad += not ok
    print("ok  " if ok else "FAIL", p, f.emblems)
sys.exit(bad)
