# snapcrop

`snapcrop` captures the Wayland desktop first and then lets you crop the frozen
image. The crop is written to a PNG file and copied to the clipboard.

It is deliberately small: there is no GUI toolkit, GPU renderer, async runtime,
or dependency on `grim`, `slurp`, `wayshot`, or `wl-copy` at runtime.

## Requirements

A wlroots-based compositor exposing these protocols:

- `zwlr_screencopy_manager_v1`
- `zwlr_layer_shell_v1`
- `zwlr_data_control_manager_v1`

Sway, Hyprland, River, and other wlroots compositors commonly provide them.
GNOME and KDE are not currently supported because they use different capture
paths.

## Build and run

```sh
cargo build --release
./target/release/snapcrop
./target/release/snapcrop screenshot.png
```

Drag with the left mouse button to select. Release to save, or press `Escape`
or right click to cancel. Cancellation exits with status 2 and does not touch
the requested file. With no file argument the crop is copied only to the
clipboard; with a file argument it is both saved and copied.

The selection overlay uses a crosshair cursor and shows a small instruction
banner on the edge opposite the pointer. The banner disappears while dragging
and is never included in the resulting crop.

For example, a Sway binding can invoke:

```text
bindsym Print exec snapcrop "$HOME/Pictures/screenshot.png"
```

Every output is captured and frozen before selection starts. A drag may cross
output boundaries; mixed-scale captures are stitched at the highest selected
output scale, with transparent pixels for gaps in the physical layout. Portal
support remains intentionally deferred; see [TODO.md](TODO.md).

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The release binary links only the Rust/Wayland protocol stack and a PNG encoder.
Clipboard contents are served by a background instance of the same executable,
which exits when another application replaces the clipboard.

For rotated, flipped, scaled, and multi-output manual verification, see
[TESTING.md](TESTING.md).

