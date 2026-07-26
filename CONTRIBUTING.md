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
changelog, not just in `git log`.

## Licence

Contributions are under [MIT](LICENSE). By opening a PR you agree your work can
be distributed under those terms.
