#!/usr/bin/env python3
"""Test app for glidedesk measurements (GTK 4, runs in the remote session).

Full-screen window, drawn with GTK render nodes (composited on the GPU, so the app itself
is never the bottleneck), with two parts:

- A band that moves every frame (driven by the compositor's frame clock), so the stream
  always has motion: used to measure delivered frames per second.
- A square in the top-left corner that turns white while a mouse button is held and black
  otherwise: a client-side probe clicks there and times the colour change (input-to-photon).

  python3 tools/flash.py [--static]   # --static: no moving band (idle desktop)
"""

import sys

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
gi.require_version("Graphene", "1.0")
from gi.repository import Gdk, GLib, Graphene, Gtk  # noqa: E402

SQUARE = 160


def rgba(r, g, b):
    c = Gdk.RGBA()
    c.red, c.green, c.blue, c.alpha = r, g, b, 1.0
    return c


BACKGROUND = rgba(0.15, 0.15, 0.18)
BAND = rgba(0.2, 0.6, 1.0)
WHITE = rgba(1, 1, 1)
BLACK = rgba(0, 0, 0)


class Canvas(Gtk.Widget):
    def __init__(self, moving):
        super().__init__()
        self.pressed = False
        self.moving = moving
        click = Gtk.GestureClick(button=0)
        click.connect("pressed", self.on_press)
        click.connect("released", self.on_release)
        self.add_controller(click)
        if moving:
            self.add_tick_callback(lambda w, clock: (w.queue_draw(), GLib.SOURCE_CONTINUE)[1])

    def on_press(self, *_):
        self.pressed = True
        self.queue_draw()

    def on_release(self, *_):
        self.pressed = False
        self.queue_draw()

    def do_snapshot(self, snapshot):
        w, h = self.get_width(), self.get_height()
        snapshot.append_color(BACKGROUND, Graphene.Rect().init(0, 0, w, h))
        if self.moving:
            clock = self.get_frame_clock()
            t = clock.get_frame_time() / 1e6 if clock else 0.0
            x = (t * 600) % (w + 200) - 200
            snapshot.append_color(BAND, Graphene.Rect().init(x, h * 0.4, 200, h * 0.2))
        snapshot.append_color(WHITE if self.pressed else BLACK, Graphene.Rect().init(0, 0, SQUARE, SQUARE))


def main():
    moving = "--static" not in sys.argv
    app = Gtk.Application(application_id="io.github.pablosoage.glidedesk.Flash")

    def activate(a):
        win = Gtk.ApplicationWindow(application=a, title="glidedesk-flash")
        win.set_child(Canvas(moving))
        win.fullscreen()
        win.present()

    app.connect("activate", activate)
    app.run([])


if __name__ == "__main__":
    main()
