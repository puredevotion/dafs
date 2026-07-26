# Contributing

This repository is the source of truth. Issues and pull requests here are the
real thing, not a mirror of a private tree.

## Before you start

It is pre-alpha and empty. Opening an issue to ask whether something is planned
is more useful right now than sending a patch into a vacuum.

## Ground rules

**Hermetic builds.** The tree must build and test with no secrets, no network,
and no private DNS. A change that needs any of those to pass CI will be
rejected — anything that can't build in a clean container can't be deployed by
anyone but its author.

**It holds people's files.** The testing bar is higher than usual:

- new invariants need property-based tests, not just examples
- anything on a write path needs a crash-consistency test
- anything parsing bytes the user didn't type (documents, on-disk formats,
  network frames) needs a `cargo fuzz` target
- any bug that loses or corrupts a byte earns a permanent regression test

**Memory is a hard budget, not an aspiration.** RSS ceilings are asserted in
CI. A change that regresses them fails, and "it's only a few MB" is not an
argument — the budget exists because a filesystem has no business using
hundreds of megabytes.

**The AI never writes to your files.** Enrichment is additive and asynchronous.
A patch that lets AI output modify original bytes will not be merged.

**No corpus documents in the repo.** AI-quality benchmarks run against a golden
corpus referenced by content hash from a manifest. Never commit the documents
themselves — including as test fixtures.

## Pull requests

- one logical change per PR
- CI must be green; don't skip gates
- explain *why*, not just what — the what is in the diff

## Commit messages

[Conventional Commits](https://www.conventionalcommits.org/) (`feat(scope): …`,
`fix(scope): …`, `docs: …`, etc.). Version bumps and `CHANGELOG.md` are generated from
these by release-please — an inaccurate type or scope shows up in the next release's
changelog, not just in `git log`. A commit it can't parse isn't rejected there, it's
silently excluded from that decision — which is how `v0.0.1` shipped tagged by hand,
with no changelog entry.

Run `./scripts/install-hooks.sh` once per clone (or just `nix develop` — the devShell
does it for you) to get a `commit-msg` hook that rejects a non-conforming subject
locally, before it reaches CI. The `commit-lint` job in `ci.yml` / `ci/main.go` is the
same check server-side, for anyone who commits with `--no-verify` or without the hook
installed.

## Branches and merging

`main` takes no direct pushes — every change lands through a pull request.
Branch names follow what's already de facto true in this repo: `feat/*` for
milestone and feature work, `fix/*` for bug fixes, `release/*` for the rare
branch cut to stabilize a release. There is no permanent `develop` branch —
`feat/*` → PR → `main` is trunk-based already, and a second long-lived branch
would just be a second thing to keep in sync for no benefit at this size.

`.github/rulesets/main-branch.json` is the source of truth for what's actually
enforced (required status checks, no force-pushes, no branch deletion,
required PR).

## Commit signing

Commits must be signed (SSH or GPG) — enforced by the branch ruleset above.
SSH signing is the lower-friction option if you don't already have GPG set up:

```sh
git config gpg.format ssh
git config user.signingkey ~/.ssh/id_ed25519.pub  # or your existing key
git config commit.gpgsign true
```

GitHub needs the same public key added under **Settings → SSH and GPG keys →
New SSH key**, key type "Signing Key".

## Licence

Contributions are under [MIT](LICENSE). By opening a PR you agree your work can
be distributed under those terms.
