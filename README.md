# Still

A quiet photo editor for Linux, written in Rust (egui).
Tone, colour, film texture and a few restrained glitches. No more than that.

## Run

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
- **Looks:** fourteen built-in looks with live thumbnails (including Pathologic, Deus Ex and MGS4 game looks), plus your own saved looks
- **Adjust:** exposure, contrast, highlights/shadows, whites/blacks, fade, white balance, saturation/vibrance, B&W, split toning, sharpen/soften
- **Masks:** local adjustments through **Object** selections (click a subject and it grows to its edges, with a threshold; click more spots to add or Alt+click to remove), linear gradients, radial (elliptical) masks, a brush with feather, flow and erase, and luminance or colour ranges. You can refine any shape by a luminance or colour range, picking the colour from the photo. Each mask has its own exposure, contrast, highlights, shadows, temperature, tint, saturation, sharpness and colour wash, plus invert, amount, a red overlay view and a visibility toggle. Masks stay attached to the picture when you re-crop, and they render the same in the preview and the export
- **Effects:** bloom, halation, chromatic aberration, vignette, film grain, CCD noise, scanlines, posterize with ordered dither, downsample (blocky or soft low resolution)
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
- importing and exporting looks

## Keys

| | |
|---|---|
| Ctrl+O | Open |
| Ctrl+E | Export |
| Ctrl+, | Settings |
| Ctrl+Z / Ctrl+Shift+Z | Undo / Redo |
| `\` (hold) | Compare with the original |
| 1–6 | Switch sidebar tab |
| O (hold) | Show the selected mask as a red overlay |
| Alt + paint | Erase with the brush |
| Alt + click | Remove an area from an Object selection |
| [ / ] | Brush size |
| Space + drag | Pan while a mask is selected |
| Delete | Delete the selected mask |
| Ctrl+0 / double-click | Fit / zoom |
| Double-click a slider | Reset it |
| Shift + drag a slider | Fine adjustment |

Settings and saved looks are stored in `~/.config/still/settings.json`.
