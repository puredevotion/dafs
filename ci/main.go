// Package main is the Dagger CI module for DAFS.
//
// # Why this exists alongside GitHub Actions
//
// Both run the same checks, deliberately. GitHub Actions is free and unmetered
// for a public repository and gives every fork and external pull request CI
// without setup, so it stays as a gate. This module means none of that is
// *load-bearing*: the identical suite runs on any machine with Dagger, so the
// project does not depend on a single forge remaining free, available, or
// willing to host it.
//
// The two agreeing is the point. When they disagree, that is a real
// environment-dependency bug worth knowing about — which is exactly the class of
// problem that cost real time during M00, when a Nix build failed locally for
// reasons that had nothing to do with the tree.
//
// Every function here is runnable locally with no credentials:
//
//	dagger call check          --source=..     # everything, in parallel
//	dagger call commit-lint    --source=.. --base=main  # Conventional Commits since base
//	dagger call test           --source=..
//	dagger call lint           --source=..
//	dagger call fmt-check      --source=..
//	dagger call hermetic       --source=..     # builds with the network off
//	dagger call rss-ceiling    --source=..     # release binary, real RSS
//	dagger call audit          --source=..
//	dagger call fuzz           --source=.. --seconds=60
//	dagger call private-refs   --source=..
//	dagger call gitleaks       --source=.. --base=main  # secret scan since base
//	dagger call ui-bundle      --source=..     # rebuild the UI, diff vs committed
//	dagger call dast           --source=..     # scanners against a live daemon
//	dagger call image          --source=..     # OCI container
//	dagger call build          --source=.. --target=x86_64-unknown-linux-gnu  # release tarball + sha256
//	dagger call sbom           --source=..     # CycloneDX SBOM per crate
//	dagger call sbom-scan      --source=..     # SBOM above, scanned for CVEs with grype
//
// `image` returns a Container rather than pushing it. Pushing needs credentials
// that are specific to whoever is deploying — this module stays credential-free
// so it can run anywhere, and the push is the caller's step.
//
// `build` and `sbom` mirror the `build`/`sbom` jobs in release.yml: same target
// matrix, same strip-then-package steps, same cargo-cyclonedx invocation. They
// exist here so a release artifact is reproducible on any machine with Dagger,
// not only inside GitHub Actions — the same reason the rest of this module
// exists. `publish` (cutting the GitHub Release itself) stays GitHub-only: it is
// the one step that is genuinely tied to that forge.
package main

import (
	"context"
	"fmt"
	"strings"

	"dagger/dafs-ci/internal/dagger"
)

// Pinned so a CI run is reproducible and does not silently change when upstream
// moves a floating tag. Bump deliberately.
const (
	rustImage    = "rust:1.97-bookworm"
	nightlyImage = "rustlang/rust:nightly-bookworm"
	// Frontend builds only. Node appears nowhere in the daemon's own build —
	// the UI bundle is committed, so `cargo build` and `nix build` need no
	// JavaScript toolchain at all. See UiBundle.
	nodeImage = "node:24-alpine"
	// Dynamic scanning. ZAP's baseline is driven from the GitHub workflow,
	// which has a maintained action for it; nuclei runs in both places.
	// Pinned to a real release, not :latest, for the same reproducibility
	// reason every other image here is pinned.
	nucleiImage = "projectdiscovery/nuclei:v3.11.0"
	// Distroless: no shell, no package manager, and it carries CA certificates,
	// which the daemon will need once it talks to peers.
	runtimeImage = "gcr.io/distroless/cc-debian12:nonroot"
	// Backstop for .githooks/pre-push. Pinned to the same version the GitHub
	// workflow's gitleaks job installs directly, so a finding here or there
	// is a version-drift bug, not two different scanners disagreeing.
	gitleaksImage = "ghcr.io/gitleaks/gitleaks:v8.30.1"
)

// DafsCi is the CI module root.
type DafsCi struct{}

// rustBase returns a Rust container with the source copied in and cargo's
// caches wired to Dagger cache volumes, so repeated local runs do not
// recompile the world. The volumes are shared across invocations on the same
// engine.
//
// WithDirectory (copy), not WithMountedDirectory (bind mount), for `source`
// here and at every other call site in this file: per BuildKit's own
// documented behavior (dagger/dagger#6421), a mounted directory only gets
// content-hash cache validation when it is BOTH a non-root mount AND
// read-only. A plain writable WithMountedDirectory -- what every one of
// these call sites used before -- skips that check entirely, so a cached
// downstream layer can legally be reused even when the mounted source
// content actually changed. WithDirectory copies the content into the
// container's own filesystem layer instead, which is always content-
// addressed like any other layer, no such carve-out. Same fix applied to
// coredns-plugins' own ci/main.go after a live incident there (a pod found
// serving a cert for an SNI name absent from its own Corefile, root-caused
// as a stale/cached vendored-source layer) surfaced this exact BuildKit
// behavior.
func (m *DafsCi) rustBase(source *dagger.Directory, image string) *dagger.Container {
	return dag.Container().
		From(image).
		WithMountedCache("/usr/local/cargo/registry", dag.CacheVolume("dafs-cargo-registry")).
		WithMountedCache("/usr/local/cargo/git", dag.CacheVolume("dafs-cargo-git")).
		WithMountedCache("/src/target", dag.CacheVolume("dafs-cargo-target")).
		WithDirectory("/src", source).
		WithWorkdir("/src").
		// Match the GitHub workflow: warnings fail. Kept out of the source so
		// local iteration is not painful.
		WithEnvVariable("RUSTFLAGS", "-D warnings").
		WithEnvVariable("CARGO_TERM_COLOR", "always")
}

// CommitLint asserts every commit subject in `base..HEAD` is a Conventional
// Commit. `base` defaults to "main" if empty.
//
// release-please (release-please.yml) derives the version bump and
// CHANGELOG.md entry from these subjects; a commit it can't parse is silently
// excluded rather than erroring, which is how v0.0.1 shipped with an empty
// changelog. Kept in lockstep with the commit-lint job in the GitHub workflow
// and the local commit-msg hook in .githooks/.
func (m *DafsCi) CommitLint(ctx context.Context, source *dagger.Directory, base string) (string, error) {
	if base == "" {
		base = "main"
	}

	pattern := `^(feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert)(\([a-z0-9./-]+\))?!?: .+`
	script := fmt.Sprintf(`set -uo pipefail
bad=0
while read -r sha subject; do
  case "$subject" in
    "Merge "*|"fixup! "*|"squash! "*) continue ;;
  esac
  if ! printf '%%s' "$subject" | grep -qE '%s'; then
    echo "$sha does not follow Conventional Commits: \"$subject\""
    bad=1
  fi
done < <(git log --format='%%H %%s' %s..HEAD)
[ "$bad" -eq 0 ] || exit 1
echo "all commit subjects are Conventional Commits"`, pattern, base)

	return dag.Container().
		From("alpine:3.21").
		WithExec([]string{"apk", "add", "--no-cache", "git", "bash"}).
		WithDirectory("/src", source).
		WithWorkdir("/src").
		WithExec([]string{"bash", "-c", script}).
		Stdout(ctx)
}

// CargoVersions asserts every workspace member pins a literal
// [package.version] matching [workspace.package].version.
//
// release-please's cargo-workspace plugin needs a literal semver to read and
// bump per member; `version.workspace = true` parses as a table, not a
// string, and broke every release-please run twice before
// scripts/check-crate-versions.sh existed. Kept in lockstep with the
// cargo-versions job in the GitHub workflow.
func (m *DafsCi) CargoVersions(ctx context.Context, source *dagger.Directory) (string, error) {
	return dag.Container().
		From("alpine:3.21").
		WithExec([]string{"apk", "add", "--no-cache", "gawk", "grep"}).
		WithDirectory("/src", source).
		WithWorkdir("/src").
		WithExec([]string{"sh", "scripts/check-crate-versions.sh"}).
		Stdout(ctx)
}

// FmtCheck asserts the tree is rustfmt-clean.
func (m *DafsCi) FmtCheck(ctx context.Context, source *dagger.Directory) (string, error) {
	return m.rustBase(source, rustImage).
		WithExec([]string{"rustup", "component", "add", "rustfmt"}).
		WithExec([]string{"cargo", "fmt", "--all", "--check"}).
		Stdout(ctx)
}

// Lint runs clippy across every target and feature.
func (m *DafsCi) Lint(ctx context.Context, source *dagger.Directory) (string, error) {
	return m.rustBase(source, rustImage).
		WithExec([]string{"rustup", "component", "add", "clippy"}).
		WithExec([]string{"cargo", "clippy", "--all-targets", "--all-features"}).
		Stdout(ctx)
}

// Test runs the workspace test suite.
func (m *DafsCi) Test(ctx context.Context, source *dagger.Directory) (string, error) {
	return m.rustBase(source, rustImage).
		WithExec([]string{"cargo", "test", "--all-features"}).
		Stdout(ctx)
}

// Hermetic proves the tree builds and tests with no network access.
//
// This is the check that makes the repository publishable: a tree that needs
// something from a private environment cannot be built by anyone but its author.
// Dependencies are vendored first (which does use the network), then cargo is
// pointed at the vendor directory and run with --offline, so anything still
// reaching out fails loudly rather than silently succeeding on a cached artifact.
func (m *DafsCi) Hermetic(ctx context.Context, source *dagger.Directory) (string, error) {
	return m.rustBase(source, rustImage).
		WithExec([]string{"sh", "-c", "mkdir -p .cargo && cargo vendor --versioned-dirs vendor > .cargo/config.toml"}).
		WithExec([]string{"cargo", "build", "--offline", "--all-targets"}).
		WithExec([]string{"cargo", "test", "--offline"}).
		Stdout(ctx)
}

// RssCeiling builds the release daemon and asserts the RSS ceilings from
// docs/memory-budget.md against the real binary.
//
// Must be the release profile: debug builds allocate differently and are not a
// valid proxy. `--nocapture` is passed so the measured figure appears in the
// log even on success — a ceiling that passes silently tells you nothing about
// how much headroom is left.
func (m *DafsCi) RssCeiling(ctx context.Context, source *dagger.Directory) (string, error) {
	return m.rustBase(source, rustImage).
		WithExec([]string{"cargo", "build", "--release", "-p", "dafs-daemon"}).
		WithExec([]string{"cargo", "test", "--release", "-p", "dafs-memtest", "--", "--nocapture"}).
		Stdout(ctx)
}

// Audit checks dependencies for advisories, licences, banned crates, and
// unexpected sources.
func (m *DafsCi) Audit(ctx context.Context, source *dagger.Directory) (string, error) {
	return m.rustBase(source, rustImage).
		WithExec([]string{"cargo", "install", "cargo-audit", "cargo-deny", "--locked"}).
		WithExec([]string{"cargo", "audit"}).
		WithExec([]string{"cargo", "deny", "check"}).
		Stdout(ctx)
}

// fuzzTargets is every target in fuzz/Cargo.toml.
//
// Listed here rather than discovered, so adding a target to the fuzz crate
// without wiring it into CI is a visible omission in this file rather than a
// target that silently never runs. Kept in lockstep with the fuzz-smoke job in
// the GitHub workflow.
var fuzzTargets = []string{
	// The store against a corrupted database file.
	"migrations",
	// Path interning against arbitrary filename bytes. On Unix any sequence
	// but NUL and `/` is a legal component, so invalid UTF-8 and
	// traversal-shaped names arrive from simply scanning a directory.
	"paths",
}

// Fuzz runs every fuzz target for `seconds` each.
//
// Nightly, because cargo-fuzz needs it. A short run per push is not a campaign;
// it catches a target that has stopped building and the occasional shallow
// crash. Long campaigns belong on a schedule, not on every push.
func (m *DafsCi) Fuzz(ctx context.Context, source *dagger.Directory, seconds int) (string, error) {
	if seconds <= 0 {
		seconds = 60
	}

	container := m.rustBase(source, nightlyImage).
		WithExec([]string{"cargo", "install", "cargo-fuzz", "--locked"})

	for _, target := range fuzzTargets {
		container = container.WithExec([]string{"cargo", "fuzz", "run", target, "--",
			fmt.Sprintf("-max_total_time=%d", seconds)})
	}

	return container.Stdout(ctx)
}

// PrivateRefs scans for internal hostnames and private address ranges.
//
// The structural counterpart to Hermetic. A denylist alone fails open — it only
// catches what was thought of when it was written — which is why the offline
// build above is the real gate and this is the cheap fast check for the obvious
// cases. Kept in lockstep with the same list in the GitHub workflow.
func (m *DafsCi) PrivateRefs(ctx context.Context, source *dagger.Directory) (string, error) {
	patterns := strings.Join([]string{
		`\.home\.arpa`,
		`\.internal\.arpa`,
		`192\.168\.`,
		`10\.(4[0-9]|[0-9])\.`,
		`172\.(1[6-9]|2[0-9]|3[01])\.`,
		`100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.`,
	}, "|")

	// `!` inverts: grep exits 0 when it finds a match, and a match is what must
	// fail the check. Excludes target/ and vendor/, which contain third-party
	// source that legitimately mentions private ranges in its own tests.
	script := fmt.Sprintf(
		`! grep -rInE '%s' --include='*.rs' --include='*.toml' --include='*.md' `+
			`--include='*.yml' --include='*.nix' --include='*.html' `+
			`--exclude-dir=target --exclude-dir=vendor . `+
			`|| { echo "private reference found (above)"; exit 1; }`,
		patterns)

	return dag.Container().
		From("alpine:3.21").
		WithDirectory("/src", source).
		WithWorkdir("/src").
		WithExec([]string{"sh", "-c", script}).
		Stdout(ctx)
}

// Gitleaks scans `base..HEAD` for hardcoded secrets.
//
// Backstop for .githooks/pre-push, the same pairing CommitLint is to the
// commit-msg hook. `base` defaults to "main" if empty — same convention as
// CommitLint, for the same reason: a local `dagger call` has no PR context to
// read a base ref from, unlike the GitHub workflow's `gitleaks` job.
func (m *DafsCi) Gitleaks(ctx context.Context, source *dagger.Directory, base string) (string, error) {
	if base == "" {
		base = "main"
	}

	return dag.Container().
		From(gitleaksImage).
		WithDirectory("/src", source).
		WithWorkdir("/src").
		WithExec([]string{"gitleaks", "git", "--log-opts=" + base + "..HEAD", "--redact", "--no-banner", "."}).
		Stdout(ctx)
}

// Dast runs dynamic scanners against a live daemon.
//
// Scoped to milestones that actually have an HTTP surface — three in the whole
// roadmap — rather than listed against every one. M01's timeline API is the
// first, so this exists from here and not before.
//
// The daemon binds loopback and is unauthenticated by design, so "no auth" is
// not the finding this looks for. It is the accidental kind: a reflected value,
// a header leaking a path, an unbounded request body. The M01 pass found the
// last of those — a 2 MB log-filter payload accepted with a 200.
//
// Kept in lockstep with the `dast` job in the GitHub workflow.
func (m *DafsCi) Dast(ctx context.Context, source *dagger.Directory) (string, error) {
	daemon := m.rustBase(source, rustImage).
		WithExec([]string{"cargo", "build", "--release", "-p", "dafs-daemon"}).
		WithExec([]string{"cp", "/src/target/release/dafs", "/dafs"})

	// A corpus, so the scan has something to find. A scanner pointed at an
	// empty timeline passes trivially, which is worse than not running it.
	service := dag.Container().
		From(runtimeImage).
		WithFile("/dafs", daemon.File("/dafs")).
		WithEnvVariable("DAFS_LISTEN", "0.0.0.0:7878").
		WithEnvVariable("DAFS_DATA_DIR", "/tmp/data").
		WithEnvVariable("DAFS_WATCH", "/tmp/corpus").
		WithDirectory("/tmp/corpus", dag.Directory().
			WithNewFile("note.md", "hello").
			WithNewFile("docs/readme.txt", "world")).
		WithExposedPort(7878).
		AsService()

	return dag.Container().
		From(nucleiImage).
		// Nuclei's template repo is ~100MB and is re-cloned on every fresh
		// container otherwise. A persistent cache volume (shared across
		// Dagger invocations on the same engine, same idea as the cargo
		// caches in rustBase) turns every run after the first into a no-op
		// fetch instead of a full clone — this is what "pre-installed"
		// means for a raw container rather than a hosted action's own cache.
		WithMountedCache("/root/.config/nuclei", dag.CacheVolume("dafs-nuclei-templates")).
		WithServiceBinding("dafs", service).
		// Fail the check only on findings that are actually actionable;
		// informational output on an unauthenticated local API is noise.
		WithExec([]string{"nuclei", "-target", "http://dafs:7878",
			"-severity", "critical,high,medium"}).
		Stdout(ctx)
}

// UiBundle rebuilds the frontend and asserts the committed bundle matches.
//
// `ui/dist/index.html` is committed and embedded into the daemon with
// include_str!, because the Rust build has to work with no network and an
// `npm ci` in front of `cargo build` would break Hermetic. The risk that buys
// is a committed artifact drifting from its source; this removes it by
// rebuilding and diffing.
//
// Kept in lockstep with the `ui-bundle` job in the GitHub workflow.
func (m *DafsCi) UiBundle(ctx context.Context, source *dagger.Directory) (string, error) {
	// `npm ci` rather than `npm install`: it installs exactly the lockfile and
	// fails if package.json and the lock disagree, which is what makes the
	// rebuild reproducible rather than merely likely.
	//
	// The single-file assertion matters because the daemon serves one string
	// and has no route for sibling assets — a build emitting a separate .js or
	// .css would render a blank page in production while every Rust test passed.
	script := `set -e
npm ci --prefix ui
npm run build --prefix ui
count=$(find ui/dist -type f | wc -l)
if [ "$count" -ne 1 ]; then
  echo "ui/dist must contain exactly one file, found $count"
  find ui/dist -type f
  exit 1
fi
if ! git diff --quiet -- ui/dist; then
  echo "ui/dist is out of date — run 'npm run build' in ui/ and commit the result"
  git diff --stat -- ui/dist
  exit 1
fi
echo "committed bundle matches a fresh build, and is a single file"`

	return dag.Container().
		From(nodeImage).
		// git is needed for the diff below and is not in the base node image.
		WithExec([]string{"apk", "add", "--no-cache", "git"}).
		WithDirectory("/src", source).
		WithWorkdir("/src").
		WithExec([]string{"sh", "-c", script}).
		Stdout(ctx)
}

// Image builds the runtime container.
//
// Returns the Container rather than pushing it: a push needs registry
// credentials specific to the deployer, and keeping this module credential-free
// is what lets it run anywhere. Callers push with `... publish` or export a
// tarball and use their own tooling — a registry behind mTLS client certs, for
// instance, cannot be driven by Dagger's native registry auth at all.
func (m *DafsCi) Image(source *dagger.Directory) *dagger.Container {
	built := m.rustBase(source, rustImage).
		WithExec([]string{"cargo", "build", "--release", "-p", "dafs-daemon"}).
		// Copy out of the cache-mounted target dir: a mounted cache is not part
		// of the container's filesystem, so the binary has to be moved somewhere
		// real before the next stage can see it.
		WithExec([]string{"cp", "/src/target/release/dafs", "/dafs"})

	return dag.Container().
		From(runtimeImage).
		WithFile("/dafs", built.File("/dafs")).
		WithUser("nonroot:nonroot").
		// Bind all interfaces inside a container: the daemon's loopback default
		// is right for a host process but would make the container unreachable.
		// Whether that port is actually exposed is the orchestrator's decision.
		WithEnvVariable("DAFS_LISTEN", "0.0.0.0:7878").
		WithEnvVariable("DAFS_DATA_DIR", "/data").
		WithExposedPort(7878).
		WithEntrypoint([]string{"/dafs"})
}

// releaseTargets maps a Rust target triple to the asset name release.yml
// publishes it under. Kept in lockstep with the `build` job's matrix there —
// a target added to one belongs in the other.
var releaseTargets = map[string]string{
	"x86_64-unknown-linux-gnu":  "dafs-x86_64-linux",
	"aarch64-unknown-linux-gnu": "dafs-aarch64-linux",
}

// Build cross-compiles the release daemon for `target`, strips it, and
// packages it exactly as release.yml's `build` job does: a `.tar.gz` plus a
// detached `.tar.gz.sha256`, both returned in the output directory.
//
// Stripping happens here rather than via the release profile, for the same
// reason release.yml does it in packaging: cargo applies profile-level
// `strip` to build-script and proc-macro binaries too, and stripping those is
// a route to obscure build failures. Doing it here affects only the artifact
// actually shipped.
func (m *DafsCi) Build(ctx context.Context, source *dagger.Directory, target string) (*dagger.Directory, error) {
	name, ok := releaseTargets[target]
	if !ok {
		return nil, fmt.Errorf("unsupported target %q (known: x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu)", target)
	}

	container := m.rustBase(source, rustImage).
		WithExec([]string{"rustup", "target", "add", target})

	stripBin := "strip"
	if target == "aarch64-unknown-linux-gnu" {
		// jemalloc and bundled SQLite are C, so cross-compiling needs a cross
		// linker and compiler too, not just a Rust target — same as release.yml.
		container = container.
			WithExec([]string{"apt-get", "update"}).
			WithExec([]string{"apt-get", "install", "-y", "gcc-aarch64-linux-gnu"}).
			WithEnvVariable("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER", "aarch64-linux-gnu-gcc").
			WithEnvVariable("CC_aarch64_unknown_linux_gnu", "aarch64-linux-gnu-gcc")
		stripBin = "aarch64-linux-gnu-strip"
	}

	container = container.WithExec([]string{"cargo", "build", "--release", "-p", "dafs-daemon", "--target", target})

	binPath := fmt.Sprintf("/src/target/%s/release/dafs", target)
	script := fmt.Sprintf(`set -e
%s %s
mkdir -p /out
tar -czf /out/%s.tar.gz -C "$(dirname %s)" dafs
sha256sum /out/%s.tar.gz > /out/%s.tar.gz.sha256
`, stripBin, binPath, name, binPath, name, name)

	container = container.WithExec([]string{"sh", "-c", script})
	return container.Directory("/out"), nil
}

// Sbom generates a CycloneDX SBOM for every crate in the workspace, mirroring
// release.yml's `sbom` job.
func (m *DafsCi) Sbom(ctx context.Context, source *dagger.Directory) *dagger.Directory {
	script := `set -e
cargo cyclonedx --format json --all
mkdir -p /out
find . -name '*.cdx.json' -not -path './target/*' -exec cp {} /out/ \;
`
	container := m.rustBase(source, rustImage).
		WithExec([]string{"cargo", "install", "cargo-cyclonedx", "--locked"}).
		WithExec([]string{"sh", "-c", script})
	return container.Directory("/out")
}

// SbomScan generates the CycloneDX SBOM (Sbom) and scans it with grype.
//
// Kept in lockstep with the `sbom` job in release.yml: cargo-audit/cargo-deny
// (the `audit` job) check the dependency graph cargo itself sees; this checks
// the SBOM that actually ships with a release, which is a real if narrow gap
// between the two — a mismatch would mean the SBOM doesn't describe what
// cargo-audit already cleared.
func (m *DafsCi) SbomScan(ctx context.Context, source *dagger.Directory) (string, error) {
	sbomDir := m.Sbom(source)

	script := `set -e
apk add --no-cache curl bash ca-certificates
curl -sSfL https://raw.githubusercontent.com/anchore/grype/main/install.sh | sh -s -- -b /usr/local/bin v0.116.0
status=0
for f in /sbom/*.cdx.json; do
  echo "== grype $f =="
  grype "sbom:$f" --fail-on high || status=1
done
exit "$status"
`
	return dag.Container().
		From("alpine:3.21").
		WithDirectory("/sbom", sbomDir).
		WithExec([]string{"sh", "-c", script}).
		Stdout(ctx)
}

// Check runs the whole suite concurrently and reports a per-check verdict.
//
// Every check runs even when an earlier one fails, and the failures are
// collected: a single run should tell you everything that is wrong, not just
// the first thing. Fuzzing is included at 30s — long enough to catch a broken
// target, short enough not to dominate the run.
func (m *DafsCi) Check(ctx context.Context, source *dagger.Directory) (string, error) {
	type result struct {
		name string
		err  error
	}

	checks := []struct {
		name string
		run  func() error
	}{
		{"cargo-versions", func() error { _, e := m.CargoVersions(ctx, source); return e }},
		{"fmt", func() error { _, e := m.FmtCheck(ctx, source); return e }},
		{"lint", func() error { _, e := m.Lint(ctx, source); return e }},
		{"test", func() error { _, e := m.Test(ctx, source); return e }},
		{"hermetic", func() error { _, e := m.Hermetic(ctx, source); return e }},
		{"rss-ceiling", func() error { _, e := m.RssCeiling(ctx, source); return e }},
		{"audit", func() error { _, e := m.Audit(ctx, source); return e }},
		{"private-refs", func() error { _, e := m.PrivateRefs(ctx, source); return e }},
		{"gitleaks", func() error { _, e := m.Gitleaks(ctx, source, ""); return e }},
		{"ui-bundle", func() error { _, e := m.UiBundle(ctx, source); return e }},
		{"dast", func() error { _, e := m.Dast(ctx, source); return e }},
		{"fuzz", func() error { _, e := m.Fuzz(ctx, source, 30); return e }},
	}

	results := make(chan result, len(checks))
	for _, c := range checks {
		go func(name string, run func() error) {
			results <- result{name, run()}
		}(c.name, c.run)
	}

	var report strings.Builder
	failed := 0
	for range checks {
		r := <-results
		if r.err != nil {
			failed++
			fmt.Fprintf(&report, "FAIL %s: %v\n", r.name, r.err)
		} else {
			fmt.Fprintf(&report, "PASS %s\n", r.name)
		}
	}

	if failed > 0 {
		return report.String(), fmt.Errorf("%d of %d checks failed", failed, len(checks))
	}
	fmt.Fprintf(&report, "\nall %d checks passed\n", len(checks))
	return report.String(), nil
}
