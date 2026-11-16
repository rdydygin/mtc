# Test data

`mtc-test-font.ttf` (560 bytes) lets tests lay out SVG text without
system fonts. It is `tests/fonts/demo.ttf` from
[ttf-parser](https://github.com/harfbuzz/ttf-parser) 0.25.1 (MIT OR
Apache-2.0), one glyph (`A`) and a `.notdef`, with a `name` table added so
font databases can load it as the family `MtcTest`:

```sh
python3 -I make-test-font.py path/to/ttf-parser-0.25.1/tests/fonts/demo.ttf mtc-test-font.ttf
```
