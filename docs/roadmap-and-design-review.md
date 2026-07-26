# Roadmap and design review

Review of the original architecture proposal and roadmap, fixing three defects in the
delivery order and one in the testing bar. This does **not** re-propose the architecture —
the technology choices (Rust, FUSE, SQLite, Merkle-DAG chunking, QUIC, Kademlia) are sound
and adopted as-is.

**Scope:** milestone sequencing + testing strategy. Not a component design.

Issue numbers in the form `#NNN` refer to the original private planning issues and are
kept only as provenance; the substance is reproduced here in full.

---

## 1. Ground truth (read 2026-07-25)

- **#215 supersedes #214 §12.** #214 lists filesystem-first milestones (M1 FUSE, M2 CAS,
  M3 versions, … M9 metadata, M10 search); #215 reorders to AI-first (M01 timeline,
  M02 metadata, M03 search, … M06 FUSE, M07 CAS). #216–#230 implement **#215's** order.
  Any discussion of "milestone N" below means **#215/#216–230's numbering**, not #214's.
- **#215's own stated philosophy:** each milestone must `provide something a user can use`,
  be `independently testable, and fully testable, every path, scenario, integration.
  Performance/load, fuzzing, security, failure/chaosmonkey, CVE, Grype, opengrep/SAST,
  owasp zap, nuclei, dast`, and `Avoid building infrastructure without feedback`.
  **The re-cut below is derived from that philosophy, not imposed on it.** Every defect
  named is a place where #216–#230 contradicts #215's own bar.
- **Target hardware (#214 §4):** Linux/NAS/older i7, **no GPU**, Rust + FUSE + SQLite.
  Phase 2 Windows 11 CFAPI on i9. AI is CPU-only, Gemma-class 4B, <4GB RAM worker. Its
  `<500MB daemon idle` figure is **superseded by §8's budget** — 32 MB idle.
- **Repo state:** no filesystem/DAFS code exists (`find` for `*dafs*`/`*distributed*fs*`
  returns nothing). This is greenfield — the re-cut costs nothing to adopt today.
- **Deployment audience:** this is intended to be **deployed publicly by other users**, not
  only in this homelab. That constraint decides the identity model (§6 item 1) and makes
  the code public from day one.
- **Existing homelab constraint that bears on this plan:** every inter-service link is
  step-ca-chained mutual TLS, never one-shot. §3.1 is a blocker rather than a preference
  because of it — though the trust *root* differs here, see §6 item 1.

---

## 2. Locked decisions (this review)

1. **#214's architecture is adopted unchanged.** Merkle DAG chunking, Kademlia DHT
   discovery, QUIC transport, SQLite-then-maybe-HNSW vector storage, Rust, three
   independent data planes (filesystem / sync / AI), embeddings as derived-and-regenerable
   data, `AI output must never modify original files automatically`. None of that is
   in question.
2. **#215's AI-first phasing is right and is kept.** Starting with a read-only observer
   over the user's real filesystem — rather than a FUSE mount — is the single best
   decision in either issue. It produces user value before any risk to user data.
3. **Device identity moves out of M15 and becomes a gate on the first sync milestone.**
   See §3.1. This is the one change that is not negotiable on grounds of taste.
4. **Version history moves ahead of sync.** It is the highest-value milestone in the
   roadmap and it needs no distributed system. See §3.2.
5. **CAS moves after sync exists**, so that dedup and integrity have real data and a real
   transfer path to be measured against. See §3.3.
6. **FUSE moves to the end of the core sequence**, where it enables online-only hydration
   instead of substituting for a working observer. See §3.4.
7. **The test bar is defined once, globally, and applied per-milestone as a gate** — not
   restated as a per-milestone menu. See §5. #215's checklist is kept but is made
   surface-appropriate: DAST tools apply where there is an HTTP surface, and nowhere else.
8. Each milestone = its own feature branch + PR. Never commit to main.
9. **Standalone by default: every native/binary dependency is vendored, never a Nix
   requirement.** Established at M02a when pdfium (native PDF parsing, isolated in its own
   worker process — §3.4's FUSE reasoning applied to a hostile-input C++ library instead of
   an LLM) was first wired up via a Nix `buildInputs` addition, then corrected: `cargo build`
   alone, with no Nix and no network, must produce a fully working binary. The shared
   library is vendored into the repo and embedded into the binary (`include_bytes!`,
   extracted to a cache file on first run) — the same "commit the artifact, keep the Rust
   build hermetic" trade already made for `ui/dist/index.html` and for SQLite via
   rusqlite's `bundled` feature. Nix, where it's used at all, stays one optional packaging
   path (reproducible builds, the OCI image) and is never required to produce a working
   `dafs` binary. This applies to every future milestone that adds a native/binary
   dependency, not just pdfium — checked per-milestone against §7 item 5's size question,
   and against the unavoidable exceptions below:
   - **Resolved by not vendoring at all**: M02b's original shape (an in-process/child-process
     local model) would have hit this exception — multi-GB weights cannot be
     `include_bytes!`'d the way pdfium's ~tens-of-MB library can. M02b instead talks to a
     user-configured OpenAI-compatible endpoint (§7 item 5 has the resolved decision); dafs
     itself never runs or vendors a model, so this exception ended up moot for M02b. M08
     (AI assistant) reuses the same client and inherits the same answer.
   - **Cannot be vendored at all**: FUSE (M06/M11a/M11b) needs kernel-level support
     (a kernel module plus, on macOS, a separate macFUSE install) that no amount of
     bundling reaches; Windows CFAPI (M12) is a host-OS-provided API, not a library to
     ship. Both get documented as standalone-principle exceptions when their milestone
     lands, not treated as a violation of this one.

---

## 3. Defects in the current order

Four findings. #3.1 is a correctness problem; #3.2–#3.4 are value-delivery problems.

### 3.1 Security is M15; the first network sync is M09 — BLOCKER

#230 (M15) is where `device identity, encryption, key rotation, permissions` are
implemented. #224 (M09) is where two devices first exchange data over a network, and
#225 (M10) adds DHT peer discovery and multi-peer chunk exchange from untrusted providers.

That is six milestones of network protocol shipped before identity exists. There are only
two ways that resolves, and both are bad:

- M09–M14 run unauthenticated and unencrypted, and "don't sync anything real yet" becomes
  an undocumented operating constraint on a tool whose entire value proposition is holding
  real files; or
- M09 invents throwaway identity/crypto, which M15 then rips out — meaning the wire
  format, the pairing UX, and the peer-trust model all churn *after* three milestones have
  been built on top of them.

This also directly contradicts how everything else in this homelab is run: every
inter-service link is step-ca-chained mutual TLS, established at the point the link is
created, never bolted on afterwards. A P2P sync protocol with deferred identity is not a
topology that would be accepted here from any other component.

**Resolution:** device identity + pairing becomes its own milestone immediately *before*
the first sync milestone. Encryption-in-transit is part of the QUIC transport from its
first line of code (QUIC requires TLS anyway — there is no cheaper path). Key rotation and
fine-grained permissions may stay late; **identity and transport encryption may not.**

What remains in the final hardening milestone: key *rotation*, permission model, backups,
repair tooling, migrations, long-running soak. Those are genuinely last-mile.

### 3.2 Version history (M08) is the roadmap's strongest milestone and is 8th

#223 (M08) delivers `"Recover anything I changed."` That is a complete, legible,
single-device user win — it needs no FUSE, no CAS, no network, no second device. It works
directly on the watched filesystem from M01: on each observed modify event, capture the
prior content, store it, expose restore.

Everything ahead of it in the current order is either weaker (M02, M04) or vastly harder
(M06, M07). A user who installs at M04 today gets search and a graph; a user who installs
at M04 under the re-cut gets search *and cannot lose work*. The second is a much stronger
reason to keep the tool installed, which is the only thing that generates the feedback
#215 says it wants.

Note this does **not** require CAS. Whole-prior-content copies plus a dedup pass later
(M09 in the re-cut) is strictly simpler and is the correct order: ship the value, then
make it cheap. Building CAS first to make versions efficient is exactly the
`infrastructure without feedback` #215 warns against.

### 3.3 CAS (M07) delivers no observable value where it sits

#222's stated value is `deduplication, integrity, foundation for sync`. Assessed honestly:
dedup is invisible to a user, integrity is invisible until the day corruption happens, and
`foundation for sync` is infrastructure by its own admission. At position 7 it has no
sync to be a foundation *for* — that arrives two milestones later.

Placed *after* sync exists, all three become measurable:
- dedup → "sync transferred N bytes instead of M" — a real number on a real transfer
- integrity → verification on a real cross-device transfer, not a synthetic corruption test
- the retrofit itself is a strong test of whether the storage abstraction was drawn
  correctly, which is `learning for later architecture decisions` per #215's philosophy

### 3.4 FUSE (M06) is a substitution, not an addition

#221's user value is `"Existing applications can use the system."` But existing
applications can already use the user's files — the files are in a real filesystem, and
M01–M05 observe them without moving them. After M06, the same applications reach the same
files through a FUSE mount that can now fail, deadlock, or lose data.

M06 is the highest-risk milestone in the roadmap (FUSE write paths, POSIX semantics,
mmap, crash consistency) and, positioned 6th, it is the lowest-value. Nothing that
requires the VFS exists at M06.

It becomes a genuine addition once selective sync exists: **online-only files that hydrate
on open, through unmodified applications.** That is something the user cannot do any other
way, and it is the actual reason to pay FUSE's cost.

Its exit criterion also needs replacing. `The filesystem feels native` is not testable.
See §5.3.

### 3.5 On "timeline and graph come too late" — partially rejected

Timeline is **M01** (#216) and graph is **M04** (#219). In sequencing terms they are as
early as they could be, and no reordering is warranted.

The real defect is depth, not position:

- **#214 §9 states the timeline is `the primary historical view`.** #216 ships a flat
  reverse-chronological event list (`10:30 Modified architecture.md`) and no later
  milestone revisits it. The event *store* is load-bearing for M08 versions, M09 sync
  (`Synchronization unit: Events. Not files.`), and M15 observability — but the event
  *view* never grows past a log tail. It never becomes the primary view #214 promises.
- **#219's graph has no surface at all.** Its own test questions —
  `Show all documents related to project X`, `Which files mention person Y?` — are both
  answerable by M03 semantic search plus a metadata filter. A milestone whose acceptance
  tests are satisfiable by the *previous* milestone cannot demonstrate its own value.
  The graph earns its place when it is *navigable* (walk file → person → project → other
  files, and pivot the timeline by any node), not when the `entities`/`relations` tables
  are populated.

**Resolution:** keep both where they are; change what they must deliver. M01 keeps the
flat list — correct for a first milestone. The graph milestone gains a navigation surface
and a timeline pivot as its exit criteria, and moves *after* sync so that relationship
extraction runs over a corpus spanning more than one device.

---

## 4. Re-cut sequence

20 milestones (M00–M15, four of them split in halves). No scope added, no scope dropped
relative to #215's 15. Splits are along **user-visible capability**, never along
architecture layer — a split that yields two units with no user value each is the
`infrastructure without feedback` failure at finer grain.

Sizes are solo-dev evenings/weekends estimates, for ordering purposes only.

| New | Was | Milestone | Size | Note |
|-----|-----|-----------|------|------|
| **M00** | — | **Walking skeleton + publishing model** | 1 wk | New. Daemon + SQLite migrations + empty HTTP API + UI shell + **the whole CI gate** (§5.2) wired end-to-end on one trivial feature, **in this public repo**. Also carries the publishing/consumption wiring for downstream deployers. The one legitimate no-user-value milestone: the CI bar must exist before M01's first real code, not be retrofitted at M15. |
| M01 | M01 | Local timeline | 3–5 wk | Unchanged. Read-only observer, no risk to user data. |
| **M01a** | — | **Ship it: install, monitor, update** | 1 wk | New. Installer script, release automation (release-please + Dagger build/SBOM parity), a read-only status TUI, and `dafs self-update`. Unlike M00, this is genuine user-facing value — installing without a `cargo build` and watching the daemon run are things a user directly does — not infrastructure-for-its-own-sake. Gated here because a user who can't install or observe M01 has no reason to still have it installed by M02a. |
| **M02a** | M02 | **Deterministic metadata + browse** | 2 wk | Split. PDF text, Office, EXIF, git, filename/FS metadata. **No LLM.** Gains its own surface (§4.1). |
| **M02b** | M02 | **LLM enrichment via an OpenAI-compatible client** | 3 wk | Split. Summary, keywords, entities, classification. Originally scoped as an in-process local model (Gemma-4B, CPU-only); resolved instead to a thin client against a user-configured OpenAI-compatible endpoint — see §7 item 5 for why. De-risks the project's biggest unknown — usable enrichment quality/latency with no model dafs manages itself — *after* M02a already ships value. |
| M03 | M03 | Semantic search | 3–4 wk | Unchanged. First "wow" milestone. |
| **M04** | **M08** | **Version history** | 2–3 wk | ↑4. Strongest single-device value; needs no FUSE/CAS/network. §3.2 |
| **M05** | **(M15)** | **Device identity + pairing** | 2 wk | Extracted from M15. Gate on all networking. §3.1 |
| **M06a** | **M09** | **One-way replication** | 4 wk | Split. Laptop → NAS, single writer, no conflicts. Value: *"my files are backed up."* |
| **M06b** | **M09** | **Bidirectional + conflicts** | 5 wk | Split. Value: *"edit anywhere."* Conflict resolution is where the hard bugs live; isolating it gives the property tests (§5.3) a known-good baseline to diff against. |
| **M07** | **M04** | **Knowledge graph as navigation** | 4–5 wk | ↓3. Gains a navigable surface + timeline pivot; runs over a multi-device corpus. §3.5 |
| M08 | M05 | AI assistant | 3–4 wk | ↓3. Retrieval richer with graph + cross-device history behind it. |
| **M09** | **M07** | **CAS retrofit** | 4–6 wk | ↓2. Dedup/integrity now measurable against a real transfer path. §3.3 |
| M10 | M11 | Selective sync | 3–4 wk | ↑1. Needs M09 CAS for chunk-level hydration. |
| **M11a** | **M06** | **Read-only FUSE + hydration** | 4 wk | Split. ↓5. **No write path = no data-loss risk.** Value: open any file incl. online-only, through any app. §3.4 |
| **M11b** | **M06** | **FUSE write path** | 6 wk | Split. POSIX conformance, mmap, fsync, rename-over-write, crash consistency. pjdfstest/xfstests gate applies here only. |
| M12 | M12 | Windows integration | 6–8 wk | CFAPI placeholders need M10 policies + M11a hydration. |
| M13 | M13 | Mobile application | 6–10 wk | Unchanged. |
| **M14a** | **M10** | **Multi-peer distribution** | 5 wk | DHT discovery, parallel chunk fetch. Value: *"faster."* |
| **M14b** | **M14** | **Erasure coding** | 5 wk | Availability-aware placement. Value: *"survives device loss."* |
| M15 | M15 | Production hardening | ongoing | Reduced: key rotation, permissions, backups, diagnostics, repair, migrations, soak. Identity left at M05. |

Max milestone drops from 12 wk to 6 wk; median stays ~4 wk. Total ≈ 18 months of
evenings — the number worth reacting to, and the reason the first five months now contain
four independently useful wins (M02a, M04, M06a, and M03).

**Release cadence is independent of milestones.** Tag a usable build every two weeks
regardless of milestone boundaries. Milestones mark *capability*; releases mark *time*. A
half-finished M06b behind a feature flag still ships M06a's value.

Dependency chain (only the non-obvious edges):

```
M00 skeleton ─> M01 event store ─┬─> M02a metadata ─> M02b LLM ─> M03 search ─> M07 graph ─> M08 assistant
                                 ├─> M04 versions ──────────────┐
                                 └─> M06a one-way sync ─────────┤
M05 identity ───────────────────────> M06a ─> M06b bidirectional┴─> M09 CAS ─> M10 selective ─> M11a RO-FUSE ─> M11b RW-FUSE
                                                                                M11a ─> M12 Windows
                                                                                M06b ─> M13 mobile
                                                                                M09  ─> M14a P2P ─> M14b erasure
```

### 4.1 M02 needs its own surface

#217's exit criterion is `Searching metadata provides value without opening documents` —
but search is M03. As specified, M02 ships a populated SQLite table and nothing a user
can look at, which fails #215's `provide something a user can use`.

Fix without adding scope: the M01 timeline UI already exists. M02 extends it — each event
row expands to show extracted metadata (document type, author, language, entities, topics,
summary), and the timeline gains faceted filtering by those fields. Structured filtering is
not semantic search; it is a strictly smaller thing that M02's own extractors already
support, and it makes M02 independently useful and independently testable.

Revised exit criterion: *a user can filter and skim their timeline by extracted document
properties, and read a summary, without opening any file.*

### 4.2 P2P and erasure coding stay separate

An earlier draft merged #225 (P2P) into #229 (erasure coding) to free a slot for M05
identity. With the roadmap no longer pinned at 15 milestones that merge is unnecessary and
is withdrawn: they are distinct user promises — *"faster"* (M14a) vs *"survives device
loss"* (M14b) — and splitting them keeps each under 6 weeks.

Two-device sync (M06a/M06b) does not need the DHT — #224 already specifies `Initial:
direct connection`, `Later: DHT discovery`. That deferral is correct and is preserved:
direct connection + M05 pairing carries M06, and the DHT first appears at M14a.

---

## 5. Test bar

### 5.1 What #216–#230 actually specify today

Audited against #215's stated requirement (`fuzzing, security, chaos, CVE, Grype,
opengrep/SAST, ZAP, nuclei, DAST` per milestone):

| Class | Milestones covering it |
|---|---|
| Performance / load | #216 (1M files), #217 (time+mem), #218 (latency+CPU), #230 (soak) |
| Chaos / failure | #224 (interrupted transfer), #225 (provider vanishes), #229 (device loss, partitions, corrupt peers), #230 (device failures) |
| Security | #229, #230 only |
| **Fuzzing** | **none** |
| **SAST / CVE / Grype / opengrep** | **none** |
| **DAST / ZAP / nuclei** | **none** |

Zero of fifteen milestones meet #215's own bar. Six of fifteen (#219, #220, #221, #222,
#223, #226, #227, #228) specify no measurable test criteria at all — #221's is
`The filesystem feels native`, #222's is a three-item bullet list with no method.

### 5.2 The bar, defined once

Applied as a merge gate on every milestone PR. Restating it per-milestone is what caused
the coverage table above; it lives here instead.

**Per-milestone, always:**
1. Unit + **property** tests for every new invariant. For a filesystem the invariants are
   the product; example-based tests alone will not find the bugs.
2. **Crash consistency**: fault injection at N points across every write path, then assert
   *no data loss and no unreadable state*. SQLite metadata + object store + (later) FUSE
   writeback is three-way state that can tear. Nothing in #216–#230 tests this today, at
   any milestone.
3. **`cargo fuzz` target for every parser that touches bytes the user did not type.**
   Concretely: PDF/Office/EXIF extractors (M02 — attacker-supplied documents), on-disk
   metadata and object formats (M04, M09), and **network-supplied Merkle metadata and
   peer frames (M06, M14)**. The network parsers are the highest-severity surface in the
   entire system and are currently first tested at #229.
4. **Data-loss regression suite, cumulative.** Every bug that ever loses or corrupts a
   byte gets a permanent test. For a filesystem this is the only suite that truly matters,
   and no milestone currently defines it.

**CI-wide, every PR (pipeline config, not per-milestone work):**
`cargo audit` + `cargo deny` (advisories, licences, duplicate crates), Grype on any
produced image, opengrep/SAST on the Rust tree. #215 asks for CVE/Grype/SAST per
milestone; the correct implementation is one pipeline that no milestone can bypass.

**Surface-appropriate, not universal:**
DAST (ZAP, nuclei) needs an HTTP surface. In this system that is exactly three: the M01
timeline API, the M03 search API, and the M08 assistant API. Listing DAST against M04
version history or M09 CAS produces a checkbox nobody can honestly tick. Scope it to
those three, and require it there.

### 5.3 Milestone-specific tests the current specs are missing

- **FUSE (new M11b) — POSIX conformance, not app smoke-testing.** #221 says test with
  `editors, browsers, media players`. That is manual smoke. Real editors do
  rename-over-write, `fsync`, `O_TMPFILE`, mmap, hardlinks, xattrs, and sparse writes;
  each is a distinct way to lose a file. Require **pjdfstest** and the applicable
  **xfstests** generic suite as a gate, plus explicit mmap and `fsync`-durability cases.
  Replace `The filesystem feels native` with: *pjdfstest and the generic xfstests subset
  pass; online-only files hydrate on first read through unmodified applications; a
  `kill -9` of the daemon mid-write leaves no partial or unreadable file.*
- **Sync (new M06a/M06b) — property-based convergence, not four hand-written cases.** #224 lists
  offline edits, reconnect, conflict, interrupted transfer. Those are four points in a
  space of arbitrary event interleavings. Require **deterministic simulation** over
  randomized interleavings, partitions, and clock skew (madsim / turmoil style), asserting
  convergence and no-loss, with a seed recorded on every failure so it replays. Four cases
  will not find the bugs that matter.
- **AI (M02b, M08) — a committed golden corpus.** #217 and #220 both list
  `hallucination rate` as a metric with no dataset defined; as written it is unmeasurable
  and will be reported as a vibe. The corpus is **real documents, grown over time** (§8),
  with the metric computed against it in CI. #218's `relevance` needs the same treatment.
- **Prompt injection (M02b, M08).** Both feed arbitrary user documents into an LLM. A
  document containing instructions is an attack surface, and #214's own rule — `AI output
  must never modify original files automatically` — is the correct mitigation but is
  tested nowhere. Require a corpus of adversarial documents attempting to induce writes,
  exfiltration via retrieved context, and tool misuse; assert the rule holds.
- **Untrusted peer behaviour (M06a onward, M14a).** #229 tests `corrupted peers` at the
  second-to-last milestone. A peer that lies about chunk availability, serves
  hash-mismatched data, or floods malformed frames must be handled from the *first*
  networked milestone.
- **Resource ceilings as tests, not aspirations.** The budget is §8's, not #214's — see
  that section for why `<500MB RAM idle` is rejected as a target. Assert the per-milestone
  RSS ceiling in CI from M00, so a regression fails a PR rather than being discovered on
  the NAS.

---

---

## 6. Resolved decisions

The five open questions from the first draft of this document, now answered.

1. **Identity trust root: Syncthing model, not step-ca.** Device identity is
   self-contained — each device generates its own keypair at first run, the device ID is a
   hash of the public key, and pairing is out-of-band (show/scan ID, confirm on both
   sides). No CA, no external trust root, no enrolment server. QUIC's TLS layer uses those
   device certificates directly, with peer acceptance driven by the local
   introduced-devices list rather than by chain validation.

   This is the first component here deliberately **not** on step-ca, and that is correct:
   the project is intended to be deployed publicly by other users who have no CA and no
   homelab. A design requiring step-ca would be undeployable for its actual audience. The
   homelab's `mTLS everywhere, never one-shot` rule is still honoured in substance — every
   link is mutually authenticated and encrypted from M05 onward — just with a different
   trust root than the rest of the fleet.

2. **M04 version storage before CAS: accepted as-is until M09.** Whole prior-content
   copies, no dedup, for five milestones. Mitigation is a retention cap rather than early
   CAS: bound version history by per-file count and total bytes, with a size threshold
   above which a file is versioned by reference-only (record the event, skip the copy).
   M09's retrofit then backfills dedup over whatever accumulated.

3. **Golden corpus: real documents, grown incrementally.** Not synthetic. It starts small
   (tens of documents at M02a) and grows as real failure cases are found — every
   extraction bug or bad answer that ever ships contributes its document plus the expected
   output, exactly like the cumulative data-loss suite in §5.2.

   Because the code is public (item 4), the corpus **cannot live in the public tree**. It
   lives outside it — NAS-hosted, referenced by content hash from a manifest that *is*
   public — so the benchmark is reproducible for anyone with the corpus and the manifest
   pins exactly which revision a result came from. Any document contributed by a public
   user needs an explicit licence/consent note in the manifest.

4. **Code is public from day one, in a standalone public repo — not a subtree mirror of
   this one.** The repo is source of truth; this repo consumes it as a pinned flake input.
   A private deployer consumes this repo as a pinned dependency; the dependency
   direction is inward only, so nothing here needs access to any private environment.

5. **Memory: `<500MB RAM idle` is rejected as a target.** See §8 for the replacement
   budget and the techniques that make it reachable.

---

## 7. Remaining open questions

1. **Licence choice** for the two new public repos, and consistency with whatever the nine
   related tools already ship.
2. **Vector index engine at M03.** SQLite vector extension (per #214) keeps the
   one-database property; a separate index (usearch/hnswlib via FFI, or tantivy for
   lexical) is faster but adds a second store to keep consistent. §8's binary quantization
   is achievable either way. Benchmark at M03 against the memory ceiling, decide then.
3. **Erasure coding parameters (M14b).** #229 says `3 logical replicas across 2 physical
   locations`. With three devices and one NAS, erasure coding may not beat plain
   replication on either space or durability until the fleet is larger. Verify the maths
   before building it; M14b may reduce to availability-aware placement only.
4. **Windows memory story (M12).** §8's budget is measured on Linux. CFAPI placeholder
   hydration has its own working-set behaviour and no `madvise` equivalent. Needs its own
   ceiling, set at M12.
5. **LLM model distribution (M02b/M08) — resolved at M02b.** §2 item 9's vendoring
   principle explicitly could not apply here the way it does to pdfium: a Gemma-class 4B
   model's weights are gigabytes, not tens of megabytes, and `include_bytes!`-ing that into
   a committed binary would make every clone, CI run, and release artifact carry a
   multi-gigabyte payload whether or not enrichment is ever used. Rather than solve that
   (a runtime fetch-and-cache-once was the leading candidate, with its own first-run-needs-
   network deployment question for the NAS/homelab target hardware), M02b sidesteps it: dafs
   never runs or vendors a model at all. It is a thin client against a user-configured
   OpenAI-compatible chat-completions endpoint — a local llama.cpp/Ollama/vLLM server, or a
   hosted API — so the model, wherever it runs, is entirely outside dafs's build and
   deployment story. This also fits the architecture better than embedding ever would: the
   AI plane (§2 item 1's three independent data planes) can now genuinely run on different,
   more capable hardware than the NAS running dafs itself. M08 (AI assistant) reuses the
   same client.

---

## 8. Memory budget

**#214's `<500MB RAM idle` daemon target is rejected.** A filesystem daemon that idles at
half a gigabyte is not deployable on the stated target hardware alongside anything else,
and it is far above what the workload actually requires. It is replaced by a per-component
budget, asserted in CI from M00 (§5.2).

### 8.1 The budget

Measured as **RSS on Linux, 1M-file corpus, after a full scan has completed and the
process has been idle for 60s.**

| Component | Ceiling | Notes |
|---|---|---|
| Daemon idle (watcher + event store + API) | **≤ 32 MB** | The baseline. No index resident. |
| Daemon, steady-state with search served | **≤ 96 MB** | Includes quantized vectors (§8.3). |
| Peak during initial 1M-file scan | **≤ 128 MB** | Bounded by streaming, not corpus size (§8.2). |
| Enrichment client, resident while a request is in flight | **negligible** | §7 item 5: M02b never runs a model — it's an HTTP client (`ureq`) against a user-configured endpoint. No weights, no inference engine, in this process ever. |
| Enrichment client, idle | **0** | No worker thread at all when unconfigured (§7 item 5) — not merely a separate process that exits, no process/thread exists to begin with. |

#214's original `≤4 GB` "AI worker, resident while enriching" figure no longer applies to
this process: the model, if any, runs on whichever machine the user pointed the enrichment
client at, which may not even be this host. #214 already separates the AI pipeline as its
own plane — M02b's client-only design makes that separation a memory boundary too, more
completely than the original in-process-worker design would have, since there is now no
resident model to bound at all on the machine running dafs.

### 8.2 Reaching it — scan and event path

The initial scan of 1M files is the classic large-input, small-state problem, and the
1BRC results transfer directly:

- **Stream, never accumulate.** Bounded channels between walker → hasher → writer, with
  backpressure. Peak memory is the channel depth, independent of corpus size. This is what
  makes the 128 MB scan ceiling hold at 10M files too.
- **mmap the read path** for hashing/extraction and let the kernel own the page cache.
  Pages are evictable and counted as cache, not anonymous RSS. This is precisely 1BRC's
  central technique, and it is the difference between the OS reclaiming memory under
  pressure and the daemon getting OOM-killed.
- **One arena per worker thread, merge at the end.** 1BRC's clearest lesson (per-thread
  hashmaps beat one shared concurrent map, by 29s on the contest server) applies to the
  scan's per-directory aggregation.
- **Don't store `String` per path.** 1M paths at ~80 bytes averages 80 MB in `String`s
  before any structure. Intern path *components* into a single arena and represent a path
  as a parent-id + component-id pair — the tree shape means components repeat heavily.
  This alone is most of the difference between the 32 MB target and a naive 200 MB+.
- **SWAR/SIMD parsing** where extraction is bytewise (delimiter scanning, EXIF/header
  fields). Lower priority than the above — it buys time, not memory — but the same
  8-bytes-at-a-time technique applies.
- **Allocator choice is load-bearing.** glibc's per-thread arenas fragment badly on a
  many-thread scan-then-idle workload, and RSS never returns after the scan. Use jemalloc
  with aggressive `dirty_decay_ms`/`muzzy_decay_ms` (background purge returns pages to the
  OS), or mimalloc. Cap `MALLOC_ARENA_MAX` if glibc is unavoidable. **Assert post-scan RSS
  returns to the idle ceiling** — that assertion is what catches fragmentation, and it is
  the test most likely to fail early.

### 8.3 Reaching it — index and storage

- **SQLite: small page cache, large mmap.** Set `cache_size` low (single-digit MB) and
  `mmap_size` large. Mapped pages are file-backed and evictable; page-cache pages are
  anonymous and are not. Same data, different accounting, and on an SSD the difference in
  latency is small — which is exactly the "SSDs are quick" tradeoff, spent in the right
  direction. WAL mode, `synchronous=NORMAL` (`FULL` on the version/object store).
- **Binary-quantize the vectors.** 1 bit per dimension instead of 32 is a **32× reduction**
  in resident vector memory; keep only the 1-bit vectors and the graph resident, page the
  full-precision floats from disk, and rescore an oversampled candidate set (e.g. fetch
  2–4× the requested `k`) against the floats. Published results put this at roughly a 2%
  recall cost — and recall is measurable against the golden corpus (§6 item 3), so the
  tradeoff is verifiable rather than assumed. For 1M documents × 384 dims this is the
  difference between ~1.5 GB and ~48 MB resident, and it is what makes the 96 MB
  steady-state ceiling arithmetically possible at all.
- **Compression: zstd with a trained dictionary for small objects, plain zstd for large.**
  Small-object compression is where a shared dictionary matters most, and this corpus is
  dominated by small text/metadata records. Train the dictionary offline from a corpus
  sample, version it, and store the dictionary ID with each object — a dictionary is not
  reconstructible after the fact, so treat it as immutable once objects reference it.
  Prefer LZ4 only where a measured latency budget rules zstd out; zstd's ratio advantage
  (roughly 15–25% at comparable settings) is worth 2–4× encode cost on a write-once,
  read-many object store. Applies from M09 (CAS), which is where objects first exist.
- **Compress the event log.** It is append-only, highly repetitive, and grows without
  bound — the single best dictionary-compression target in the system.

### 8.4 Where this lands in the roadmap

Memory work is **not** a separate milestone; a milestone with no user value that exists to
make earlier milestones smaller is the trap §4 avoids. Instead:

- **M00** establishes the RSS assertion harness and picks the allocator. Cheapest possible
  point, and it makes every later milestone's regression visible on its own PR.
- **M01** must hit 32 MB idle / 128 MB scan with the streaming + path-interning design.
  Retrofitting path interning after M07's graph depends on path IDs is expensive; do it now.
- **M03** must hit 96 MB steady-state, which requires binary quantization in its first
  implementation. This is the one place where a memory technique is a *functional*
  requirement rather than an optimization — full-float resident vectors cannot meet the
  ceiling at 1M documents, so the quantize-and-rescore path is the design, not a later tuning.
- **M09** adds compression when objects first exist.
- **M15** is where sustained-soak memory behaviour (fragmentation over months) is proven.

---

## 9. Research inputs

Named as inputs to specific milestones so they inform design rather than sitting as a
reading list.

- **1BRC (One Billion Row Challenge)** — mmap, SWAR/SIMD parsing, per-thread maps merged
  at the end, avoiding `String` in hot paths, branchless parsing. Input to **M01** scan
  design (§8.2).
  [morling.dev](https://www.morling.dev/blog/one-billion-row-challenge/) ·
  [curiouscoding.nl](https://curiouscoding.nl/posts/1brc/) ·
  [Rust walkthrough](https://aminediro.com/posts/billion_row/) ·
  [12 lessons](https://foojay.io/today/12-lessons-learned-from-doing-the-one-billion-row-challenge/)
- **IETF QUIC WG** — current state as of 2026-07: **RFC 9221** datagrams is published;
  **multipath** (`draft-ietf-quic-multipath-21`) is in the RFC Editor queue; **ACK
  frequency** (`-14`) has WG consensus; **reliable stream reset** (`-09`) is in last call
  to 2026-08-03; **qlog** schemas are WG documents; **address discovery** has *expired*.
  Input to **M06a** transport choice. Practical read: build on a QUIC implementation
  tracking these (quinn/s2n-quic), adopt datagrams where useful, treat multipath as
  near-term-stable, and **do not** design around address discovery.
  [datatracker](https://datatracker.ietf.org/wg/quic/documents/)
- **Compression** — zstd dictionary training for small objects; LZ4 only under a measured
  latency budget. Input to **M09**.
  [facebook/zstd](https://github.com/facebook/zstd) ·
  [LZ4 vs zstd](https://www.oreateai.com/blog/lz4-vs-zstd-decoding-the-compression-conundrum-for-your-data/1a1a74dabc2873bcb2acfc2dd9418d41) ·
  [ZFS practice](https://datazone.de/en/aktuelles/zfs-komprimierung-speichereffizienz-performance/)
- **Vector search under a memory ceiling** — binary quantization (32× reduction),
  oversample-then-rescore against full-precision vectors, ~2% recall cost; 8-bit rotational
  quantization as the middle option. Input to **M03** (§8.3).
  [Qdrant BQ](https://qdrant.tech/articles/binary-quantization/) ·
  [Weaviate 32×](https://weaviate.io/blog/binary-quantization) ·
  [Elastic recall measurement](https://www.elastic.co/search-labs/blog/recall-vector-search-quantization) ·
  [8-bit rotational](https://weaviate.io/blog/8-bit-rotational-quantization)
- **Allocators / RSS** — jemalloc background purge and decay tuning vs glibc arena
  fragmentation; `MALLOC_ARENA_MAX`. Input to **M00** (§8.2).
  [allocator comparison](https://beefed.ai/en/choose-memory-allocator-jemalloc-tcmalloc-mimalloc) ·
  [jemalloc in Rust](https://dev.to/leapcell/optimizing-rust-performance-with-jemalloc-36lo)
- **SQLite memory tuning** — `mmap_size` vs `cache_size` accounting, WAL. Input to
  **M01**/**M03** (§8.3).
  [SQLite mmap docs](https://www3.sqlite.org/mmap.html) ·
  [phiresky tuning](https://phiresky.github.io/blog/2020/sqlite-performance-tuning/)

**Still to research** (not yet done, flagged rather than assumed): content-defined chunking
parameter selection (FastCDC vs Gear, chunk-size distribution vs dedup ratio) before M09;
CRDT vs version-vector conflict models before M06b; FUSE-vs-`io_uring`/FUSE-passthrough
performance on modern kernels before M11a.

---

