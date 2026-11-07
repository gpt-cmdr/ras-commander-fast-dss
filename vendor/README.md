# Vendored readers

`hdf5-reader` and `netcdf-reader` 0.12.1, from crates.io, with CLB's patches, used through `[patch.crates-io]` in the root `Cargo.toml`. They are part of the NetCDF source in `crates/dss-vortex`.

| Crate | Version | crates.io SHA-256 of the `.crate` | Upstream |
|---|---|---|---|
| `hdf5-reader` | 0.12.1 | `d1ac773e9e47f3765399b71525783c203c53844c83d52a73f3f313d0a040e413` | https://github.com/roteiro-gis/netcdf-rust |
| `netcdf-reader` | 0.12.1 | `a9f1a196246124ca06303e9d11ed268f499852277899e52fb47cc74e08fdd11b` | https://github.com/roteiro-gis/netcdf-rust |

**Licence.** Upstream's: `MIT OR Apache-2.0`, copyright 2026 Ian Norden. The published crates carry no licence file, so `LICENSE-MIT` and `LICENSE-APACHE` here are copied from the upstream repository at v0.9.3, the last tag in CLB's clone, and they are unchanged. CLB's changes are offered under the same terms. These files are modified: Apache-2.0 section 4(b) is met by [PATCHES.md](PATCHES.md), which lists each change and the files it touches, and each modified crate's commits in this repository's history.

**Not vendored.** `hdf5-core` and `netcdf-core` stay registry crates at 0.12.1, locked with checksums. Nothing here changes them.

**What differs from the tarball, beyond the patches.** `Cargo.toml.orig`, `Cargo.lock` and `.cargo_vcs_info.json` are not copied. 0.12.0 to 0.12.1 changes only `hdf5-reader`'s enum datatype parse and its tests, verified against the tarballs.

**Contact.** Nothing here has been sent upstream. Only the maintainer decides that.
