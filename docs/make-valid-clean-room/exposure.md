# Task-local exposure record

This is a maintainer summary of the implementing agent's record from
2026-10-30 (~02:48 UTC), not a replacement attestation. The
[unabridged record](https://github.com/opengeos/terrio/blob/a2d85cb49c231c1d13653b9170f03bdc74cd9404/docs/make-valid-clean-room/exposure.md)
remains pinned to the reviewed commit (SHA256 `70fa76cec4f90f36c1b809e5acf9aee903830f56c4beb7d865b2c8105c3c5a5e`). The sealed delivery
archive has SHA256
`5257385a86089d787eee6adf80102872d5fd84c3e0858828bc2ccac1a99cc1ab`.

## Inputs and access

The configured model was `claude-opus-5-5`, using Claude Code 2.1.301 in a fresh
process without inherited transcripts, saved project state, browser access or
MCP tools. The implementer could use shell, read and edit tools within the
isolated process. Its own record does not independently verify the backend
model identity beyond that configuration.

The approved inputs were the behavioral specification and adapter protocol,
fixture observations, capture/replay tools, a Rust scaffold, vendored
dependencies and compiler/runtime executables. Executable references were
GEOS 3.14.1, Shapely 2.2.0, NumPy 2.5.4 and the tile-encoder adapter; Rust was
1.99.0. The original checkout, repair source, repository history, host home and
other conversations were unavailable. Outbound network access was disabled
apart from the restricted model-inference transport. Package-integrity and
boundary checks passed before implementation.

The implementer reported reading the approved inputs and a non-geometry CLI
launcher, plus environment metadata and directory listings. It reported no
access to GEOS, JTS, PostGIS or Terrio repair implementation source. Shapely and
GEOS were called through public APIs, without opening their implementation or
test source. The tile-encoder binary was executed, not disassembled.

## Input identities

| File | sha256 |
| --- | --- |
| SPEC.md | e8a6e806a051091fe653af45f9523bc2d62a7feeb2e45829f9b84092e80fd12e |
| PROTOCOL.md | d34d3b544a39904312e57697b15240968d815b84b2bd2e5acb1b17f3e4a5fc6c |
| fixtures/observations.json | e15bec9086c2fbdc7b9fd2d4a62decc32e5eb5ec800fd7dd6aed989548de91f4 |
| reference/capture_make_valid_contract.py | bb27219aed5874c7f7bc542cea45c9733610ba03d24975801acd00c61d7cc021 |
| reference/tile-encoder | da04be3b96fb160ade4c77e04d758c6f30e49214be70fbf9fe43f2f0723f24af |
| tools/replay.py | 7308c2af7b825ff849a7cc075591041f334ac64360becba73b48fefcce8c8eee |
| libgeos.so.3.14.1 | bbe7a60a4fad2120b101f9b8551cab67227b0633002f8a4f9a8550b76632912a |
| libgeos_c.so.1 | 2166c0a874b92aba2836bdd84b7dbc92fbf6111ca57913e8f1e9129b3636ec93 |

## Design observations and background knowledge

Black-box probes covered encoder orientation/rotation sensitivity, pinched bays,
chords, overlaps and degenerate exteriors. A behavioral hypothesis using public
polygonize/union primitives agreed with executable linework repair on 400 random
small cases; an output-vertex hypothesis agreed on 353 invalid cases. The
independent design records how those observations determined the fill rule.

The implementer reported general knowledge of computational geometry, including
planar arrangements and exact predicates, and documentation-level awareness
that GEOS/PostGIS linework repair nodes boundaries and builds area. It stated
that no remembered implementation source was reconstructed or translated.
A fresh context and access controls do not establish a model's pretraining
history; this record describes task-local inputs and observed access.

No accidental prohibited-source exposure was reported. During implementation,
one source-exposed integrator message conveyed only the approved fallible API
and validation priorities; it supplied no algorithm details. The
[interface decision](https://github.com/opengeos/terrio/blob/3a2df9cc469a3883eba7968d98e13e07fda46a9d/docs/make-valid-clean-room/custodian-response.md) remains recorded. Detailed process
paths and transport configuration are historical evidence in the pinned
original, not contributor setup requirements.
