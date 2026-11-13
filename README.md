# Still

A quiet photo editor for Linux and Windows, written in Rust (egui).
Tone, colour, film texture and a few restrained glitches. No more than that.

## Download

Grab the latest build from **Releases**:

- **Linux:** `Still-x86_64.AppImage`. Run `chmod +x Still-x86_64.AppImage`, then run the file.
- **Windows:** `Still-windows-x64.exe`. The app isn't code-signed, so SmartScreen may ask: choose *More info → Run anyway*.

Releases are built by GitHub Actions (`.github/workflows/release.yml`) whenever a `v*` tag is pushed.

## Build from source

```sh
cargo run --release                 # open the app
cargo run --release -- photo.jpg    # open a photo directly
./install.sh                        # install to ~/.local/bin + app launcher
```

Headless batch rendering with a look:

```sh
still --render "Overcast" in.RW2 out.tif
```

## What it does

- **Opens** JPEG, PNG, TIFF (8/16-bit), WebP, EXR, and camera RAW (RW2, CR2/CR3, NEF, ARW, DNG, RAF, ORF…)
- **Presets:** fifteen built-in presets with live thumbnails (including Pathologic, Deus Ex, MGS4 and Half-Life 2 “City 17” game looks), plus your own saved presets
- **Adjust:** exposure, contrast, highlights/shadows, whites/blacks, fade, white balance, saturation/vibrance, B&W, split toning, sharpen/soften
- **Masks:** local adjustments through **Object** selections (click a subject and it grows to its edges, with a threshold; place + points to add to it and − points where it should not go, and click a point to delete it), linear gradients, radial (elliptical) masks, a brush with feather, flow and erase, and luminance or colour ranges. You can refine any shape by a luminance or colour range, picking the colour from the photo. Each mask has its own exposure, contrast, highlights, shadows, temperature, tint, saturation, sharpness and colour wash, plus invert, amount, a visibility toggle and a red overlay that stays up while you shape the mask and hides once you adjust. **Make Transparent** cuts a mask's area out of the picture (select a subject, invert, and the background is gone, or put a layer **Behind Photo** to replace it); PNG, TIFF and WebP exports keep the transparency. Masks stay attached to the picture when you re-crop, and they render the same in the preview and the export
- **Projects:** **Save** (Ctrl+S) stores the photo and the whole edit (look, crop, masks, retouching, layers) in a `.still` file. Open it again later with **Open**, by dropping it on the window, or with `still edit.still`. If you move a folder that holds the project together with its photos, it still opens
- **Layers:** place other photos on the canvas, PNG transparency included. Move, scale and rotate them directly on the photo (Shift snaps rotation). Side handles stretch a layer, and **Stretch** fills the whole canvas. Edges snap to the canvas sides and centre (hold Ctrl to place freely). Arrow keys nudge a layer, and dragging layers in the list reorders them. Each layer has opacity, nine blend modes (Normal, Multiply, Screen, Overlay, Soft Light, Darken, Lighten, Difference, Add), flip, Fit/Fill, ordering and visibility. A layer either takes on the look with the photo or sits on top of it, which suits light leaks and textures. Masks only look at and cut your photo, never the layers, and **Behind Photo** puts a layer underneath the picture so it shows through whatever **Make Transparent** cut out (a new sky behind a subject, for example). Add layers with **Add Photo…** or drop files on the window while the Layers tab is open
- **Retouch:** a clone brush for painting out stray objects. Ctrl+click where to copy from, then paint; the source follows the brush at the same distance
- **Effects:** bloom, halation, astigmatism (lights smear into streaks, crosses or stars), chromatic aberration, vignette, film grain, CCD noise, scanlines, posterize with ordered dither, downsample (blocky or soft low resolution). **Apply To** limits them to one mask
- **Glitch:** pixel sort, data streaks, slice shift, RGB split, block compression. Each one is seeded, so **Shuffle** gives a new arrangement you can reproduce. **Apply To** limits them to one mask
- **Crop:** drag the crop rectangle on the photo; Free, Original, Square, 5:4, 3:2, 4:3, 16:9, 2.39:1 and XPan presets in landscape or portrait; straighten (±45°), a horizon line you lay along the horizon (or any angle) and **Level Photo** to match, rotate, flip, border; a rule-of-thirds grid while cropping, and thirds or golden-ratio guides in every tab
- **Export:** JPEG (quality), PNG/TIFF at 8 or 16 bits, WebP lossless; resize by long edge or percent (Lanczos); output sharpening; keeps EXIF metadata and the embedded colour profile; 8-bit output is dithered so gradients don't band; writes atomically

Effect sizes scale with image resolution, so the preview matches the full-size export.

## Sharing presets

Right-click any preset and choose **Export…**, or use **Share Presets → Export My Presets…**, to save a `.stilllook` file. Friends can **Import…** it, drop it onto the window, or open it with `still look.stilllook`. Only the settings are shared: never the photo, the crop, the masks or the retouching. The command line accepts look files too:

```sh
still --render friend.stilllook in.jpg out.jpg
```

## Settings

**Settings** (Ctrl+,) covers:

- preview quality (1200 / 1800 / 2600 px)
- canvas colour
- mask overlay colour
- histogram mode
- render time
- Reduce Motion
- guides
- whether a new photo keeps the current look
- importing and exporting presets

## Keys

| | |
|---|---|
| Ctrl+O | Open |
| Ctrl+S / Ctrl+Shift+S | Save project / Save As |
| Ctrl+E | Export |
| Ctrl+, | Settings |
| Ctrl+Z / Ctrl+Shift+Z | Undo / Redo |
| `\` (hold) | Compare with the original |
| 1–8 | Switch sidebar tab |
| O (hold) | Show the selected mask as a red overlay |
| Alt + paint | Erase with the brush |
| Alt + click | Place the other kind of point (+ or −) in an Object selection |
| Ctrl + click | Choose the clone source (Retouch) |
| [ / ] | Brush size |
| Space + drag | Pan while a mask is selected |
| Delete | Delete the selected mask or layer |
| Arrow keys (+Shift) | Nudge the selected layer |
| Ctrl+0 / double-click | Fit / zoom |
| Double-click a slider | Reset it |
| Shift + drag a slider | Fine adjustment |

Settings and saved presets are stored in `~/.config/still/settings.json` (Windows: `%APPDATA%\Still\settings.json`).

The interface font is Adwaita Sans, bundled under the SIL Open Font License (`assets/fonts/OFL.txt`).
