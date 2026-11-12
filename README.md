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
- **Looks:** fifteen built-in looks with live thumbnails (including Pathologic, Deus Ex, MGS4 and Half-Life 2 “City 17” game looks), plus your own saved looks
- **Adjust:** exposure, contrast, highlights/shadows, whites/blacks, fade, white balance, saturation/vibrance, B&W, split toning, sharpen/soften
- **Masks:** local adjustments through **Object** selections (click a subject and it grows to its edges, with a threshold; click more spots to add or Alt+click to remove), linear gradients, radial (elliptical) masks, a brush with feather, flow and erase, and luminance or colour ranges. You can refine any shape by a luminance or colour range, picking the colour from the photo. Each mask has its own exposure, contrast, highlights, shadows, temperature, tint, saturation, sharpness and colour wash, plus invert, amount, a red overlay view and a visibility toggle. Double-click a mask to rename it; right-click → **Change Color** gives it a label colour, which its overlay then uses. Masks stay attached to the picture when you re-crop, and they render the same in the preview and the export
- **Projects:** **Save** (Ctrl+S) stores the photo and the whole edit (look, crop, masks, layers) in a `.still` file. Open it again later with **Open**, by dropping it on the window, or with `still edit.still`. If you move a folder that holds the project together with its photos, it still opens
- **Layers:** place other photos on the canvas, PNG transparency included. Move, scale and rotate them directly on the photo (Shift snaps rotation). Side handles stretch a layer, and **Stretch** fills the whole canvas. Edges snap to the canvas sides and centre (hold Ctrl to place freely). Arrow keys nudge a layer, and dragging layers in the list reorders them. Each layer has opacity, nine blend modes (Normal, Multiply, Screen, Overlay, Soft Light, Darken, Lighten, Difference, Add), flip, Fit/Fill, ordering and visibility. A layer either takes on the look with the photo or sits on top of it, which suits light leaks and textures. Add layers with **Add Photo…** or by dropping photos on the window while a photo is open. Double-click a layer to rename it, or right-click → **Change Color** to label it
- **Effects:** bloom, halation, astigmatism (lights smear into streaks, crosses or stars), chromatic aberration, vignette, film grain, CCD noise, scanlines, posterize with ordered dither, downsample (blocky or soft low resolution)
- **Glitch:** pixel sort, data streaks, slice shift, RGB split, block compression. Each one is seeded, so **Shuffle** gives a new arrangement you can reproduce
- **Crop:** drag the crop rectangle on the photo; Free, Original, Square, 5:4, 3:2, 4:3, 16:9, 2.39:1 and XPan presets in landscape or portrait; straighten, rotate, flip, border; rule-of-thirds and golden-ratio guides
- **Export:** JPEG (quality), PNG/TIFF at 8 or 16 bits, WebP lossless; resize by long edge or percent (Lanczos); output sharpening; keeps EXIF metadata and the embedded colour profile; 8-bit output is dithered so gradients don't band; writes atomically

Effect sizes scale with image resolution, so the preview matches the full-size export.

## Sharing looks

Right-click any look and choose **Export…**, or use **Share Looks → Export My Looks…**, to save a `.stilllook` file. Friends can **Import…** it, drop it onto the window, or open it with `still look.stilllook`. Only the look is shared: never the photo, the crop or the masks. The command line accepts look files too:

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
- Linux: native Wayland or XWayland (the default, needed for drag and drop)
- importing and exporting looks

## Keys

| | |
|---|---|
| Ctrl+O | Open |
| Ctrl+S / Ctrl+Shift+S | Save project / Save As |
| Ctrl+E | Export |
| Ctrl+, | Settings |
| Ctrl+Z / Ctrl+Shift+Z | Undo / Redo |
| `\` (hold) | Compare with the original |
| 1–7 | Switch sidebar tab |
| O (hold) | Show the selected mask as a red overlay |
| Alt + paint | Erase with the brush |
| Alt + click | Remove an area from an Object selection |
| [ / ] | Brush size |
| Space + drag | Pan while a mask is selected |
| Delete | Delete the selected mask or layer |
| Arrow keys (+Shift) | Nudge the selected layer |
| Ctrl+0 / double-click | Fit / zoom |
| Double-click a slider | Reset it |
| Shift + drag a slider | Fine adjustment |

Settings and saved looks are stored in `~/.config/still/settings.json` (Windows: `%APPDATA%\Still\settings.json`).

The interface font is Adwaita Sans, bundled under the SIL Open Font License (`assets/fonts/OFL.txt`).
