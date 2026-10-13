# Generated image inputs

These fixtures contain solid-color image content authored for the tests; no
third-party image assets are included. Tests read the committed files and do
not run the generation tools. To reproduce them, use ImageMagick `7.1.2-26`
and macOS `sips-316`:

```sh
magick -size 2x3 xc:red frame-red.png
magick -size 2x3 xc:blue frame-blue.png
magick -delay 10 -loop 0 frame-red.png frame-blue.png two-frame.apng
magick -delay 10 -loop 0 frame-red.png frame-blue.png two-frame.webp
magick -size 1x1 xc:red tiny-rgb-source.png
sips -s format jp2 tiny-rgb-source.png --out tiny-rgb.jp2
rm frame-red.png frame-blue.png tiny-rgb-source.png
```

The commands above are run from a scratch directory, not this fixture
directory. Verify committed outputs with `shasum -a 256` (macOS) or
`sha256sum` (Linux):

```text
0e72eff7770ccd570afc2f1da4426a3374bc6465553b3e1a5b1c7b893b83e36f  two-frame.apng
f4e46247743269f5c35b7967d66832219f56429097ecc904aa173f26cd58c0e5  two-frame.webp
cd6561649da06ea8624cc4fa9ebbacbebbf9d4a709b5aa68e0da13b63568c530  tiny-rgb.jp2
```
