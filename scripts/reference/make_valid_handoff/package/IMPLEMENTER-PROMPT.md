Implement the standalone integer-tile polygon repair in this approved package.
Begin by reading AGENTS.md, README.md, SPEC.md and PROTOCOL.md. Record task-local
source exposure and your independent design under notes/ before writing repair
code. Use only approved package inputs and vetted references/dependencies.

Implement the signature in candidate/src/lib.rs, register it through
implementation(), and validate geometric results plus the post-truncation tile
encoder behavior. Keep immutable references unchanged. Obtain additional
observable behavior or dependency approval through the custodian. Record any
prohibited source exposure and stop implementation in that context for review.

Deliver source, original adversarial/generated tests and seeds, design and
provenance notes, and a validation report distinguishing passes, exclusions,
failures and outstanding integration. Production integration is a later
custodian task. A fresh conversation and agent-wide access boundary are required;
do not inherit the analyst conversation or original repository access.
