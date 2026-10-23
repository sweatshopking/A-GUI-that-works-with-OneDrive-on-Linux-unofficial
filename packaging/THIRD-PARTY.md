# Third-party software

## OneDrive Client for Linux

The syncing is done by the OneDrive Client for Linux by abraunegg (GPL-3.0,
https://github.com/abraunegg/onedrive). It is **not** included in these packages: the app uses the
copy installed on your system, so it keeps getting updates from its own packages.

## Rust libraries

The app itself is built from the Rust crates listed in `Cargo.lock`, under their own licenses:
MIT, Apache-2.0, BSD-2/3-Clause, Zlib, ISC, Unicode-3.0, Unlicense, CC0-1.0 and CDLA-Permissive-2.0,
all compatible with the GPL-3.0. Slint is used under its GPL-3.0 option.

One of them, ksni (the tray icon, public domain under the Unlicense), is built from a copy in
`vendor/ksni` with one small addition: it accepts the activation token desktops hand over when
the icon is clicked, so the panel opens in front. `vendor/ksni/README.vendored` describes it.

## System libraries in the AppImage

So that one AppImage runs on many distributions, it also carries a few small unmodified libraries
from Ubuntu 22.04 (currently libpng and Brotli).
Inside the AppImage, `usr/share/doc/<package>/copyright` holds each one's license, and
`usr/share/licenses/linux-onedrive-gui/BUNDLED-LIBRARIES.txt` lists the exact source package and
version of each. Their source is available from `https://launchpad.net/ubuntu/+source/<source>/<version>`
or with `apt-get source <source>=<version>` on Ubuntu 22.04. If you can't get it there, open an
issue on this project and it will be provided.

The .deb and .rpm packages don't bundle system libraries; your distribution installs them.
