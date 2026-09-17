# Manual compositor verification

Run these checks after a release build:

```sh
cargo build --release
./target/release/snapcrop /tmp/snapcrop-test.png
```

For each case, place distinguishable content near every screen edge, drag a
small selection over that content, and verify both the PNG and a paste from the
clipboard. Confirm `Escape` and right click leave the target path untouched.

| Case | Expected result |
| --- | --- |
| One output, scale 1 | Crop pixels match the frozen overlay. |
| One output, scale 2 | Crop remains sharp at physical resolution. |
| Two horizontal outputs | A single drag crossing the join produces one image. |
| Outputs with a gap | The gap is transparent in the PNG. |
| Mixed scales | Lower-scale content is expanded to the higher selected scale. |
| `normal`, `90`, `180`, `270` | Content is upright and the selected region is exact. |
| `flipped`, `flipped_90`, `flipped_180`, `flipped_270` | Content is not mirrored or rotated unexpectedly. |
| Output disconnect while open | snapcrop cancels cleanly and writes no output file. |

Suggested transform controls:

- Sway: `swaymsg output NAME transform 90` (replace `90` with the desired
  transform), then restore with `normal`.
- Hyprland: temporarily set `transform = N` in the monitor configuration, where
  `N` is Hyprland's transform value, then reload its configuration.
- River/wlroots: use the compositor's output-management tool (for example
  `wlr-randr`) if available.

Do not use a production clipboard or output filename for this matrix; each run
replaces the clipboard selection by design.
