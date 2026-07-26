import { defineConfig } from "vite";
import { viteSingleFile } from "vite-plugin-singlefile";

// The daemon embeds `dist/index.html` with `include_str!`, so the build must
// produce exactly one self-contained file: no sibling JS or CSS to resolve at
// runtime, and no hashed filenames for a Rust `include_str!` to chase.
//
// That keeps the daemon a single deployable binary with no asset path to get
// wrong — the property M00 chose, and the reason the frontend is a build step
// rather than a directory of files the daemon has to serve.
//
// `dist/` is committed. The Rust build must work with no network (CI vendors
// crates and builds `--offline`), and an `npm ci` in that path would break it.
// Committing the output keeps the Rust side hermetic and the Nix flake free of
// node entirely; CI rebuilds the bundle and fails if the committed copy is
// stale, so the two cannot drift.
export default defineConfig({
  plugins: [viteSingleFile()],
  build: {
    outDir: "dist",
    // Belt and braces alongside the plugin: without this, an asset over the
    // 4 KiB default would be emitted as a sibling file and the single-file
    // assumption would break silently rather than loudly.
    assetsInlineLimit: Number.MAX_SAFE_INTEGER,
    // A source map would be a second file the embedded page cannot fetch.
    sourcemap: false,
    // Vite warns about chunk size above 500 KiB; this bundle is far smaller,
    // and a warning nobody can act on is noise in CI logs.
    chunkSizeWarningLimit: 2000,
  },
});
