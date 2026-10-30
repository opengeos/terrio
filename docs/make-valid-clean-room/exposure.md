# Task-local exposure and boundary record

Recorded by the implementing agent before any repair code was written
(2026-10-30, UTC ~02:48). Facts below are what this process could observe; they
are not a claim about model pretraining or about the host outside the sandbox.

## Agent identity and tools

- Model configured by the environment: `claude-opus-5-5` (ANTHROPIC_DEFAULT_*_MODEL
  and the launcher's `--model`). I cannot independently verify the model behind
  the gateway beyond this configuration.
- Client: Claude Code 2.1.301 (`/agent-tools/claude`, `AI_AGENT=claude-code_2-1-301_agent`),
  bare/print mode, fresh session `591d41e0-…`, `--strict-mcp-config` with
  `{"mcpServers":{}}`, `--setting-sources ""`, hooks disabled, slash commands
  disabled, `--no-session-persistence`.
- Tools actually available to me: **Bash, Read, Edit**. (The launcher
  `/agent-tools/entry.py` requests `Bash,Read,Write,Edit,Glob,Grep`; Write, Glob
  and Grep were not exposed in this session. No MCP, browser or web tool exists.)
- Fresh `CLAUDE_CONFIG_DIR=/agent-state/claude` and `HOME=/agent-state/home`
  (empty). `.claude.json` contains only first-start metadata and two built-in
  plugin usage counters at 0. No prior transcript, memory or project history
  was present; `projects/-work/` holds only this session.

## Filesystem boundary (from /proc/self/mountinfo)

Read-only: `/usr/bin`, `/usr/lib`, `/usr/libexec`, `/usr/lib64` (host OS),
`/toolchain` (stable Rust 1.99.0 incl. wasm32 std, clippy, rustfmt), `/work`
(this package), `/reference/{numpy,shapely}` and `/reference/lib/*` (hashed GEOS
3.14.1 / OpenBLAS / runtime libs), `/agent-tools/{claude,entry.py,prompt.txt}`,
`/bridge/model.sock`.
Writable: `/work/candidate`, `/work/notes`, `/build` (cargo target), `/agent-state`,
tmpfs `/tmp`, `/dev` and `/`.
Absent: `/home/mavrick`, `/mnt`, `/run`, `/usr/local`, `/usr/share`, `/usr/include`,
`/work/.git`, `/work/docs`. `/home/implementer` exists and is empty.

Mount *source paths* reveal host directory names (e.g. the package's host
checkout path and `…/terrio/scripts/reference/isolated_claude_entry.py` mounted as
`/agent-tools/entry.py`). Only that single launcher file from the Terrio tree is
visible; I read it: it is a socket relay + CLI launcher, with no geometry code.

## Network

`1.1.1.1:443` and `8.8.8.8:53`: "Network is unreachable"; `/proc/net/route` empty.
Only `127.0.0.1:39876` (in-namespace relay to the model socket) connects.

## Package checks run

- `python3 tools/verify_package.py --initial` → PASS (all hashes incl. scaffold).
- `HOME=/home/implementer python3 tools/check_boundary.py` → PASS
  (Shapely 2.2.0, GEOS 3.14.1, NumPy 2.5.4). The script asserts
  `HOME=/home/implementer`; the launcher ran it before switching HOME to
  `/agent-state/home`, so I re-ran it with that HOME value. All other checks in
  it (hidden paths, read-only references, network, writable dirs) ran unmodified.

## Files read (content)

Approved package: RUN-ENVIRONMENT.md, AGENTS.md, README.md, SPEC.md, PROTOCOL.md,
IMPLEMENTER-PROMPT.md, HANDOFF.json, HOST-RUNTIME.json, NOTICE, Cargo.toml,
rustfmt.toml, .gitignore, .cargo/config.toml, candidate/Cargo.toml,
candidate/src/{lib,main}.rs, notes/.keep, tools/*.py (replay, check_boundary,
check_harness, check_scaffold, verify_package, runtime_hash; isolate.py not
opened), reference/capture_make_valid_contract.py, fixtures/observations.json
(parsed and summarised). Environment: /agent-tools/entry.py, prompt.txt,
/agent-state/claude/.claude.json, directory listings of /, /usr/lib (top),
/usr/lib/python3/dist-packages (names), /toolchain.

Input hashes (sha256):

| File | sha256 |
| --- | --- |
| SPEC.md | e8a6e806a051091fe653af45f9523bc2d62a7feeb2e45829f9b84092e80fd12e |
| PROTOCOL.md | d34d3b544a39904312e57697b15240968d815b84b2bd2e5acb1b17f3e4a5fc6c |
| fixtures/observations.json | e15bec9086c2fbdc7b9fd2d4a62decc32e5eb5ec800fd7dd6aed989548de91f4 |
| reference/capture_make_valid_contract.py | bb27219aed5874c7f7bc542cea45c9733610ba03d24975801acd00c61d7cc021 |
| reference/tile-encoder | da04be3b96fb160ade4c77e04d758c6f30e49214be70fbf9fe43f2f0723f24af |
| tools/replay.py | 7308c2af7b825ff849a7cc075591041f334ac64360becba73b48fefcce8c8eee |
| /reference/lib/libgeos.so.3.14.1 | bbe7a60a4fad2120b101f9b8551cab67227b0633002f8a4f9a8550b76632912a |
| /reference/lib/libgeos_c.so.1 | 2166c0a874b92aba2836bdd84b7dbc92fbf6111ca57913e8f1e9129b3636ec93 |

## Not read / deliberately avoided

- No GEOS, JTS, PostGIS or Terrio repair source is present or was sought.
- `/reference/shapely` is the oracle's Python package. A filename search showed
  it contains Shapely's own tests (e.g. `tests/legacy/test_make_valid.py`);
  I did not open them or any Shapely module source. Shapely/GEOS were only
  imported and called through public APIs as a black box.
- The tile encoder binary was only executed (JSON in/out), never disassembled.
- Host tools such as git, gcc, clang, curl, wget, node exist in `/usr/bin`;
  with no network and no repository they expose no prohibited material. I used
  none of them except gcc implicitly as cargo's linker.

## Oracle probes used for design (independently authored, black-box)

Scripts under `/build/tmp` (copied into `candidate/tests/oracle/` later):
encoder orientation/rotation sensitivity, a Python *behavioural hypothesis*
model built from public Shapely primitives (polygonize/union) checked against
`make_valid(method="linework")` on 400 random small cases (0 mismatches), an
output-vertex-set hypothesis check (353 invalid cases, 0 mismatches), and
targeted probes (pinched bays, chords, triple overlap, degenerate exteriors).
These are behaviour observations, not implementation input.

## Pretraining caveat

A fresh context proves nothing about pretraining. I have general background
knowledge of computational geometry (DCELs, planar arrangements, exact
predicates) and public documentation-level awareness that GEOS/PostGIS
"linework" repair nodes boundaries and builds area. I did not reconstruct or
translate any remembered source. The fill rule implemented here was derived
from, and is justified by, the black-box observations recorded above and in
design.md.

## Accidental exposure

None observed in this context.

## Addendum (during implementation)

At ~03:17 UTC a custodian process message interrupted my optional stress-oracle
run and pointed to `notes/custodian-response.md` (created by the custodian in the
writable notes directory). I read it: it conveys only an interface/error-policy
decision (production propagates the fallible `try_make_valid` result) and
validation priorities. It contains no algorithm or prohibited source material.
No other new inputs were received; no accidental exposure occurred.
