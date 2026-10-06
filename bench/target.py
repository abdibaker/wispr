#!/usr/bin/env python3
"""Controlled insertion target: `target.py gtk|vte OUT`. Writes what it received to OUT."""
import sys, gi
gi.require_version("Gtk", "3.0")
from gi.repository import Gtk, GLib

kind, out = sys.argv[1], sys.argv[2]
window = Gtk.Window(title=f"vp-target-{kind}")
window.set_default_size(700, 400)
if kind == "gtk":
    view = Gtk.TextView()
    window.add(view)
    def text():
        b = view.get_buffer()
        return b.get_text(b.get_start_iter(), b.get_end_iter(), True)
else:
    gi.require_version("Vte", "2.91")
    from gi.repository import Vte
    view = Vte.Terminal()
    # Raw terminal: what the shell receives on stdin, byte for byte.
    view.spawn_async(Vte.PtyFlags.DEFAULT, None, ["/bin/sh", "-c", f"stty raw -echo; cat > {out}.raw"],
                     None, GLib.SpawnFlags.DEFAULT, None, None, -1, None, None, None)
    window.add(view)
    text = lambda: ""
def dump():
    with open(out, "w") as f:
        f.write(text())
    return True
GLib.timeout_add(200, dump)
window.connect("destroy", Gtk.main_quit)
window.show_all()
window.present()
view.grab_focus()
Gtk.main()
