# Independent tile repair workspace

Read `README.md`, `SPEC.md` and `PROTOCOL.md` before designing or coding. Record
task-local exposure and permitted inputs in `notes/exposure.md`, then describe
your independent design in `notes/design.md` before implementing it.

Use the approved files listed in `MANIFEST.json`. Reference material is read-only;
write implementation/tests under `candidate/` and work notes under `notes/`.
The initial candidate explicitly reports `unimplemented`; wire the required
function through `implementation()` when it exists. Empty successful output
means no surviving area and must never stand for an unfinished implementation.

The source-exposed analyst conversation, legacy repair, repository history and
unreviewed fixtures are outside the information boundary. Obtain new behavioral
observations or approved dependencies through the custodian. Record accidental
source exposure and stop implementation in that context for custodian review.

Use a fresh conversation. An ordinary host-side agent is not confined merely
because its working directory is here: `tools/isolate.py` confines only commands
it launches. Agent-wide filesystem/browser/tool restrictions must enforce the
same approved boundary if strict isolation is claimed. No existing conversation
or agent is launched by this package.

Build/test using the commands in `README.md`. Preserve immutable reference hashes.
Deliver source, original tests/seeds, design/provenance notes and validation that
separates passes, exclusions and failures. Production integration and notices
belong to the custodian after the independent kernel is delivered.
