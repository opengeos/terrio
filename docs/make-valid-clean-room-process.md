# Tile MakeValid: research, provenance and clean-room process

For the executed whole-agent isolation and independent implementation, see
[the implementation record](make-valid-implementation.md). The earlier package
validation below covers command isolation only and is retained as history.

Research date: 2026-10-29. Baseline:
`0281ec0add52008c051cd024ab9a23ca855055d7`.

**Audience:** maintainer, specification reviewer and integration/provenance
custodian. This file is deliberately outside the initial implementer package.
The [implementer specification](make-valid-clean-room-spec.md) contains the
behavioral handoff; the paired JSON and collector contain independent-input
reference evidence. No replacement is implemented or authorized to be described
as provenance-cleared by these documents.

## Findings and exact replacement boundary

The deferred function is
`crates/terrio/src/tiles/valid.rs::make_valid`, starting at line 497 in this
baseline. Its repair section begins at line 80. The production caller is
`tiles/vector.rs::encode_polygons`, around lines 651–672: it validates the
already quantized polygons, invokes repair only if invalid, clears provisional
commands and re-encodes the returned polygonal result using truncation.
`tiles/mod.rs` declares the module. `tiles/validity.rs` supplies the separate
JTS-derived validity checker and imports the shared coordinate/polygon aliases
through `valid`.

The small entry point has a larger private dependency footprint. Removing only
the named function or renaming its module would not replace the implementation.
The custodian's retirement inventory must include:

| Area | Required audit/action |
| --- | --- |
| All repair code below the line-80 marker | Retire together, including private graph, area-building, traversal, containment and ordering helpers; do not transfer this structure to the implementer |
| Prefix of `valid.rs` | Audit `Env`, `orient`, `in_box`, `box_pairs`, imports and type aliases; some repair dependencies lie before the marker |
| `box_pairs` | Also called by `vector.rs::ring_crossings`; preserve its needed behavior through an audited existing helper or an independent replacement, rather than silently deleting a live caller |
| `validity.rs` import | Retain access to necessary shared types, preferably through a reviewed neutral type module or compatibility shim |
| Tests in `valid.rs` | Audit independently of implementation; do not hand unreviewed legacy tests to the fresh implementer |
| Retained/indirect paths | Verify no new repair path calls the retired implementation or carries a translated helper under another name |

The repository's root and crate `NOTICE`, and `docs/jts-provenance.md`, explicitly
exclude this deferred repair from the recent JTS replacement. The relevant local
history contains introduction commit
`f509340e31f7d5753a3b66e9dec80c626ddc7889` (tile archive work, #78) and the
baseline JTS replacement commit above. That history identifies audit targets;
it does not by itself identify every upstream source revision or license term.
[Pinned provenance record](https://github.com/opengeos/terrio/blob/0281ec0add52008c051cd024ab9a23ca855055d7/docs/jts-provenance.md),
[pinned NOTICE](https://github.com/opengeos/terrio/blob/0281ec0add52008c051cd024ab9a23ca855055d7/NOTICE).

Terrio's public `ops/topology.rs::make_valid` is different: its invalid polygon
path uses the `geo` crate and has separately documented behavior. The CLI,
Python and JavaScript public geometry-repair operations are not the target.
Replacing those APIs would leave the deferred tile implementation in place.

GEOS is free/open-source software under the LGPL; “restrictive” here refers to
the project's wish to avoid carrying those conditions in this implementation,
not a claim that GEOS is proprietary. A port does not obtain MIT/EDL provenance
merely by being written in Rust or placed under a project-wide notice.
The exact upstream file/revision/license and distribution implications of the
old port remain a separate maintainer audit; this work did not inspect GEOS
implementation source to settle them. [GEOS's own licensing statement](https://libgeos.org/).

## What clean-room practice establishes

The practical aim is evidence of independent expression: preserve the required
behavior while preventing old implementation expression from becoming new code.
Clean-room separation is an engineering practice, not a universal legal test.

| Primary source | Finding and relevance |
| --- | --- |
| [17 USC §102(b), official 2024 edition](https://www.govinfo.gov/content/pkg/USCODE-2024-title17/html/USCODE-2024-title17-chap1-sec102.htm) and [Copyright Office Circular 61, p. 1](https://www.copyright.gov/circs/circ61.pdf) | U.S. law distinguishes program expression from underlying functionality, processes and methods. Independently state behavior; do not assume a translation or source-shaped specification is harmless. |
| [Sega v. Accolade, court opinion](https://www.law.berkeley.edu/files/sega.pdf), 977 F.2d 1510, 1520–28 (9th Cir. 1992) | The court accepted intermediate disassembly as fair use on the compatibility facts before it. The interface-documentation boundary is instructive; the holding is not blanket permission to reverse engineer. Documentation and ordinary black-box calls suffice here. |
| [Google v. Oracle, Supreme Court opinion](https://www.supremecourt.gov/opinions/20pdf/18-956_new_0e04.pdf), 593 U.S. 1 (2021) | The Court assumed copyrightability for its analysis and found fair use in that case. It did not categorically declare every API uncopyrightable. Preserve necessary signatures without copying implementation expression. |
| [EU Directive 2009/24/EC, Arts. 1, 5, 6, 8](https://eur-lex.europa.eu/legal-content/EN/ALL/?uri=CELEX%3A32009L0024) | Entitled observation/testing and indispensable interoperability decompilation have distinct conditions. Other rights/rules can still matter. Do not treat publicly accessible code as unconditional permission. |
| [SAS Institute v. World Programming, C-406/10, official judgment](https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX%3A62010CJ0406), especially paras. 39–46, 50–70 | The Court distinguishes functionality from expression and separately recognizes protected expression in manuals. Write original prose and examples; cite documentation rather than transplant its presentation. |
| [Asahi Linux copyright/reverse-engineering policy](https://asahilinux.org/copyright/) | A first-party engineering precedent for separating source analysis from implementation, tracking authorship and rejecting direct translation. Its policy explicitly treats two-team separation as neither mandatory nor an absolute guarantee. |

These sources concern different jurisdictions and facts. The procedures below
are a synthesis for this project, not a legal opinion, a finding about earlier
Terrio distributions, or a guarantee of noninfringement. A new independent
implementation would not retroactively settle past distribution obligations.

## Research method and source-exposure declaration

The primary analyst inspected the legacy module's prefix and source snippets,
private declarations/comments and symbol references while locating its boundary.
The analyst also read caller/adaptor code, repository notices, public docs and
test infrastructure. The analyst is therefore **source-exposed for this repair**
and must not implement the replacement in this conversation or a child context
that inherits it. No assertion of having avoided all legacy expression is made.
No upstream GEOS implementation was opened for this task.

A separate research agent studied copyright authorities and official process
guidance without inspecting Terrio or GEOS implementation source. It subsequently
reviewed only the handoff spec, original collector and observations. Its review
is a useful process check, not a legal clearance or a replacement for maintainer
sign-off. Record any further reviewer exposure accurately.

Behavioral input sources for the implementer spec are the supplied public API
contract, the tile caller/type interface, independently authored small numeric
inputs, and public executable output. The spec intentionally contains no old
repair pseudocode, graph layout, helper sequence, private constants or ordering
recipe. Necessary type/signature names are included for integration only.

The original-input collector calls Shapely's public API and never imports
Terrio or reads its repair implementation. It constructs each feature as a
MultiPolygon and selects linework explicitly. The capture holds **51** cases,
with complete reference result WKB, polygonal projection, input arrays, version
metadata, collector hash and loaded GEOS binary hashes. It was run in the
host queue using the existing scratch validation environment. No source build,
Terrio runtime comparison or replacement-code test was run for this research.

Several observations constrain plausible substitutes: duplicate shells retain
area; crossing overlaps and coincident-edge overlaps differ; a hole sharing a
shell edge does not simply become a notch. Therefore a generic XOR fill or a
shell-union repair cannot be assumed equivalent. Fractional output also has to
survive the tile writer's truncation semantics. These are numeric observations,
not extracted instructions on how the old implementation achieves them.

## Recommended execution process

### 1. Freeze the contract and review the handoff

Have a reviewer examine the spec for functional sufficiency and inappropriate
transmission of implementation expression. Keep the source-exposed ledger here
separate from implementer-facing materials. Record whether each requirement is
interface evidence, public documentation, oracle observation or project policy.
Require a concrete rationale for any exact byte-level ordering requirement.

The observations prove specific cases only. If a new reference probe conflicts
with prose or a requirement, classify it as a spec gap, reference issue,
intentional divergence or implementation bug. Review the contract change before
refreshing expected output. Never edit goldens merely to make new code pass.

### 2. Prepare a technically isolated implementation environment

The safest initial deliverable is a standalone Rust kernel. A custodian who is
not the implementer prepares a new directory or repository containing only:

- The reviewed specification, JSON observations and original-input collector.
- A minimal new Cargo scaffold, the approved public signature/types and a small
  harness protocol. No legacy repair body or legacy-derived stub implementation.
- A reviewed test-only adapter to the production tile encoder, or an isolated
  callable service with its input/output protocol. It must expose the actual
  truncation/winding/degeneracy rules for the mandatory downstream comparison.
- Explicitly audited dependencies/source primitives, with versions and licenses.
- A written allowed-tool/reference policy and a route for specification questions.

Do not clone or mount the full original repository into that environment. Do
not expose `.git`, sibling worktrees, compiler sources containing the old code,
prior diffs, logs, agent transcripts, source indexes or unreviewed test archives.
A normal worktree or a prompt saying “do not look” does not provide technical
separation. Use a restricted account/container/filesystem policy when making a
strict isolation claim; document any residual access. Scratch isolation is
useful, but the only copy of code/audit evidence must not remain on disposable
scratch. Nothing needs to be deployed or exposed as a public web host.

For a later repository scaffold, export approved files at the pinned commit
without history, remove quarantined material **before** the implementer can
access it, and inspect the resulting file manifest. Prefer copying an allowlist
over deleting a few known paths from a full checkout. Source-exposed custodians
can prepare interface adapters; implementers should not have to open the old
module to discover their integration contract.

The follow-up package preparation supplies a standalone Cargo scaffold, an exact
production encoder extraction as a test-only executable, offline vendored
dependencies and a tested Bubblewrap command boundary. See the prepared
workspace's README and the validation record below. The current analyst session
still has unrestricted filesystem access; neither it nor an ordinary T3 thread
becomes agent-wide isolated merely by using the new working directory.

### 3. Start a genuinely separate task context

Create a fresh agent without inherited messages, summaries, memory additions or
attachments from this analyst. Do not use a full-history fork. Record the actual
model/provider/version when available, context settings, approved input hashes,
tools and reachable paths. Ask for an exposure declaration and independent design
before code. Do not ask it to “remember/reproduce GEOS MakeValid.”

These controls limit task-local exposure. They cannot prove that a model never
encountered a public implementation during training. Do not manufacture that
attestation. A human maintainer owns the decision about whether the evidence
meets the project's provenance needs.

### 4. Keep a reviewed question/answer boundary

Implementer questions go to the custodian. Replies should contain original
behavioral requirements, public documentation or new independent-input oracle
results, never old loops, helper names or source-shaped pseudocode. Log each
question, evidence, reply and spec version.

If prohibited source is exposed, stop implementation in that context, record the
event, quarantine affected work and review whether a fresh implementation task
must restart. Do not try to fix the evidence trail by erasing the exposure or
asking the same context to forget it.

### 5. Validate independent behavior and integrate separately

The implementer builds and tests the isolated kernel. A separate integrator
plugs it into the production caller, retires the complete old repair dependency
footprint and checks the rest of the writer. The integrator may examine old code
for removal/audit, but must not rewrite the new kernel from that knowledge.
Return integration failures as minimal original inputs and expected behavior
through the reviewed boundary.

Run standalone geometry comparisons, exact grid-adapter comparisons, original
adversarial tests, real tile comparisons, affected binding tests and WASM checks
as specified in the handoff. Record missing data/skips. A test that passes after
skipping is not reference evidence. Runtime GEOS is an external test oracle,
not a dependency of the shipped default implementation.

Separately review code/source provenance. Similarity checks can identify
suspicious material but cannot certify independence. Check retained helper code,
comments, unreviewed fixtures and transitive paths as well as the new function.
Any explicitly permitted primitive port needs its own attribution.

### 6. Update notices only after the actual replacement is complete

Reconcile root/crate notices, `docs/jts-provenance.md`, acknowledgements, relevant
format/testing docs and shipped package notices against the code that remains.
Preserve required JTS/dependency attribution. Remove or amend deferred-repair
language only when the entire relevant implementation has actually been retired
and reviewed. Historical release/history questions remain distinct; do not erase
history or assert complete repository license compliance from this one change.

## Evidence packet and completion gates

Keep durable, versioned copies of:

1. Exact scope, baseline and introduction commit; upstream provenance/license
   findings and unresolved questions; retired and retained helper inventory.
2. Approved spec/package hashes and review record; task-local exposure statements.
3. Reference binary versions/hashes, collector/version, original inputs, raw
   outputs, generated-test seeds and minimized mismatches.
4. Independent design, authored changes, allowed source/dependency manifest,
   reviewed Q&A and actual isolation/access limitations.
5. Functional validation, integration results, provenance review and the precise
   notice changes justified by the final patch.

The handoff is ready for implementation after spec review and actual isolation
preparation. The replacement is ready for release only after separate functional
and provenance acceptance. This research supplies a reviewed draft and measured
behavior, not completion of those later gates.

## Deliberate limitations and open decisions

- Exact upstream derivation and old distribution obligations are not resolved.
- The prepared handoff now includes a strict replay harness, encoder adapter and
  comparator controls. Original generated/stress cases and production integration
  remain implementation work. The wide-`i32` bowtie is explicitly outside the
  existing encoder's delta arithmetic domain; its kernel comparison remains required.
- The infallible current signature has no explicit budget/error channel. A
  candidate needing resource limits or ordinary failure must propose a reviewed
  fallible integration rather than conflate failure with legitimate empty area.
- Compatibility is geometric and tile-semantic; byte-identical GEOS enumeration
  is not required. A newly discovered consumer requirement needs evidence.
- No blanket equivalence is asserted for JTS `GeometryFixer`, JTS polygonization,
  the existing `geo` repair, other GEOS versions or extreme floating behavior.
- The Wine clean-room page returned an access-denied challenge during research;
  no recommendation relies on reading it. Some EUR-Lex opens challenged access;
  official indexed text/identifiers were used by the research agent.

## Reproduction and current verification

Run the collector as shown in the implementer spec, targeting a new scratch
capture. The checked-in JSON is the observed result of that collector with the
recorded runtime, not a Terrio-generated golden. Verify its embedded
`script_sha256` against the collector before releasing an approved handoff.

For documentation/reference-only changes, check JSON decoding, unique case IDs,
finite/valid reference outputs, input integer bounds, collector-hash consistency,
deterministic recapture and repository whitespace/changed-file hooks. Builds
and full geometry conformance belong to the eventual implementation change.

Verification performed for this research: the collector compiled as Python;
all 51 IDs were unique; all input ordinates were integers within `i32`; all
polygonal reference outputs were valid and their counts/areas matched the
recorded summaries; the collector SHA-256 matched the capture; both loaded
GEOS binaries were recorded; and a fresh capture was byte-identical. Changed-file
pre-commit passed JSON, end-of-file, trailing-whitespace and size checks.
Rust/format/Clippy hooks correctly skipped because no Rust files changed.
No production repair or conformance pass is claimed.

## Prepared package

The standalone workspace is `/home/mavrick/projects/terrio-make-valid-clean-room`.
Its `IMPLEMENTER-PROMPT.md` is the fresh-context brief; its `README.md` documents
build, replay and command isolation. The source-exposed builder and audit ledger
stay outside that workspace. `MANIFEST.json` checks the supplied file set and
hashes; unexpected immutable files and symlinks are rejected. Mutable code and
notes are separate from read-only fixtures, tools and vendored dependencies.

The command sandbox uses new user/PID/network namespaces, clears inherited
environment variables, hides the host home and checkout, mounts system binaries
and libraries without `/usr/local` or `/usr/share`, and disables network access.
Every package receives a new scratch build directory tied to its manifest.
Approved external Shapely/NumPy packages and their native dependency files are
hashed and individually mounted; no GEOS headers or implementation sources are
included. Rust dependencies retain MIT/Apache/Unicode notices as applicable.
These controls implement a selected command policy; Bubblewrap itself supplies
mechanisms rather than a complete universal policy.
[Bubblewrap's own security model](https://github.com/containers/bubblewrap/blob/main/README.md).

The encoder executable contains only selected original caller functions from
the pinned `vector.rs` and `mvt.rs`, copied unchanged into a new transport wrapper.
It includes no repair or validity algorithm. The custodian ledger hashes each
selected source block, the generated wrapper and the stripped binary. Relevant
GDAL source headers identify MIT licensing and copyrights; package NOTICE retains
them. [GDAL ring source license](https://raw.githubusercontent.com/OSGeo/gdal/v3.13.3/ogr/ogrlinearring.cpp),
[GDAL MVT source license](https://raw.githubusercontent.com/OSGeo/gdal/v3.13.3/ogr/ogrsf_frmts/mvt/ogrmvtdataset.cpp).

Reproduce preparation with `scripts/reference/prepare_make_valid_handoff.py`
through `hq`, using a new destination. Validate it with
`scripts/reference/validate_make_valid_handoff.py`; its command invocations use
the package's own queue-aware isolation launcher. The checked-in
`make-valid-handoff-validation.json` records the final package audit, tests and
known exclusions. No implementation task is started by these scripts.
