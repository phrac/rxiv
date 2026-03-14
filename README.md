# rxiv

Fast, minimal image viewer in Rust. No UI chrome — just the picture.

## Install

```
cargo build --release
cp target/release/rxiv ~/.local/bin/
```

## Usage

```
rxiv [path]
```

Pass an image file or a directory. Defaults to the current directory. The window sizes itself to the image (clamped to screen).

## Controls

| Key | Action |
|---|---|
| PgDn / Space / Scroll Down | Next image |
| PgUp / Backspace / Scroll Up | Previous image |
| f | Toggle fullscreen |
| Del | Delete current image |
| + / = | Zoom in |
| - | Zoom out |
| 0 | Reset zoom/pan |
| Arrow keys | Pan (when zoomed) |
| q / Esc | Quit (Esc exits fullscreen first) |

## Supported Formats

png, jpg, jpeg, gif (animated), apng, bmp, tiff, webp, ico, pnm, pbm, pgm, ppm, qoi

## Dependencies

- [winit](https://github.com/rust-windowing/winit) — windowing
- [softbuffer](https://github.com/rust-windowing/softbuffer) — pixel buffer
- [image](https://github.com/image-rs/image) — decoding

## License

MIT
