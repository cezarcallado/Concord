# Third-party notices

concord (called voxhub before v1.0.1) embeds the following assets in the desktop app
(`concord.exe`) and, for the logo, in the server (`concord-server.exe`). Rust crate dependencies
are listed in `Cargo.lock`; vendored code keeps its own license files under `vendor/`.

## Logo (v1.0.1)

- Files: `app/assets/logo/concord.ico` and `app/assets/logo/concord-logo-{16,24,32,48,64,128,256,512}.png`
  (a white speech-bubble face in a black circle, transparent background).
- Created by Cezar Callado for concord, with the help of an AI image generator. It is the project's
  logo and is not covered by the MIT license of the code. Please don't use it to represent other
  projects.
- Used unchanged as the exe icon (Windows resource) and, resized, as the window, taskbar, tray and
  notification icon and on the home screen and in Configurações → Sobre.

## Fonts (v1.0 interface)

### Atkinson Hyperlegible Next

- Files: `app/assets/fonts/AtkinsonHyperlegibleNext-Regular.ttf`, `AtkinsonHyperlegibleNext-SemiBold.ttf`
- Copyright 2020-2024 The Atkinson Hyperlegible Next Project Authors
  (https://github.com/googlefonts/atkinson-hyperlegible-next)
- License: SIL Open Font License 1.1, full text in `app/assets/fonts/OFL-AtkinsonHyperlegibleNext.txt`
- Source: google/fonts, `ofl/atkinsonhyperlegiblenext/AtkinsonHyperlegibleNext[wght].ttf` at commit
  47d54ca1ebb14fd864695f1d39dc734f39017c33 (SHA-256 5a455d1cfa099b601ab70751bb9673e8fe1854dc4500c80e1a220d0d75e31745)
- Modification: static instances of the variable font at wght=400 and wght=600 (fontTools
  `varLib.instancer`); no glyph was changed.

### Bricolage Grotesque

- File: `app/assets/fonts/BricolageGrotesque-SemiBold.ttf`
- Copyright 2022 The Bricolage Grotesque Project Authors (https://github.com/ateliertriay/bricolage)
- License: SIL Open Font License 1.1, full text in `app/assets/fonts/OFL-BricolageGrotesque.txt`
- Source: google/fonts, `ofl/bricolagegrotesque/BricolageGrotesque[opsz,wdth,wght].ttf` at commit
  47d54ca1ebb14fd864695f1d39dc734f39017c33 (SHA-256 413e7357809ddd12fd80a96a8a396de0e401638d4acd3cb3e37532f0472ac682)
- Modification: a static instance at wght=600, wdth=100, opsz=18 (fontTools `varLib.instancer`),
  renamed "Bricolage Grotesque SemiBold"; no glyph was changed.

## QR code (v1.1)

- The `qrcode` crate (https://github.com/kennytm/qrcode-rust), default features off, for the Pix
  QR code in the desktop app. Copyright (c) 2016 kennytm. License: MIT OR Apache-2.0.

## Icons

### Phosphor Icons

- Regular weight, through the `egui-phosphor` crate 0.14 (MIT OR Apache-2.0), subset at compile
  time to the icons the app uses.
- Copyright (c) 2020-2021 Phosphor Icons (https://phosphoricons.com; license text from github.com/phosphor-icons/web)
- License: MIT

```
MIT License

Copyright (c) 2020-2021 Phosphor Icons

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Emoji (v1.1)

### Twemoji

- Files: `app/assets/emoji/atlas.png` (the pictures) and `app/assets/emoji/index.txt` (which
  emoji is where); attribution and license notice in `app/assets/emoji/LICENSE-Twemoji.txt`.
- Twemoji © Twitter, Inc and other contributors. Graphics licensed under CC-BY 4.0:
  https://creativecommons.org/licenses/by/4.0/
- Source: the maintained Twemoji fork, https://github.com/jdecked/twemoji, release tag `v15.1.0`,
  `assets/72x72/<code points>.png` (fetched from
  `https://cdn.jsdelivr.net/gh/jdecked/twemoji@v15.1.0/assets/72x72/`).
- Changes: only the emoji the app uses (its reaction, sound and status picks plus common ones)
  were taken; each was scaled from 72×72 to 48×48 pixels and packed with the others into one PNG
  atlas with a shared palette of 256 colours. The pictures themselves were not edited. They are
  drawn in colour over the monochrome emoji font's glyphs.

## egui's bundled fonts (unchanged, kept as fallbacks)

Ubuntu-Light (Ubuntu Font Licence 1.0), Hack (MIT / Bitstream Vera), Noto Emoji (SIL OFL 1.1) and
emoji-icon-font (SIL OFL 1.1), as shipped by the `epaint_default_fonts` crate.

## Rust crates under licenses other than MIT / Apache-2.0

Every crate is used unchanged, as published on crates.io (exact versions and checksums in
`Cargo.lock`; each crate's source, with its license file, is on crates.io). Most dependencies are
MIT and/or Apache-2.0; these are not (or not only):

- **MPL-2.0** (file-level copyleft: the source of these files stays available on crates.io, and
  concord does not modify them):
  - `attohttpc` 0.30: the small HTTP client `igd-next` uses for the router's UPnP control (host
    mode, "Criar servidor"); built without TLS or compression.
  - `symphonia` 0.5 and its codec / format crates (`symphonia-core`, `-bundle-flac`, `-bundle-mp3`,
    `-codec-aac`, `-codec-alac`, `-codec-pcm`, `-codec-vorbis`, `-format-isomp4`, `-format-ogg`,
    `-format-riff`, `-metadata`, `-utils-xiph`): decoding the soundboard's audio files.
- **BSD-3-Clause**: `nnnoiseless` (RNNoise, "IA leve"), `subtle`; `matchit` (MIT AND BSD-3-Clause).
- **ISC**: `rustls-webpki`, `untrusted`, `libloading`; `ring` (Apache-2.0 AND ISC); `aws-lc-rs` and
  `aws-lc-sys` (ISC, Apache-2.0, MIT and BSD-3-Clause parts).
- **CDLA-Permissive-2.0**: `webpki-roots` (the Mozilla root certificates, for link previews and
  the outgoing HTTPS requests).
- **Unicode-3.0**: the ICU4X crates `url` uses (`icu_*`, `litemap`, `potential_utf`, `tinystr`,
  `writeable`, `yoke`, `zerofrom`, `zerotrie`, `zerovec` and their derive crates).
- **Zlib**: `foldhash`, `zlib-rs`. **CC0-1.0**: `tiny-keccak`.

Host mode also brings in `igd-next` and `xmltree` (both MIT), for the router's UPnP.

## TLS between the app and the server (v1.1)

The server makes its own self-signed certificate with `rcgen` 0.14 and its DER encoder `yasna`
0.6 (both MIT OR Apache-2.0, per their Cargo metadata). The TLS itself is `rustls` /
`tokio-rustls` on the `ring` provider, and the first request line is read with `httparse` (all
already in the tree, licenses above). `Cargo.lock` also lists `x509-parser` and its ASN.1 crates
(`asn1-rs`, `der-parser`, `oid-registry`, ...) as optional dependencies of `rcgen`; they are not
built into concord.

## End-to-end encryption keys (v1.1)

The app's per-device identity keys and their key-wrapping envelope (`crates/vox-e2ee`) use
`x25519-dalek` 2.0.1 (BSD-3-Clause; Copyright (c) 2017-2021 isis agora lovecruft, 2019-2021
DebugSteven) and `curve25519-dalek` 4.1.3 (BSD-3-Clause; Copyright (c) 2016-2021 isis agora
lovecruft, 2016-2021 Henry de Valence), with `curve25519-dalek-derive` 0.1 (its build-time
macros) and `rand_core` 0.6 (both MIT OR Apache-2.0). The HKDF and ChaCha20-Poly1305 are `ring`'s,
the randomness `getrandom`'s and the wiping `zeroize`'s (already in the tree, licenses above).
`Cargo.lock` also lists `fiat-crypto` as an optional backend of `curve25519-dalek`; it is not built
into concord. The server does not link any of it.
