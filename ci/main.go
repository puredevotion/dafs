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
//	dagger call test           --source=..
//	dagger call lint           --source=..
//	dagger call fmt-check      --source=..
//	dagger call hermetic       --source=..     # builds with the network off
//	dagger call rss-ceiling    --source=..     # release binary, real RSS
//	dagger call audit          --source=..
//	dagger call fuzz           --source=.. --seconds=60
//	dagger call private-refs   --source=..
//	dagger call image          --source=..     # OCI container
//
// `image` returns a Container rather than pushing it. Pushing needs credentials
// that are specific to whoever is deploying — this module stays credential-free
// so it can run anywhere, and the push is the caller's step.
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
	// Distroless: no shell, no package manager, and it carries CA certificates,
	// which the daemon will need once it talks to peers.
	runtimeImage = "gcr.io/distroless/cc-debian12:nonroot"
)

// DafsCi is the CI module root.
type DafsCi struct{}

// rustBase returns a Rust container with the source mounted and cargo's caches
// wired to Dagger cache volumes, so repeated local runs do not recompile the
// world. The volumes are shared across invocations on the same engine.
func (m *DafsCi) rustBase(source *dagger.Directory, image string) *dagger.Container {
	return dag.Container().
		From(image).
		WithMountedCache("/usr/local/cargo/registry", dag.CacheVolume("dafs-cargo-registry")).
		WithMountedCache("/usr/local/cargo/git", dag.CacheVolume("dafs-cargo-git")).
		WithMountedCache("/src/target", dag.CacheVolume("dafs-cargo-target")).
		WithMountedDirectory("/src", source).
		WithWorkdir("/src").
		// Match the GitHub workflow: warnings fail. Kept out of the source so
		// local iteration is not painful.
		WithEnvVariable("RUSTFLAGS", "-D warnings").
		WithEnvVariable("CARGO_TERM_COLOR", "always")
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

// Fuzz runs the metadata-store fuzz target for `seconds`.
//
// Nightly, because cargo-fuzz needs it. A short run per push is not a campaign;
// it catches a target that has stopped building and the occasional shallow
// crash. Long campaigns belong on a schedule, not on every push.
func (m *DafsCi) Fuzz(ctx context.Context, source *dagger.Directory, seconds int) (string, error) {
	if seconds <= 0 {
		seconds = 60
	}
	return m.rustBase(source, nightlyImage).
		WithExec([]string{"cargo", "install", "cargo-fuzz", "--locked"}).
		WithExec([]string{"cargo", "fuzz", "run", "migrations", "--",
			fmt.Sprintf("-max_total_time=%d", seconds)}).
		Stdout(ctx)
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
		WithMountedDirectory("/src", source).
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
		{"fmt", func() error { _, e := m.FmtCheck(ctx, source); return e }},
		{"lint", func() error { _, e := m.Lint(ctx, source); return e }},
		{"test", func() error { _, e := m.Test(ctx, source); return e }},
		{"hermetic", func() error { _, e := m.Hermetic(ctx, source); return e }},
		{"rss-ceiling", func() error { _, e := m.RssCeiling(ctx, source); return e }},
		{"audit", func() error { _, e := m.Audit(ctx, source); return e }},
		{"private-refs", func() error { _, e := m.PrivateRefs(ctx, source); return e }},
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
