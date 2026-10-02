# oblyx

Convert GoodNotes `.goodnotes` notebook archives to SVG, PDF or PNG from the command line.

Not affiliated with or endorsed by GoodNotes.

## Features

- Vector-accurate strokes (width, color, opacity preserved), smoothed with
  Catmull-Rom interpolation so curves stay rounded instead of polygonal
- Embedded images (JPEG/PNG attachments and frames)
- Sticky notes, text boxes and equations
- Multi-page PDF with embedded images and text
- Bulk conversion: pass files and/or directories (scanned recursively)
- Fast: parallel decoding and rendering across CPU cores

## Build

```
cargo build --release
```

The binary is at `target/release/oblyx`.

## Usage

```
oblyx [OPTIONS] <INPUT>...
```

`INPUT` may be `.goodnotes` files or directories. Directories are scanned recursively for `.goodnotes` files (hidden directories are skipped).

### Options

| Option | Description | Default |
| --- | --- | --- |
| `-f, --format` | Output format(s): `svg`, `png`, `pdf`, `all`, or a comma-separated list | `svg` |
| `-o, --output` | Output directory (default: next to each input file) | — |
| `--dpi` | Raster resolution for PNG output | `144` |
| `--page` | Only convert pages whose UUID contains this text | — |
| `-j, --jobs` | Number of parallel jobs | all cores |
| `-q, --quiet` | Suppress per-file progress output | off |
| `--include-deleted` | Also render deleted (tombstoned) objects | off |

### Examples

```
# One file to SVG (default)
oblyx notebook.goodnotes

# Everything (SVG + PNG + PDF) into ./out
oblyx -f all -o out notebook.goodnotes

# Bulk: convert a whole folder tree to PDF at 300 DPI PNG too
oblyx -f pdf,png --dpi 300 ~/GoodNotes/

# Single page, higher raster resolution
oblyx --page 0547ACEA --dpi 300 -f png notebook.goodnotes
```

### Output layout

- **PDF**: `<output>/<stem>.pdf` — one multi-page file per notebook
- **SVG / PNG**: `<output>/<stem>/<page-uuid>.svg|png` — one file per page

## Format notes

`.goodnotes` files are ZIP archives (schema 35) containing a record stream per page.
Strokes are stored as `bv41`-compressed `tpl` blobs; elements (images, frames,
text, stickies, equations) are protobuf records paired with tombstone wrapper
records. Deleted objects are skipped unless `--include-deleted` is given.
See `FORMAT.md` in the original reverse-engineering notes for details.

## License

Private / all rights reserved unless stated otherwise.
