# JSON transport and encoder reference

Both executables accept exactly one UTF-8 JSON object on stdin and return one
JSON object on stdout. Use a new process for each request. The transport input
limit is 16 MiB; stress the Rust API directly for larger cases. The transport's
limits are not a geometry simplification or repair policy.

## Candidate

Build with `cargo build --locked --offline`. The executable is
`/build/debug/candidate` inside the sandbox. Input:

```json
{"polygons": [[[[0,0],[6,6],[0,6],[6,0],[0,0]]]]}
```

`polygons` is an array of polygons, each containing exterior-then-hole rings,
each containing `[i32,i32]` points. Successful output is
`{"status":"ok","polygons":[...]}`, using finite fractional coordinates.
The untouched scaffold returns `{"status":"unimplemented"}`. Transport errors
return `{"status":"error","message":"..."}`. A process crash, timeout, error
or unimplemented status is an acceptance failure, including for empty input.

The required callable signature and `implementation()` registration are in
`candidate/src/lib.rs`. Implement the geometry there or in independently authored
modules; preserve the transport so the same acceptance harness remains usable.

## Tile encoder

`reference/tile-encoder` accepts the same nesting, with finite floating points
in `i32` coordinate range. This is the production writer's post-repair encoder,
starting with a fresh `(0,0)` geometry cursor, operating in truncation mode.
It contains **no repair implementation** and never calls the candidate or GEOS.

Successful output includes:

- `status: "ok"`;
- `present`: whether the production caller would retain a polygon feature;
- `commands`: the actual MVT geometry-command integers;
- `polygons`: closed integer-coordinate rings in command traversal order, grouped
  by the surviving exteriors. These are emitted by the original encoder itself,
  not by a substitute rounding implementation.

The underlying production functions are copied unchanged at the pinned baseline;
the standalone wrapper supplies validation, JSON transport and error reporting.
The reference keeps the original truncation, winding, closure, duplicate-removal,
hole-dropping and cursor rules. Its source stays with the custodian; only its
reviewed executable and necessary attribution enter this handoff.

The build enables integer overflow checks. A production-encoder arithmetic
overflow yields `{"status":"encoder_overflow"}`, never an empty success. The
`wide-i32-bowtie` kernel stress case spans deltas that do not fit the existing
encoder's `i32` arithmetic. Its **kernel** comparison is mandatory, but its grid
comparison is explicitly excluded and reported. All other captured cases must
encode successfully. This is a known caller-domain limitation, not permission
to omit arbitrary failed cases or weaken the full-`i32` kernel contract.
Other unexpected encoder panics are reported separately as `encoder_panic` and
are failures, not domain exclusions.

Finite inputs outside the encoder coordinate domain and malformed requests
return `status: "error"`. The reference wrapper does not redefine production
behavior for non-finite or out-of-range coordinates.

`tools/replay.py` compares floating polygonal geometry and then exact geometric
content of the emitted integer rings, allowing equivalent ring starts/order.
It also compares feature presence. Whole command-array equality is a useful
diagnostic, but ring start/component enumeration alone is not an acceptance
requirement. The runner fails on mismatches, oracle errors and unimplemented
candidate responses; reference-only self-check is explicitly labeled.
