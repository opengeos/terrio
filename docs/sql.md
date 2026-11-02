# Vector SQL

`terrio sql`, Rust `ops::sql::query`, Python `terrio.sql(src, query)` and JavaScript
`sql(src, query)` share one pure-Rust query engine, including WebAssembly builds. The `sql`
MCP tool exposes the same command, as in
`terrio sql countries.gpkg big.geojson --query "SELECT ADMIN FROM countries WHERE POP_EST > 10000000"`. `convert --sql` and Python `convert(..., sql=...)` use
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
  SELECT arithmetic requires numeric columns. Field types are determined from the expression
  and schema even when no rows match; the legacy WHERE evaluator may also coerce numeric text.
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
External-dataset joins, arbitrary `ON` expressions, `OFFSET`,
`UNION`, casts, general scalar functions and standard-deviation summaries are not implemented.
String predicates (comparisons, `IN` and `BETWEEN`), like join lookups, ignore ASCII case
as observed with GDAL 3.12; `ORDER BY` and `MIN`/`MAX` use case-sensitive ordering. SQL expressions should use explicit null tests where needed.
Integer SUM reports an error on overflow. Row-expression integer addition, subtraction and
multiplication wrap at 64 bits; division/remainder overflow
and division by zero yield null (OGRSQL instead uses an integer-maximum sentinel for non-null
division by zero). Use floating-point expressions for larger numeric ranges.

## Grouping and spatial functions

The same engine also supports a defined subset of
[GDAL's SQLite dialect](https://gdal.org/en/stable/user/sql_sqlite_dialect.html).
It does not switch existing OGRSQL expressions, comparisons or joins to SQLite semantics.

```sql
SELECT CONTINENT, COUNT(*) AS n, SUM(POP_EST) AS population
FROM countries GROUP BY CONTINENT ORDER BY population DESC LIMIT 5

SELECT ADMIN, ST_Transform(ST_Centroid(geometry), 3857) AS shape
FROM countries WHERE ST_Area(geometry) > 1
```

`GROUP BY` accepts one or more primary attribute fields (including qualified names).
Selected columns must be grouping fields or the existing field-based aggregates;
keys not selected may also group rows. Text keys are case-sensitive; null keys
form one group. Filtering happens before grouping. Each group has independent
`COUNT(DISTINCT field)` state. Empty grouped input produces no rows; ungrouped
summaries still produce one row. Grouped results discard geometry and CRS.
One selected output column or alias may supply `ORDER BY`; `LIMIT` applies after
ordering. Grouping does not support joins, SELECT DISTINCT, computed grouping
keys, HAVING or aggregate expressions. It stores keys and accumulators per group,
without copying the source features.

The special `geometry` field denotes the primary geometry when there is no
attribute of that name. Its primary table qualifier is accepted. Functions may
nest and may appear in SELECT expressions and WHERE predicates:

| Function | Result and scope |
|---|---|
| `ST_Area(geometry)` | Planar XY area in squared coordinate units; non-area geometries contribute zero. |
| `ST_Length(geometry)` | Planar XY length of line components; polygon boundaries contribute zero, as in SpatiaLite. |
| `ST_Intersects(a, b)` | Boundary-inclusive XY predicate: 1/0, or SpatiaLite's -1 when either geometry is null. |
| `ST_Buffer(geometry, distance)` | Planar buffer with round joins/caps and 30 segments per quadrant. Requires valid input; invalid inputs or nonfinite distances return null. New geometry is XY. |
| `ST_Centroid(geometry)` | Planar centroid of the highest-dimensional components, returned as an XY point. |
| `ST_Transform(geometry, epsg)` | Constant positive target EPSG code and an EPSG-identified source CRS. Uses the shared CRS engine; nested transforms use the intermediate CRS. Geocentric transforms use XYZ; ordinary horizontal transforms retain source Z/M. |

Except for the predicate's -1 convention, null geometry propagates as null.
Coordinate transformation failures return null for that geometry. Unsupported functions,
invalid argument counts, non-geometry arguments and unsupported transform CRS codes
are query errors, including for empty inputs. Transforms are prepared once per
expression. No external geometry constructors, joined geometry columns, spatial
aggregates or spatial indexes are provided by this subset.

An explicit geometry expression replaces the result's primary geometry; only one
is supported, so combining it with a wildcard or another geometry selection is
an error. Geometry aliases do not create attribute fields. Scalar-only projections
retain the primary geometry, following Terrio's existing SQL behavior; select
geometry explicitly when comparing with SQLite projections. Geometry result types
are known independently of the first row, whereas GDAL SQLite can infer a scalar
column when its first geometry value is null. Terrio also retains static numeric
types for empty grouped results and all-null scalar spatial expressions, where
OGR SQLite infers String. The conformance suite checks these two narrow schema
policies explicitly and reports them separately from exact schema matches. Buffer topology is two-dimensional
and may have different vertex order or last-bit coordinates than GEOS; no claim
of byte-identical constructed geometries is made.

## Execution and validation

Unordered projections iterate borrowed source features and can stop once the requested result
limit is reached; summaries use constant accumulator memory except `COUNT(DISTINCT ...)`.
`DISTINCT` stores seen values, `ORDER BY` caches sort keys and feature references, and joins index
secondary keys once, retaining the first row per matching key. Existing file-based SQL entry points load their input layers;
chunked `FeatureStream` SQL is not supported. Ordinary streaming vector conversions retain
their existing path.

`scripts/compare_vector_ops_with_gdal.py` includes Natural Earth SQL comparisons against
`ExecuteSQL(..., dialect="OGRSQL")`, exercising both standalone SQL and `convert --sql`.
It compares values, nulls, field names and logical types, complete geometry WKB and CRS
retention, with duplicate, unmatched and null join keys. Integer widths normalize to the
shared `Integer` model. These cases are included in the conformance workflow's vector suite.

`scripts/compare_sql_depth_with_gdal.py` (conformance suite `sql-depth`) checks
grouped and spatial queries on Natural Earth through standalone CLI/MCP,
`convert --sql`, Python and an optional current Node/WASM package. Counts, nulls,
text, logical schemas and CRS are exact. Floating values allow relative/absolute
1e-10; constructed point displacement allows 1e-7 CRS units. Buffers allow
symmetric-difference area relative 1e-7 and envelope error absolute 1e-7 source
units; original geometry WKB remains exact. These checks supplement the existing
OGRSQL suite without changing its equivalence checks.
