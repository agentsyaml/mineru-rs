# Generated input fixtures

These files are generated from solid colors and carry no third-party image content.
The commands run once when the fixtures were created; tests do not invoke these tools.

ImageMagick `7.1.2-26` and macOS `sips-316`:

```sh
magick -size 2x3 xc:red frame-red.png
magick -size 2x3 xc:blue frame-blue.png
magick -delay 10 -loop 0 frame-red.png frame-blue.png two-frame.apng
magick -delay 10 -loop 0 frame-red.png frame-blue.png two-frame.webp
magick -size 1x1 xc:red tiny-rgb-source.png
sips -s format jp2 tiny-rgb-source.png --out tiny-rgb.jp2
rm frame-red.png frame-blue.png tiny-rgb-source.png
```

SHA-256:

```text
0e72eff7770ccd570afc2f1da4426a3374bc6465553b3e1a5b1c7b893b83e36f  two-frame.apng
f4e46247743269f5c35b7967d66832219f56429097ecc904aa173f26cd58c0e5  two-frame.webp
cd6561649da06ea8624cc4fa9ebbacbebbf9d4a709b5aa68e0da13b63568c530  tiny-rgb.jp2
```
