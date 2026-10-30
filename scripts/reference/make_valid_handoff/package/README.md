# Independent tile repair handoff

Start with `AGENTS.md`, then the complete contract in `SPEC.md`. `PROTOCOL.md`
describes the candidate and production-encoder tools. `MANIFEST.json` identifies
and hashes the supplied files; it is an audit snapshot, not a signed legal
certification. `notes/` is for your original design and evidence.

The kernel is intentionally unimplemented. The standalone crate and transport
build, while normal acceptance replay fails until a repair is supplied. All 51
reference observations and an actual production tile-encoder executable are
included. No repair algorithm, original repository history or analyst transcript
is included. The encoder is an isolated test reference, not a candidate library.

## Commands on the preparation host

Run from this directory. The launcher places heavy commands in the host queue,
mounts only the approved workspace/toolchain/runtime, disables network access,
and puts build output on the SSD. It needs no sudo and starts no service.

```sh
python3 tools/verify_package.py --initial
python3 tools/isolate.py -- python3 tools/check_boundary.py
python3 tools/isolate.py --slot heavy -- cargo build --locked --offline
python3 tools/isolate.py -- python3 tools/check_scaffold.py
python3 tools/isolate.py -- python3 tools/check_harness.py
python3 tools/isolate.py -- python3 tools/replay.py --reference-self-check
```

After implementing, run strict acceptance and your original tests:

```sh
python3 tools/isolate.py -- cargo test --locked --offline
python3 tools/isolate.py -- cargo clippy --workspace --all-targets --locked --offline -- -D warnings
python3 tools/isolate.py -- cargo fmt --all -- --check
python3 tools/isolate.py -- python3 tools/replay.py
python3 tools/isolate.py --slot heavy -- cargo build --locked --offline --lib --target wasm32-unknown-unknown
```

`check_scaffold.py` is a preparation check: once implemented it is supposed to
fail. `--reference-self-check` checks fixtures/adapter plumbing using recorded
reference polygons, not a repair implementation. Neither is candidate acceptance.
The replay harness covers the recorded cases; generated/stress tests and final
Terrio integration required by `SPEC.md` remain additional work.

`HOST-RUNTIME.json` names only host toolchain and approved Python runtime files
needed by the sandbox. It is machine-specific; it exposes no runtime credentials.
The Python runtime and Rust toolchain are mounted read-only from existing
installations, not distributed inside this source package. GEOS is only a
reference-test dependency. Native candidate code must remain pure Rust.

The launcher is a tested **command** sandbox. To run an agent, give a fresh
context the brief in `IMPLEMENTER-PROMPT.md` and arrange for all its file,
terminal, browser and other tool access to obey this boundary. Starting a normal
T3 thread here with unrestricted host tools does not enforce that rule. This
package neither changes T3 provider settings nor starts another conversation.

## Portability

The source crate and vendored Rust dependencies build offline. The supplied
encoder executable targets Linux x86-64 and is versioned by its hash. On another
host, a custodian must rebuild that executable from the reviewed source boundary
and provision the pinned reference runtime, then review the runtime configuration.
Do not replace the immutable encoder with a generic encoder.

Reference preparation metadata is in `HANDOFF.json`; the custodian retains the
completed validation record outside the implementation package. Run the commands
above to verify the package in your own environment.
`NOTICE` distinguishes original scaffold code, permissively licensed dependencies
and the copied encoder reference. No package preparation check establishes the
independence or correctness of a future implementation by itself.
