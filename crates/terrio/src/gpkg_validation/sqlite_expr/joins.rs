//! Joined view sources, with explicit name binding and streaming nested loops.
//! The left input streams; right inputs are materialized once per evaluation so
//! nested loops do not repeat whole-database integrity scans or view evaluation.
use super::*;
use parse::Parser;
use select::{Source, View, empty_schema};
use std::cell::Cell;
use std::collections::BTreeSet;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Inner,
    Left,
    Right,
    Full,
}
struct Relation {
    source: Source,
    schema: Schema,
    alias: String,
    start: usize,
    hidden: Option<usize>,
    rowid: bool,
}
#[derive(Clone)]
struct Field {
    name: String,
    expr: Expr,
    qualifier: Option<String>,
}
enum Node {
    Leaf(usize),
    Pair { left: Box<Node>, right: usize, kind: Kind, left_width: usize, on: Option<Expression> },
}
pub(super) struct Join {
    pub schema: Schema,
    relations: Vec<Relation>,
    visible: Vec<Field>,
    root: Node,
}
fn column(index: usize) -> Expr {
    Expr::Column(vec![format!("\0terrio_join:{index}")], 1)
}
fn hidden(name: &str) -> bool {
    ["rowid", "_rowid_", "oid"].iter().any(|s| name.eq_ignore_ascii_case(s))
}
impl Relation {
    fn parse(db: &Snapshot, p: &mut Parser, stack: &mut Vec<String>) -> Result<Self> {
        let mut name = p.name()?;
        if p.eat(".") {
            if !name.eq_ignore_ascii_case("main") {
                return Err(bad("view references another database"));
            }
            name = p.name()?;
        }
        let alias = if p.eat("AS")
            || matches!(p.tokens.get(p.at),Some(lex::Token::Name(n,q)) if *q!=0 || !["WHERE","GROUP","HAVING","ORDER","LIMIT","UNION","INTERSECT","EXCEPT","WINDOW","JOIN","LEFT","RIGHT","FULL","INNER","CROSS","NATURAL","ON","USING"].iter().any(|s|n.eq_ignore_ascii_case(s)))
        {
            p.name()?
        } else {
            name.clone()
        };
        let (source, schema) = if let Some((name, table)) = db.tables.iter().find(|(n, _)| n.eq_ignore_ascii_case(&name)) {
            if table.schema.virtual_module.is_some() {
                return Err(Error::Unsupported("virtual-table view source".into()));
            }
            (Source::Table(name.clone()), table.schema.clone())
        } else {
            let view = View::compile_inner(db, &name, stack)?;
            let schema = view.schema.clone();
            (Source::View(Box::new(view)), schema)
        };
        let rowid = matches!(source, Source::Table(_)) && !schema.without_rowid;
        Ok(Self { source, schema, alias, start: 0, hidden: None, rowid })
    }
    fn field(&self, name: &str) -> Option<Field> {
        if let Some(i) = self.schema.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name)) {
            return Some(Field {
                name: self.schema.columns[i].name.clone(),
                expr: column(self.start + i),
                qualifier: Some(self.alias.clone()),
            });
        }
        if self.rowid && hidden(name) {
            let index = self.schema.rowid_alias.map(|i| self.start + i).or(self.hidden)?;
            let label = self.schema.rowid_alias.map_or_else(|| "rowid".into(), |i| self.schema.columns[i].name.clone());
            return Some(Field { name: label, expr: column(index), qualifier: Some(self.alias.clone()) });
        }
        None
    }
    fn width(&self) -> usize {
        self.schema.columns.len() + usize::from(self.hidden.is_some())
    }
    fn visit(&self, db: &Snapshot, bytes: &[u8], budget: &ViewBudget, visitor: &mut dyn FnMut(&[Value]) -> Result<bool>) -> Result<()> {
        let encoding = u32::from_be_bytes(bytes.get(56..60).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("truncated header"))?);
        match &self.source {
            Source::Table(name) => db.visit_table(bytes, name, |rowid, record| {
                let mut row = db.tables[name].decode(name, record, rowid, encoding)?;
                if self.hidden.is_some() {
                    row.push(rowid.map_or(Value::Null, Value::Int));
                }
                visitor(&row)
            }),
            Source::View(view) => view.visit_budgeted(db, bytes, budget, visitor),
            _ => Err(Error::Unsupported("nested join relation".into())),
        }
    }
}
// Shared by one root view evaluation and every nested view/compound arm.
const MAX_JOIN_WORK: usize = 1_000_000;
const MAX_JOIN_CACHE_BYTES: usize = 128 * 1024 * 1024;
const MAX_JOIN_COPY_BYTES: usize = 1024 * 1024 * 1024;
pub(super) struct ViewBudget {
    remaining: Cell<usize>,
    retained: Cell<usize>,
    byte_limit: usize,
    copy_remaining: Cell<usize>,
}
impl Default for ViewBudget {
    fn default() -> Self {
        Self {
            remaining: Cell::new(MAX_JOIN_WORK),
            retained: Cell::new(0),
            byte_limit: MAX_JOIN_CACHE_BYTES,
            copy_remaining: Cell::new(MAX_JOIN_COPY_BYTES),
        }
    }
}
impl ViewBudget {
    fn charge(&self) -> Result<()> {
        let Some(remaining) = self.remaining.get().checked_sub(1) else {
            return Err(Error::Unsupported(format!("SQLite joined-view evaluation exceeds {MAX_JOIN_WORK} row-work units")));
        };
        self.remaining.set(remaining);
        Ok(())
    }
    pub(super) fn row_bytes(row: &[Value]) -> usize {
        row.iter()
            .try_fold(std::mem::size_of::<Vec<Value>>(), |bytes, value| {
                let payload = match value {
                    Value::Text(text) => text.len(),
                    Value::Blob(blob) => blob.len(),
                    _ => 0,
                };
                bytes.checked_add(std::mem::size_of::<Value>()).and_then(|bytes| bytes.checked_add(payload))
            })
            .unwrap_or(usize::MAX)
    }
    fn retain(&self, row: &[Value]) -> Result<usize> {
        let bytes = Self::row_bytes(row);
        self.reserve(bytes)?;
        Ok(bytes)
    }
    fn reserve(&self, bytes: usize) -> Result<()> {
        let retained = self
            .retained
            .get()
            .checked_add(bytes)
            .filter(|bytes| *bytes <= self.byte_limit)
            .ok_or_else(|| Error::Unsupported(format!("SQLite view working row bytes exceed {}", self.byte_limit)))?;
        self.retained.set(retained);
        Ok(())
    }
    pub(super) fn memory(&self) -> ViewMemory<'_> {
        ViewMemory { budget: self, bytes: 0 }
    }
    fn candidate(&self, left: &[Value], right: &[Value], nulls: usize) -> Result<ViewMemory<'_>> {
        self.charge()?;
        let bytes =
            Self::row_bytes(left).saturating_add(Self::row_bytes(right)).saturating_add(nulls.saturating_mul(std::mem::size_of::<Value>()));
        let remaining = self
            .copy_remaining
            .get()
            .checked_sub(bytes)
            .ok_or_else(|| Error::Unsupported(format!("SQLite joined-view candidate copies exceed {MAX_JOIN_COPY_BYTES} bytes")))?;
        self.copy_remaining.set(remaining);
        let mut memory = self.memory();
        memory.reserve(bytes)?;
        Ok(memory)
    }
}
pub(super) struct ViewMemory<'a> {
    budget: &'a ViewBudget,
    bytes: usize,
}
impl ViewMemory<'_> {
    pub(super) fn reserve(&mut self, bytes: usize) -> Result<()> {
        self.budget.reserve(bytes)?;
        self.bytes += bytes;
        Ok(())
    }
    pub(super) fn release(&mut self, bytes: usize) {
        self.bytes -= bytes;
        self.budget.retained.set(self.budget.retained.get() - bytes);
    }
    pub(super) fn absorb(&mut self, mut other: Self) {
        self.bytes += other.bytes;
        other.bytes = 0;
    }
}
impl Drop for ViewMemory<'_> {
    fn drop(&mut self) {
        self.budget.retained.set(self.budget.retained.get() - self.bytes);
    }
}
struct JoinInputs<'a> {
    rows: Vec<Vec<Vec<Value>>>,
    budget: &'a ViewBudget,
    retained: usize,
}
impl Drop for JoinInputs<'_> {
    fn drop(&mut self) {
        self.budget.retained.set(self.budget.retained.get().saturating_sub(self.retained));
    }
}
impl Join {
    pub fn parse(db: &Snapshot, p: &mut Parser, stack: &mut Vec<String>) -> Result<(Source, Schema, Option<String>)> {
        let first = Relation::parse(db, p, stack)?;
        let Some((kind, natural)) = Self::operator(p)? else {
            return Ok((first.source, first.schema, Some(first.alias)));
        };
        let mut plan = Self { schema: empty_schema(), relations: Vec::new(), visible: Vec::new(), root: Node::Leaf(0) };
        let fields = plan.add(first);
        plan.visible = fields;
        let mut next = Some((kind, natural));
        while let Some((kind, natural)) = next {
            if plan.relations.len() >= 64 {
                return Err(Error::Unsupported("SQLite join exceeds 64 tables".into()));
            }
            let left_width = plan.schema.columns.len();
            let right = Relation::parse(db, p, stack)?;
            let right_fields = plan.add(right);
            let index = plan.relations.len() - 1;
            let mut using = Vec::new();
            if natural {
                for field in &right_fields {
                    if plan.visible.iter().any(|c| c.name.eq_ignore_ascii_case(&field.name)) {
                        using.push(field.name.clone());
                    }
                }
                if p.is("ON") || p.is("USING") {
                    return Err(bad("NATURAL join cannot have ON/USING"));
                }
            } else if p.eat("USING") {
                p.expect("(")?;
                loop {
                    using.push(p.name()?);
                    if !p.eat(",") {
                        break;
                    }
                }
                p.expect(")")?;
            }
            let mut on = None;
            for name in &using {
                let matches =
                    plan.visible.iter().enumerate().filter(|(_, f)| f.name.eq_ignore_ascii_case(name)).map(|(i, _)| i).collect::<Vec<_>>();
                if matches.len() != 1 {
                    return Err(bad("missing or ambiguous left USING column"));
                }
                let left_index = matches[0];
                plan.visible[left_index].qualifier = None;
                let left = plan.visible[left_index].expr.clone();
                let right = right_fields
                    .iter()
                    .find(|f| f.name.eq_ignore_ascii_case(name))
                    .ok_or_else(|| bad("missing right USING column"))?
                    .expr
                    .clone();
                let equal = Expr::Binary("=".into(), Box::new(left.clone()), Box::new(right.clone()));
                on = Some(Expression(if let Some(Expression(old)) = on {
                    Expr::Binary("AND".into(), Box::new(old), Box::new(equal))
                } else {
                    equal
                }));
                if kind == Kind::Right {
                    plan.visible[left_index].expr = right;
                } else if kind == Kind::Full {
                    let kind = select::output_column(&left, &plan.schema, String::new()).data_type;
                    let normalized = if kind.is_empty() {
                        String::new()
                    } else {
                        match affinity(&kind) {
                            Affinity::Numeric if kind.contains("INT") => "INT",
                            Affinity::Numeric => "NUM",
                            Affinity::Real => "REAL",
                            Affinity::Text => "TEXT",
                            Affinity::Blob => "BLOB",
                        }
                        .into()
                    };
                    plan.visible[left_index].expr = Expr::Using(Box::new(left), Box::new(right), normalized);
                }
            }
            plan.visible.extend(right_fields.into_iter().filter(|f| !using.iter().any(|name| name.eq_ignore_ascii_case(&f.name))));
            if p.eat("ON") {
                if natural || !using.is_empty() {
                    return Err(bad("join cannot have both ON and USING"));
                }
                let mut expr = p.expression(0)?;
                plan.bind(&mut expr)?;
                Parser::check_depth(&expr)?;
                let expr = Expression(expr);
                expr.dependencies("", &plan.schema, false)?;
                on = Some(expr);
            }
            let left = std::mem::replace(&mut plan.root, Node::Leaf(0));
            plan.root = Node::Pair { left: Box::new(left), right: index, kind, left_width, on };
            next = Self::operator(p)?;
        }
        let schema = plan.schema.clone();
        Ok((Source::Join(Box::new(plan)), schema, None))
    }
    fn operator(p: &mut Parser) -> Result<Option<(Kind, bool)>> {
        if p.eat(",") {
            return Ok(Some((Kind::Inner, false)));
        }
        let natural = p.eat("NATURAL");
        let kind = if p.eat("LEFT") {
            Some(Kind::Left)
        } else if p.eat("RIGHT") {
            Some(Kind::Right)
        } else if p.eat("FULL") {
            Some(Kind::Full)
        } else if p.eat("INNER") || p.eat("CROSS") {
            Some(Kind::Inner)
        } else {
            None
        };
        if let Some(kind) = kind {
            if p.eat("OUTER") && kind == Kind::Inner {
                return Err(bad("INNER/CROSS cannot be OUTER"));
            }
            p.expect("JOIN")?;
            return Ok(Some((kind, natural)));
        }
        if p.eat("JOIN") {
            Ok(Some((Kind::Inner, natural)))
        } else if natural {
            Err(bad("NATURAL requires JOIN"))
        } else {
            Ok(None)
        }
    }
    fn add(&mut self, mut relation: Relation) -> Vec<Field> {
        relation.start = self.schema.columns.len();
        let mut fields = Vec::new();
        for (i, c) in relation.schema.columns.iter().enumerate() {
            fields.push(Field { name: c.name.clone(), expr: column(relation.start + i), qualifier: Some(relation.alias.clone()) });
            let mut c = c.clone();
            c.name = format!("\0terrio_join:{}", relation.start + i);
            if c.data_type.is_empty() && !relation.schema.is_view || relation.schema.strict && c.data_type == "ANY" {
                c.data_type = "BLOB".into();
            }
            self.schema.columns.push(c);
        }
        if relation.rowid && relation.schema.rowid_alias.is_none() {
            let index = self.schema.columns.len();
            relation.hidden = Some(index);
            self.schema.columns.push(schema::Column {
                name: format!("\0terrio_join:{index}"),
                data_type: "INTEGER".into(),
                not_null: false,
                primary_key: 0,
                default: None,
                collation: "BINARY".into(),
                not_null_declared: false,
                generated: None,
            });
        }
        self.relations.push(relation);
        fields
    }
    fn resolve(&self, parts: &[String]) -> Result<Option<Field>> {
        if parts.len() == 1 && parts[0].starts_with('\0') {
            return Ok(None);
        }
        let name = parts.last().unwrap();
        let fields = if parts.len() > 1 {
            if parts.len() > 2 && !parts[0].eq_ignore_ascii_case("main") {
                return Err(bad("unknown database qualifier"));
            }
            self.relations
                .iter()
                .filter(|r| r.alias.eq_ignore_ascii_case(&parts[parts.len() - 2]))
                .filter_map(|r| r.field(name))
                .collect::<Vec<_>>()
        } else {
            let visible = self.visible.iter().filter(|f| f.name.eq_ignore_ascii_case(name)).cloned().collect::<Vec<_>>();
            if visible.is_empty() && hidden(name) { self.relations.iter().filter_map(|r| r.field(name)).collect() } else { visible }
        };
        if fields.len() > 1 {
            return Err(bad(&format!("ambiguous column {name}")));
        }
        if fields.is_empty() && parts.len() > 1 {
            return Err(bad(&format!("unknown qualified column {name}")));
        }
        Ok(fields.into_iter().next())
    }
    pub fn bind(&self, expr: &mut Expr) -> Result<()> {
        if let Expr::Column(parts, _) = expr
            && let Some(field) = self.resolve(parts)?
        {
            *expr = field.expr;
            return Ok(());
        }
        for child in expr.children_mut() {
            self.bind(child)?;
        }
        Ok(())
    }
    pub fn label(&self, expr: &Expr) -> Result<Option<String>> {
        if let Expr::Column(parts, _) = expr { Ok(self.resolve(parts)?.map(|f| f.name)) } else { Ok(None) }
    }
    pub fn star(&self, prefix: Option<&str>) -> Result<Vec<(String, Expression)>> {
        let fields = if let Some(prefix) = prefix {
            let sources = self.relations.iter().filter(|r| r.alias.eq_ignore_ascii_case(prefix)).collect::<Vec<_>>();
            if sources.is_empty() {
                return Err(bad("unknown star qualifier"));
            }
            sources
                .into_iter()
                .flat_map(|r| {
                    r.schema.columns.iter().enumerate().map(|(i, c)| Field {
                        name: c.name.clone(),
                        expr: column(r.start + i),
                        qualifier: Some(r.alias.clone()),
                    })
                })
                .collect::<Vec<_>>()
        } else {
            self.visible.clone()
        };
        for field in &fields {
            if let Some(prefix) = &field.qualifier {
                self.resolve(&[prefix.clone(), field.name.clone()])?;
            }
        }
        Ok(fields.into_iter().map(|f| (f.name, Expression(f.expr))).collect())
    }
    pub fn visit(&self, db: &Snapshot, bytes: &[u8], budget: &ViewBudget, visitor: &mut dyn FnMut(&[Value]) -> Result<bool>) -> Result<()> {
        let encoding = u32::from_be_bytes(bytes.get(56..60).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("truncated header"))?);
        let mut inputs = JoinInputs { rows: Vec::with_capacity(self.relations.len().saturating_sub(1)), budget, retained: 0 };
        for relation in self.relations.iter().skip(1) {
            let mut rows = Vec::new();
            relation.visit(db, bytes, budget, &mut |row| {
                budget.charge()?;
                inputs.retained += budget.retain(row)?;
                rows.push(row.to_vec());
                Ok(true)
            })?;
            inputs.rows.push(rows);
        }
        self.visit_node(&self.root, db, bytes, encoding, &inputs, visitor)
    }
    fn visit_node(
        &self,
        node: &Node,
        db: &Snapshot,
        bytes: &[u8],
        encoding: u32,
        inputs: &JoinInputs<'_>,
        visitor: &mut dyn FnMut(&[Value]) -> Result<bool>,
    ) -> Result<()> {
        let Node::Pair { left, right, kind, left_width, on } = node else {
            let Node::Leaf(i) = node else { unreachable!() };
            return self.relations[*i].visit(db, bytes, inputs.budget, visitor);
        };
        let relation = &self.relations[*right];
        let rows = &inputs.rows[*right - 1];
        let mut matched = BTreeSet::new();
        let active = Cell::new(true);
        self.visit_node(left, db, bytes, encoding, inputs, &mut |left_row| {
            let mut found = false;
            for (current, right_row) in rows.iter().enumerate() {
                let _candidate_memory = inputs.budget.candidate(left_row, right_row, 0)?;
                let mut row = left_row.to_vec();
                row.extend_from_slice(right_row);
                let context = Context {
                    columns: &self.schema.columns,
                    row: &row,
                    rowid: None,
                    table: None,
                    encoding,
                    strict: false,
                    logical: true,
                    unicode_case: db.unicode_case,
                };
                if let Some(on) = on
                    && truth(&on.evaluate(&context)?) != Some(true)
                {
                    continue;
                }
                found = true;
                if matches!(kind, Kind::Right | Kind::Full) {
                    matched.insert(current);
                }
                active.set(visitor(&row)?);
                if !active.get() {
                    break;
                }
            }
            if !found && matches!(kind, Kind::Left | Kind::Full) && active.get() {
                let _candidate_memory = inputs.budget.candidate(left_row, &[], relation.width())?;
                let mut row = left_row.to_vec();
                row.resize(left_row.len() + relation.width(), Value::Null);
                active.set(visitor(&row)?);
            }
            Ok(active.get())
        })?;
        if active.get() && matches!(kind, Kind::Right | Kind::Full) {
            for (current, right_row) in rows.iter().enumerate() {
                if matched.contains(&current) {
                    continue;
                }
                let _candidate_memory = inputs.budget.candidate(&[], right_row, *left_width)?;
                let mut row = vec![Value::Null; *left_width];
                row.extend_from_slice(right_row);
                if !visitor(&row)? {
                    break;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn joined_sources_are_scanned_once_per_evaluation() {
        use crate::drivers::vector::sqlite as writer;
        let table = |name: &str, count: i64| writer::Table {
            name: name.into(),
            sql: format!("CREATE TABLE {name}(id INTEGER)"),
            rows: (0..count).map(|id| (id + 1, vec![writer::Value::Int(id)])).collect(),
            autoindexes: vec![],
        };
        let objects = [
            ("vb", "SELECT id FROM b"),
            ("sorted_unused", "SELECT * FROM unused ORDER BY id"),
            ("small_sorted", "SELECT * FROM a ORDER BY id DESC LIMIT 5"),
            ("small_sorted_join", "SELECT b.id FROM b CROSS JOIN small_sorted"),
            ("distinct_unused", "SELECT DISTINCT id,payload FROM unused"),
            ("compound_unused", "SELECT * FROM unused UNION SELECT * FROM unused"),
            ("copy_budget", "SELECT a.id FROM a CROSS JOIN unused"),
            ("limited", "SELECT id FROM a LIMIT 1"),
            ("blob_budget", "SELECT a.id FROM a CROSS JOIN b CROSS JOIN unused"),
            ("joined_b", "SELECT b.id FROM b CROSS JOIN b other"),
            ("nested_budget", "SELECT a.id FROM a CROSS JOIN joined_b LIMIT 1"),
            ("expensive", "SELECT x.id FROM a x CROSS JOIN a y CROSS JOIN a z"),
            ("direct", "SELECT a.id,b.id AS right_id FROM a LEFT JOIN b ON a.id=b.id"),
            ("nested", "SELECT a.id,vb.id AS right_id FROM a LEFT JOIN vb ON a.id=vb.id"),
        ]
        .into_iter()
        .map(|(name, query)| writer::SchemaObject {
            kind: "view",
            name: name.into(),
            table: name.into(),
            sql: format!("CREATE VIEW {name} AS {query}"),
        })
        .collect::<Vec<_>>();
        let unused = writer::Table {
            name: "unused".into(),
            sql: "CREATE TABLE unused(id INTEGER,payload BLOB)".into(),
            rows: vec![(1, vec![writer::Value::Int(1), writer::Value::Blob(vec![42; 2 * 1024 * 1024])])],
            autoindexes: vec![],
        };
        let bytes = writer::write_database(&[table("a", 2000), table("b", 3), unused], &objects, 0, 0).unwrap();
        let db = Snapshot::read(&bytes).unwrap();
        for name in ["direct", "nested"] {
            sqlite_repack::take_inspection_stats();
            let mut rows = 0;
            View::compile(&db, name)
                .unwrap()
                .visit_control(&db, &bytes, &mut |row| {
                    assert_eq!(row[0], Value::Int(rows));
                    assert_eq!(row[1], if rows < 3 { Value::Int(rows) } else { Value::Null });
                    rows += 1;
                    Ok(true)
                })
                .unwrap();
            assert_eq!(rows, 2000);
            let stats = sqlite_repack::take_inspection_stats();
            assert_eq!(stats.whole_images, 0, "{name}: {stats:?}");
            assert_eq!(stats.table_roots, 2, "{name}: {stats:?}");
            assert!(stats.pages < 64, "unrelated BLOB pages were traversed: {stats:?}");
        }
        let error = View::compile(&db, "expensive").unwrap().visit_control(&db, &bytes, &mut |_| Ok(true)).unwrap_err();
        assert!(matches!(error, Error::Unsupported(ref message) if message.contains("row-work units")));
        sqlite_repack::take_inspection_stats();
        let mut limited = 0;
        View::compile(&db, "limited")
            .unwrap()
            .visit_control(&db, &bytes, &mut |_| {
                limited += 1;
                Ok(true)
            })
            .unwrap();
        let stats = sqlite_repack::take_inspection_stats();
        assert_eq!(limited, 1);
        assert_eq!(stats.whole_images, 0);
        assert_eq!(stats.table_roots, 1);
        assert!(stats.pages <= 2, "LIMIT did not stop the B-tree walk: {stats:?}");
        for name in ["sorted_unused", "distinct_unused", "compound_unused"] {
            let budget = ViewBudget { byte_limit: 1024 * 1024, ..ViewBudget::default() };
            let error = View::compile(&db, name).unwrap().visit_budgeted(&db, &bytes, &budget, &mut |_| Ok(true)).unwrap_err();
            assert!(error.to_string().contains("working row bytes"), "{name}: {error}");
            assert_eq!(budget.retained.get(), 0, "{name}");
        }
        let budget = ViewBudget { byte_limit: 4096, ..ViewBudget::default() };
        let mut sorted_rows = 0;
        View::compile(&db, "small_sorted_join")
            .unwrap()
            .visit_budgeted(&db, &bytes, &budget, &mut |_| {
                sorted_rows += 1;
                Ok(true)
            })
            .unwrap();
        assert_eq!(sorted_rows, 15);
        assert_eq!(budget.retained.get(), 0);
        let budget = ViewBudget { copy_remaining: Cell::new(3 * 1024 * 1024), ..ViewBudget::default() };
        let mut copied = 0;
        let error = View::compile(&db, "copy_budget")
            .unwrap()
            .visit_budgeted(&db, &bytes, &budget, &mut |_| {
                copied += 1;
                Ok(true)
            })
            .unwrap_err();
        assert!(error.to_string().contains("candidate copies"), "{error}");
        assert_eq!(copied, 1);
        assert_eq!(budget.retained.get(), 0);
        let budget = ViewBudget { byte_limit: 1024 * 1024, ..ViewBudget::default() };
        let error = View::compile(&db, "blob_budget").unwrap().visit_budgeted(&db, &bytes, &budget, &mut |_| Ok(true)).unwrap_err();
        assert!(matches!(error, Error::Unsupported(ref message) if message.contains("working row bytes")));
        assert_eq!(budget.retained.get(), 0);
        let row_bytes = std::mem::size_of::<Vec<Value>>() + std::mem::size_of::<Value>();
        let budget = ViewBudget { byte_limit: row_bytes * 10, ..ViewBudget::default() };
        let error = View::compile(&db, "nested_budget").unwrap().visit_budgeted(&db, &bytes, &budget, &mut |_| Ok(true)).unwrap_err();
        assert!(matches!(error, Error::Unsupported(ref message) if message.contains("working row bytes")));
        assert_eq!(budget.retained.get(), 0);
        let budget = ViewBudget { remaining: Cell::new(20), ..ViewBudget::default() };
        let error = View::compile(&db, "nested_budget").unwrap().visit_budgeted(&db, &bytes, &budget, &mut |_| Ok(true)).unwrap_err();
        assert!(matches!(error, Error::Unsupported(ref message) if message.contains("row-work units")));
    }
    #[test]
    fn joins_bind_names_and_bound_source_count() {
        let mut db = Snapshot {
            unicode_case: true,
            objects: vec![],
            tables: BTreeMap::from([("t".into(), Table::from_sql("t", "CREATE TABLE t(id INTEGER)").unwrap())]),
        };
        for count in [2, 64, 65] {
            let mut sql = "CREATE VIEW v AS SELECT a0.id FROM t a0".to_string();
            for i in 1..count {
                sql.push_str(&format!(" JOIN t a{i} ON 1"));
            }
            db.objects = vec![("view".into(), "v".into(), "v".into(), 0, sql)];
            assert_eq!(View::compile(&db, "v").is_ok(), count <= 64);
        }
        for sql in [
            "CREATE VIEW v AS SELECT id FROM t a JOIN t b ON a.id=b.id",
            "CREATE VIEW v AS SELECT a.id FROM t a JOIN t b USING(missing)",
            "CREATE VIEW v AS SELECT a.id FROM t a JOIN t b ON later.id=a.id",
            "CREATE VIEW v AS SELECT a.id FROM t a INNER OUTER JOIN t b ON a.id=b.id",
        ] {
            db.objects = vec![("view".into(), "v".into(), "v".into(), 0, sql.into())];
            assert!(View::compile(&db, "v").is_err(), "{sql}");
        }
    }
}
