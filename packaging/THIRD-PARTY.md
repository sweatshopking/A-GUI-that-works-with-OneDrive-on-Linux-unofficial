# Third-party software

## OneDrive Client for Linux

The sync engine bundled in these packages is the OneDrive Client for Linux by abraunegg,
licensed under the GNU General Public License v3.0. Its license is installed next to this file
as `onedrive-LICENSE`.

- Project: https://github.com/abraunegg/onedrive
- Source for the bundled version: https://github.com/abraunegg/onedrive/tree/v2.5.11
  (the exact tag is set by `ONEDRIVE_TAG` in `packaging/build.sh`)

It is built unmodified from that tagged source by `packaging/build.sh`.

## Rust libraries

The app itself is built from Rust crates listed in `Cargo.lock`, under their own licenses
(MIT, Apache-2.0, GPL-3.0 for Slint, and similar). Slint is used under its GPL-3.0 option.
