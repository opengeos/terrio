`duplicate_members.zip` is an original synthetic fixture generated with Python's `zipfile`.
It contains two stored entries named `data.geojson`, with fixed ZIP timestamps of 1980-01-01.
Each contains one point at (1, 2); their dataset names and `source` properties are `first`
and `second`. It exercises ambiguous name handling without malformed ZIP headers or CRCs.
