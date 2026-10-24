# Vector SQL

`terrio sql`, Rust `ops::sql::query`, Python `terrio.sql(src, query)` and JavaScript
`sql(src, query)` share one pure-Rust query engine, including WebAssembly builds. The CLI MCP
`sql` tool exposes the same command. `convert --sql` and Python `convert(..., sql=...)` use
the same engine before subsequent vector translation options.

```sql
SELECT c.ADMIN, c.POP_EST/1000000.0 AS millions, r.label
FROM countries c LEFT JOIN regions r ON c.CONTINENT = r.continent
WHERE POP_EST > 10000000 ORDER BY POP_EST DESC LIMIT 10
```

## Supported OGR SQL subset

- `SELECT *`, `table.*`, fields, `FID`, numeric/string/null literals, computed expressions,
  and explicit `AS` column aliases. Computed expressions support `+`, `-`, `*`, `/`, `%`,
  unary signs, parentheses, comparisons, predicates and the existing `||` string concatenation
  extension. Integer division truncates toward zero; use a floating literal for real division.
  Field types are determined from the expression and schema even when no rows match.
- `WHERE`: comparisons, `AND`, `OR`, `NOT`, `IN`, `BETWEEN`, `IS [NOT] NULL`, `LIKE`, `ILIKE`,
  arithmetic and parentheses. Only primary-layer fields may appear in `WHERE` or `ORDER BY`.
- One `ORDER BY` primary field, optionally `ASC` or `DESC`, followed by optional `LIMIT n`.
  Nulls sort first ascending and last descending. Computed aliases cannot be used for ordering.
- `SELECT DISTINCT field`: one attribute field, retaining its type, with null emitted once.
  Results discard geometry and CRS; strings use exact case-sensitive distinctness, as observed
  with GDAL 3.12 OGRSQL. `ORDER BY` must use that field.
- `COUNT(*)`, `COUNT(field)`, `COUNT(DISTINCT field)`, `SUM(field)`, `AVG(field)`, `MIN(field)`
  and `MAX(field)`. Every selected column must be a summary. `COUNT(field)` and
  `COUNT(DISTINCT field)` skip nulls. Empty inputs yield one row: counts are zero and the other
  summaries are null. `SUM` retains the numeric input type, `AVG` is floating point, and
  `MIN`/`MAX` retain the input type. Summary arguments must be fields, and summary queries do
  not accept joins, `DISTINCT` at SELECT level, or `ORDER BY`.
- Same-dataset attribute `LEFT [OUTER] JOIN` (bare `JOIN` has the same OGR lookup semantics).
  Each `ON` clause must be `primary.field = joined.field`; table aliases are supported.
  Multiple independent lookups may refer to the primary layer. Joined fields require table
  qualification in expressions. Every primary feature yields exactly one row, using the first
  matching secondary feature in source order when keys repeat. Missing matches and null keys
  produce null joined fields. String join keys compare without ASCII case sensitivity.
  Only primary geometry, CRS and IDs are retained. Wildcards expand fields in source order;
  qualified selections retain their qualifier in the output field name.

Unaliased computed fields are named `FIELD_1`, `FIELD_2`, and so on by output position;
summaries are named `COUNT_field`, `SUM_field`, etc. Output names must be unique ignoring case
for the standalone query APIs. All integer types map to Terrio's signed 64-bit `Integer` model;
OGR's distinction between 32-bit and 64-bit count/predicate fields is not represented.

This is a defined subset of [OGRSQL](https://gdal.org/en/stable/user/ogr_sql_dialect.html).
`GROUP BY`, spatial SQL, external-dataset joins, arbitrary `ON` expressions, `OFFSET`,
`UNION`, casts, scalar functions and standard-deviation summaries are not implemented.
String predicates (comparisons, `IN` and `BETWEEN`), like join lookups, ignore ASCII case
as observed with GDAL 3.12; `ORDER BY` and `MIN`/`MAX` use case-sensitive ordering. SQL expressions should use explicit null tests where needed.
Integer addition, subtraction and multiplication wrap at 64 bits; division/remainder overflow
and division by zero yield null (OGRSQL instead uses an integer-maximum sentinel for non-null
division by zero). Use floating-point expressions for larger numeric ranges.

## Execution and validation

Unordered projections iterate borrowed source features and can stop once the requested result
limit is reached; summaries use constant accumulator memory except `COUNT(DISTINCT ...)`.
`DISTINCT` stores seen values, `ORDER BY` stores sorted feature references, and joins index
secondary keys once, retaining the first row per matching key. Existing file-based SQL entry points load their input layers;
chunked `FeatureStream` SQL is not supported. Ordinary streaming vector conversions retain
their existing path.

`scripts/compare_vector_ops_with_gdal.py` includes Natural Earth SQL comparisons against
`ExecuteSQL(..., dialect="OGRSQL")`, exercising both standalone SQL and `convert --sql`.
It compares values, nulls, field names and logical types, complete geometry WKB and CRS
retention, with duplicate, unmatched and null join keys. Integer widths normalize to the
shared `Integer` model. These cases are included in the conformance workflow's vector suite.
