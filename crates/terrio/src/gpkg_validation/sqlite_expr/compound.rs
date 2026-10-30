//! Left-associative SQLite compound SELECTs. Set equality does not apply affinity;
//! NULLs compare equal, and the first available collation governs each column.
use super::joins::ViewBudget;
use super::*;
use order::storage_atom;
use parse::Parser;
use select::View;
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq)]
enum Operation {
    All,
    Union,
    Intersect,
    Except,
}
pub(super) struct Compound {
    pub arms: Vec<View>,
    operations: Vec<Operation>,
    collations: Vec<String>,
}
fn collation(expr: &Expr, schema: &Schema) -> Option<String> {
    if let Some(c) = expr.explicit_collation() {
        return Some(c.into());
    }
    match expr {
        Expr::Column(parts, _) => {
            schema.columns.iter().find(|c| c.name.eq_ignore_ascii_case(parts.last().unwrap())).map(|c| c.collation.clone())
        }
        Expr::Unary(_, inner) | Expr::Cast(inner, _) => collation(inner, schema),
        _ => None,
    }
}
// Ported from SQLite 3.54.1 subquery type inference; see NOTICE.
#[derive(Clone, Copy, PartialEq)]
enum Decl {
    Blob,
    Text,
    Numeric,
    Integer,
    Real,
    Flex,
}
fn declared(kind: &str) -> Decl {
    match affinity(kind) {
        Affinity::Numeric if kind.contains("INT") => Decl::Integer,
        Affinity::Numeric => Decl::Numeric,
        Affinity::Real => Decl::Real,
        Affinity::Text => Decl::Text,
        Affinity::Blob => Decl::Blob,
    }
}
fn expression_affinity(expr: &Expr, schema: &Schema) -> Option<Decl> {
    match expr {
        Expr::Column(parts, _) => {
            if let Some(c) = schema.columns.iter().find(|c| c.name.eq_ignore_ascii_case(parts.last().unwrap())) {
                if schema.is_view && c.data_type.is_empty() {
                    None
                } else if schema.strict && c.data_type == "ANY" {
                    Some(Decl::Blob)
                } else {
                    Some(declared(&c.data_type))
                }
            } else if !schema.without_rowid && ["rowid", "oid", "_rowid_"].iter().any(|s| parts.last().unwrap().eq_ignore_ascii_case(s)) {
                Some(Decl::Integer)
            } else {
                None
            }
        }
        Expr::Collate(inner, _) => expression_affinity(inner, schema),
        Expr::Cast(_, kind) => Some(declared(kind)),
        Expr::Using(_, _, kind) => {
            if kind.is_empty() {
                None
            } else {
                Some(declared(kind))
            }
        }
        _ => None,
    }
}
fn possible_types(expr: &Expr, schema: &Schema) -> u8 {
    match expr {
        Expr::Literal(Value::Null) => 0,
        Expr::Literal(Value::Text(_)) => 2,
        Expr::Literal(Value::Blob(_)) => 4,
        Expr::Collate(inner, _) => possible_types(inner, schema),
        Expr::Unary(op, inner) if op == "+" => possible_types(inner, schema),
        Expr::Column(parts, quote)
            if !schema.columns.iter().any(|c| c.name.eq_ignore_ascii_case(parts.last().unwrap())) && *quote == 2 && parts.len() == 1 =>
        {
            2
        }
        Expr::Column(parts, 0)
            if parts.len() == 1
                && ["true", "false"].iter().any(|s| s.eq_ignore_ascii_case(&parts[0]))
                && !schema.columns.iter().any(|c| c.name.eq_ignore_ascii_case(&parts[0])) =>
        {
            1
        }
        Expr::Column(..) | Expr::Cast(..) => match expression_affinity(expr, schema) {
            Some(Decl::Integer | Decl::Real | Decl::Numeric | Decl::Flex) => 5,
            Some(Decl::Text) => 6,
            _ => 7,
        },
        Expr::Binary(op, _, _) if op == "||" => 6,
        Expr::Call(..) | Expr::Using(..) => 7,
        Expr::Case(_, arms, otherwise) => {
            arms.iter().fold(possible_types(otherwise, schema), |m, (_, value)| m | possible_types(value, schema))
        }
        _ => 1,
    }
}
fn compound_type(arms: &[View], index: usize) -> String {
    let first = &arms[0];
    let expr = &first.projections[index].0;
    let mut at = 0;
    let mut mask = 0;
    let mut kind = expression_affinity(expr, &first.source_schema);
    while kind.is_none() && at + 1 < arms.len() {
        mask |= possible_types(&arms[at].projections[index].0, &arms[at].source_schema);
        at += 1;
        kind = expression_affinity(&arms[at].projections[index].0, &arms[at].source_schema);
    }
    if kind.is_some_and(|k| k != Decl::Blob) {
        for arm in &arms[at + 1..] {
            mask |= possible_types(&arm.projections[index].0, &arm.source_schema);
        }
        if kind == Some(Decl::Text) && mask & 1 != 0
            || matches!(kind, Some(Decl::Numeric | Decl::Integer | Decl::Real | Decl::Flex)) && mask & 2 != 0
        {
            kind = Some(Decl::Blob);
        }
        if matches!(kind, Some(Decl::Numeric | Decl::Integer | Decl::Real | Decl::Flex)) && matches!(expr, Expr::Cast(..)) {
            kind = Some(Decl::Flex);
        }
    }
    let original = &first.schema.columns[index].data_type;
    if !original.is_empty() && Some(declared(original)) == kind {
        return original.clone();
    }
    match kind {
        None => "",
        Some(Decl::Blob) => "BLOB",
        Some(Decl::Text) => "TEXT",
        Some(Decl::Real) => "REAL",
        Some(Decl::Integer) => "INT",
        Some(Decl::Numeric | Decl::Flex) => "NUM",
    }
    .into()
}
impl Compound {
    pub fn compile(db: &Snapshot, sql: &str, p: &mut Parser, spans: &[(usize, usize)], stack: &mut Vec<String>) -> Result<Option<Self>> {
        let mut boundaries = Vec::new();
        let mut depth = 0;
        let mut suffix = None;
        for at in p.at..p.tokens.len() {
            let token = &p.tokens[at];
            if token.is("(") {
                depth += 1;
            } else if token.is(")") {
                depth -= 1;
            }
            if depth != 0 {
                continue;
            }
            let op = if token.is("UNION") {
                Some(if p.tokens.get(at + 1).is_some_and(|t| t.is("ALL")) { Operation::All } else { Operation::Union })
            } else if token.is("INTERSECT") {
                Some(Operation::Intersect)
            } else if token.is("EXCEPT") {
                Some(Operation::Except)
            } else {
                None
            };
            if let Some(op) = op {
                if suffix.is_some() {
                    return Err(bad("ORDER BY/LIMIT must follow the complete compound query"));
                }
                boundaries.push((at, op));
            } else if token.is("ORDER") || token.is("LIMIT") || token.is(";") {
                suffix.get_or_insert(at);
            }
        }
        if boundaries.is_empty() {
            return Ok(None);
        }
        if boundaries.len() >= 500 {
            return Err(Error::Unsupported("SQLite compound query exceeds 500 SELECTs".into()));
        }
        let end = suffix.unwrap_or(p.tokens.len());
        let mut start = p.at;
        let mut arms = Vec::new();
        let mut operations = Vec::new();
        for &(stop, op) in &boundaries {
            arms.push(Self::arm(db, sql, spans, start, stop, stack)?);
            operations.push(op);
            start = stop + if op == Operation::All { 2 } else { 1 };
        }
        arms.push(Self::arm(db, sql, spans, start, end, stack)?);
        let size = arms[0].schema.columns.len();
        if arms.iter().any(|arm| arm.schema.columns.len() != size) {
            return Err(bad("compound SELECT column counts differ"));
        }
        let collations = (0..size)
            .map(|i| arms.iter().find_map(|arm| collation(&arm.projections[i].0, &arm.source_schema)).unwrap_or_else(|| "BINARY".into()))
            .collect::<Vec<_>>();
        if collations.iter().any(|c| !matches!(c.as_str(), "BINARY" | "NOCASE" | "RTRIM")) {
            return Err(Error::Unsupported("SQLite compound collation".into()));
        }
        p.at = end;
        Ok(Some(Self { arms, operations, collations }))
    }
    fn arm(db: &Snapshot, sql: &str, spans: &[(usize, usize)], start: usize, end: usize, stack: &mut Vec<String>) -> Result<View> {
        if start >= end {
            return Err(bad("empty compound SELECT"));
        }
        View::compile_sql(db, &format!("CREATE VIEW __terrio_compound_arm AS {}", &sql[spans[start].0..spans[end - 1].1]), stack)
    }
    pub fn schema(&self) -> Schema {
        let mut schema = self.arms[0].schema.clone();
        for (i, c) in schema.columns.iter_mut().enumerate() {
            c.data_type = compound_type(&self.arms, i);
        }
        schema
    }
    pub fn column_collation(&self, index: usize) -> &str {
        &self.collations[index]
    }
    pub fn visit(&self, db: &Snapshot, bytes: &[u8], budget: &ViewBudget, visitor: &mut dyn FnMut(&[Value]) -> Result<bool>) -> Result<()> {
        let schema = self.schema();
        let mut emit = |row: &[Value]| {
            if row.iter().zip(&schema.columns).any(|(v, c)| matches!(v, Value::Int(_)) && affinity(&c.data_type) == Affinity::Real) {
                let mut row = row.to_vec();
                for (v, c) in row.iter_mut().zip(&schema.columns) {
                    if affinity(&c.data_type) == Affinity::Real
                        && let Value::Int(n) = v
                    {
                        *v = Value::Real(*n as f64);
                    }
                }
                visitor(&row)
            } else {
                visitor(row)
            }
        };
        if self.operations.iter().all(|op| *op == Operation::All) {
            let mut active = true;
            for arm in &self.arms {
                if !active {
                    break;
                }
                arm.visit_budgeted(db, bytes, budget, &mut |row| {
                    active = emit(row)?;
                    Ok(active)
                })?;
            }
            return Ok(());
        }
        let encoding = u32::from_be_bytes(bytes.get(56..60).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("truncated header"))?);
        let collect = |arm: &View| -> Result<Vec<Vec<Value>>> {
            let mut rows = Vec::new();
            arm.visit_budgeted(db, bytes, budget, &mut |row| {
                rows.push(row.to_vec());
                Ok(true)
            })?;
            Ok(rows)
        };
        let key = |row: &[Value]| row.iter().zip(&self.collations).map(|(v, c)| storage_atom(v, c, encoding)).collect::<Vec<_>>();
        let unique = |rows: Vec<Vec<Value>>| {
            let mut map = BTreeMap::new();
            for row in rows {
                map.entry(key(&row)).or_insert(row);
            }
            map
        };
        let mut result = collect(&self.arms[0])?;
        for (op, arm) in self.operations.iter().zip(&self.arms[1..]) {
            let right = collect(arm)?;
            if *op == Operation::All {
                result.extend(right);
                continue;
            }
            let mut left = unique(result);
            let right = unique(right);
            match op {
                Operation::Union => left.extend(right),
                Operation::Intersect => left.retain(|key, _| right.contains_key(key)),
                Operation::Except => left.retain(|key, _| !right.contains_key(key)),
                Operation::All => unreachable!(),
            }
            let mut sorted = left.into_iter().collect::<Vec<_>>();
            sorted.sort_by(|(a, _), (b, _)| {
                a.iter()
                    .zip(b)
                    .map(|(a, b)| constraints::compare_atom(a, b))
                    .find(|order| *order != Ordering::Equal)
                    .unwrap_or(Ordering::Equal)
            });
            result = sorted.into_iter().map(|(_, row)| row).collect();
        }
        for row in result {
            if !emit(&row)? {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    #[test]
    fn compound_select_count_is_bounded() {
        let mut db = Snapshot { unicode_case: true, objects: vec![], tables: BTreeMap::new() };
        for count in [2, 500, 501] {
            let sql = format!("CREATE VIEW v AS {}", vec!["SELECT 1"; count].join(" UNION ALL "));
            db.objects = vec![("view".into(), "v".into(), "v".into(), 0, sql)];
            assert_eq!(View::compile(&db, "v").is_ok(), count <= 500);
        }
    }
    #[test]
    #[ignore = "requires SQLite 3.54 from the GDAL reference environment"]
    fn compound_column_types_match_sqlite_across_affinities() {
        let script = r#"
import json,sqlite3
out=[]
with sqlite3.connect(':memory:') as db:
 db.execute('CREATE TABLE types(i INTEGER,r REAL,t TEXT,b BLOB,n NUMERIC,u)')
 expressions=['i','r','t','b','n','u','i+0','lower(t)','NULL','CAST(i AS INTEGER)',"CASE WHEN i>0 THEN t ELSE i END","'text'",'1',"X'00'",'TRUE','FALSE','"missing"']
 for a in expressions:
  for b in expressions:
   for op in ['UNION','UNION ALL','INTERSECT','EXCEPT']:
    sql='CREATE VIEW v AS SELECT '+a+' AS x FROM types '+op+' SELECT '+b+' FROM types'
    db.execute(sql);out.append([sql,db.execute('PRAGMA table_info(v)').fetchone()[2]]);db.execute('DROP VIEW v')
print(json.dumps(out))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: Vec<(String, String)> = serde_json::from_slice(&output.stdout).unwrap();
        let mut db = Snapshot {
            unicode_case: true,
            objects: vec![],
            tables: BTreeMap::from([(
                "types".into(),
                Table::from_sql("types", "CREATE TABLE types(i INTEGER,r REAL,t TEXT,b BLOB,n NUMERIC,u)").unwrap(),
            )]),
        };
        for (sql, expected) in &cases {
            db.objects = vec![("view".into(), "v".into(), "v".into(), 0, sql.clone())];
            let view = View::compile(&db, "v").unwrap();
            assert_eq!(&view.schema.columns[0].data_type, expected, "{sql}");
        }
        eprintln!("{} SQLite compound view type comparisons passed", cases.len());
    }
}
