# kitkat-rs

Minimal PNG and JPEG viewer for terminals implementing the Kitty graphics protocol.

## Packages

The flake exposes four packages. Each installs the same `kitkat-rs` executable.

| Package | Resize path | Large PNG memory | Intended use |
| --- | --- | --- | --- |
| `.#` / `.#quality` | Full-frame RGB/RGBA decode, SIMD Lanczos3 | Proportional to decoded image size | Default; highest downscaling quality |
| `.#low-rss` | Row-streamed non-interlaced PNG decode, nearest-neighbor sampling | Proportional to one decoded row plus terminal-sized output | Very large non-interlaced PNGs under tight memory limits |
| `.#faster` | Full-frame RGB/RGBA decode, Rayon and SIMD Lanczos3 | Proportional to decoded image size | Fast Kitty-like rendering |
| `.#fastest` | Full-frame RGB/RGBA decode, Rayon and SIMD nearest-neighbor resize | Proportional to decoded image size | Lowest render latency |

`low-rss` falls back to a full-frame buffer for interlaced PNGs because Adam7 rows cannot be sampled independently. JPEG decoding is also full-frame. Its low-memory guarantee therefore applies specifically to non-interlaced PNG input.

`faster` uses the same Lanczos3 antialiasing as `quality`, but enables Rayon and optimizes for runtime speed rather than binary size. `fastest` deliberately trades that antialiasing for nearest-neighbor sampling and can show substantial aliasing when an image is reduced. `quality` remains the size-optimized default.

On a 32,767×9,259 RGB PNG, the development measurements were approximately:

| Package | Static binary | Runtime | Peak RSS |
| --- | ---: | ---: | ---: |
| `low-rss` | 1.0 MiB | 2.1–2.3 s | 14 MiB |
| `faster` | 6.8 MiB | about 2.2 s | 890 MiB |
| `fastest` | 6.8 MiB | 1.6–1.9 s | 890 MiB |

These figures are workload- and machine-dependent; they document the tradeoff rather than a performance guarantee.

## Usage

```console
nix run .#low-rss -- image.png
nix run .#faster -- image.jpg
nix run .#fastest -- image.jpg
```

Use `-` to read compressed image data from standard input.
