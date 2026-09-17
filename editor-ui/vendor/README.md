# Vendored libraries

Copied from the npm registry tarballs listed below. Nothing is loaded from a CDN
at runtime. The `sha256` of each tarball is what was downloaded; to upgrade,
fetch the new tarball, check it, and replace the folder.

| library | version | licence | tarball | tarball sha256 |
|---|---|---|---|---|
| leaflet | 1.9.4 | BSD-2-Clause | https://registry.npmjs.org/leaflet/-/leaflet-1.9.4.tgz | `84c65a256e50657896f54c33bd857b6849ebe94c817803be818bf32a3dde0b77` |
| qrcode-generator | 2.0.4 | MIT | https://registry.npmjs.org/qrcode-generator/-/qrcode-generator-2.0.4.tgz | `02e2e18a99a90b02dad940851f59b7c3c5fd1ab79cbdece8595cb06328878159` |
| @fontsource/atkinson-hyperlegible | 5.3.0 | OFL-1.1 | https://registry.npmjs.org/@fontsource/atkinson-hyperlegible/-/atkinson-hyperlegible-5.3.0.tgz | `6c22186d05f555e1ebf58431ef2c154445e64b760e381c483a56a0a0c5dc2d43` |

Files taken from each. Text files had trailing whitespace and CRLF line endings
normalised, and a final newline added, for the repo's pre-commit check; nothing
else was changed.

- `leaflet-1.9.4/`: `dist/leaflet.js`, `dist/leaflet.css`, `dist/images/*.png`, `LICENSE`
- `qrcode-generator-2.0.4/`: `dist/qrcode.mjs` (licence in the file header)
- `atkinson-hyperlegible-5.3.0/`: `files/atkinson-hyperlegible-latin-{400,700}-normal.woff2`, `LICENSE`

Atkinson Hyperlegible was chosen for the ops team: it was designed by the Braille
Institute to keep look-alike characters (`0`/`O`, `1`/`l`/`I`) distinct, which is
exactly the failure that matters when reading stop ids and stage numbers.

Map tiles come from OpenStreetMap (`TILE_URL` in `js/config.js`) and load in the
viewer's browser; change that one constant to point at an internal tile server.
