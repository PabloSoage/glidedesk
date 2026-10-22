"""Regenerates the icons from assets/logo/glidedesk.svg.

Needs rsvg-convert and Pillow. Writes:
  assets/logo/glidedesk.ico        Windows executable icon (16-256 px)
  assets/logo/glidedesk-64.rgba    64x64 RGBA pixels for window icons (winit, X11)
  assets/logo/glidedesk-512.png    for READMEs and stores

The Android adaptive icon is vector XML (android/app/src/main/res) and repeats the
SVG's shapes by hand.
"""

import io
import pathlib
import subprocess

from PIL import Image

ROOT = pathlib.Path(__file__).resolve().parent.parent
LOGO = ROOT / "assets" / "logo"
SVG = LOGO / "glidedesk.svg"


def render(size: int) -> Image.Image:
    png = subprocess.run(
        ["rsvg-convert", "-w", str(size), "-h", str(size), str(SVG)],
        check=True,
        capture_output=True,
    ).stdout
    return Image.open(io.BytesIO(png)).convert("RGBA")


def main() -> None:
    sizes = [16, 24, 32, 48, 64, 128, 256]
    images = [render(s) for s in sizes]
    images[-1].save(LOGO / "glidedesk.ico", sizes=[(s, s) for s in sizes], append_images=images[:-1])
    (LOGO / "glidedesk-64.rgba").write_bytes(render(64).tobytes())
    render(512).save(LOGO / "glidedesk-512.png", optimize=True)


if __name__ == "__main__":
    main()
