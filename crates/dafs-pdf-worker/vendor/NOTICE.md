# Vendored Pdfium

`libpdfium_linux_x64.so` is the unmodified `lib/libpdfium.so` from
[bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries)
release `chromium/7961` (PDFium 152.0.7961.0), asset `pdfium-linux-x64.tgz`
(sha256 `3019ad1cd6980e51d900bb9266f8980cb846cb8e0c1f6553c52a7a1626469020`).
7961 is above the `pdfium_7543` API binding set `../Cargo.toml` pins
`pdfium-render` to — an older binding set against a newer runtime library is
the supported direction (see that file's comment), so this pairing is safe.

Vendored here, rather than fetched at build or run time, so `cargo build`
produces a working `dafs-pdf-worker` with no network access and no Nix (or
any other external packaging step) required — see `src/main.rs`'s
`PDFIUM_SO_BYTES` for how it is embedded, and `ui/dist/index.html` /
`crates/dafs-api/src/lib.rs`'s `UI_INDEX` comment for the same
committed-build-output reasoning applied elsewhere in this repo.

## License

The release's own `LICENSE` file (MIT, copyright Benoit Blanchon) covers the
packaging scripts. `licenses/pdfium.txt` in that same release covers PDFium
itself: BSD-3-Clause (copyright The PDFium Authors), plus an Apache License
2.0 block for components PDFium bundles. Neither restricts redistributing
the compiled binary, which is what this file is.
