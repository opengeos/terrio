// Written to match the behavior of:
//   SQLite version-3.54.1:
//     src/select.c
// No code is translated from these sources.

//! Read-only, streaming view projections. Query forms not implemented here are
//! rejected rather than silently omitting a registered view from validation.
use super::*;
use compound::Compound;
use joins::{Join, ViewBudget};
use order::{Order, Sorter, Window};
use parse::Parser;
use std::cell::Cell;
use std::collections::BTreeSet;

pub(super) enum Source {
    Table(String),
    View(Box<View>),
    Unit,
    Compound(Compound),
    Join(Box<Join>),
}
enum Item {
    Star(Option<String>),
    Expression(Expression, String, bool),
}
pub(in super::super) struct View {
    pub schema: Schema,
    pub(super) source_schema: Schema,
    pub(super) source: Source,
    pub(super) qualifier: Option<String>,
    pub(super) projections: Vec<Expression>,
    predicate: Option<Expression>,
    distinct: bool,
    order: Vec<Order>,
    window: Window,
}
pub(super) fn empty_schema() -> Schema {
    Schema {
        columns: vec![],
        foreign_keys: vec![],
        unique: vec![],
        without_rowid: true,
        is_view: true,
        rowid_alias: None,
        virtual_module: None,
        strict: false,
        checks: vec![],
    }
}
pub(super) fn output_column(expr: &Expr, schema: &Schema, name: String) -> schema::Column {
    fn column<'a>(expr: &Expr, schema: &'a Schema) -> Option<&'a schema::Column> {
        if let Expr::Column(parts, _) = expr {
            schema.columns.iter().find(|c| c.name.eq_ignore_ascii_case(parts.last().unwrap()))
        } else {
            None
        }
    }
    fn expression_type(expr: &Expr, schema: &Schema) -> Option<String> {
        let kind = match expr {
            Expr::Column(..) => column(expr, schema)?.data_type.clone(),
            Expr::Collate(inner, _) => expression_type(inner, schema)?,
            Expr::Cast(_, kind) => kind.clone(),
            _ => return None,
        };
        Some(
            match affinity(&kind) {
                Affinity::Numeric if kind.contains("INT") => "INT",
                Affinity::Numeric => "NUM",
                Affinity::Text => "TEXT",
                Affinity::Real => "REAL",
                Affinity::Blob => "BLOB",
            }
            .into(),
        )
    }
    let data_type = if let Some(column) = column(expr, schema) {
        if column.data_type.is_empty() && !schema.is_view || schema.strict && column.data_type == "ANY" {
            "BLOB".into()
        } else {
            column.data_type.clone()
        }
    } else if let Expr::Using(_, _, kind) = expr {
        kind.clone()
    } else if !schema.without_rowid
        && matches!(expr, Expr::Column(parts, _) if ["rowid","oid","_rowid_"].iter().any(|s| parts.last().unwrap().eq_ignore_ascii_case(s)))
    {
        "INTEGER".into()
    } else {
        expression_type(expr, schema).unwrap_or_default()
    };
    schema::Column {
        name,
        data_type,
        not_null: false,
        primary_key: 0,
        default: None,
        collation: expression_collation(expr, schema),
        not_null_declared: false,
        generated: None,
    }
}
pub(super) fn expression_collation(expr: &Expr, schema: &Schema) -> String {
    if let Some(name) = expr.explicit_collation() {
        return name.into();
    }
    match expr {
        Expr::Column(parts, _) => schema
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(parts.last().unwrap()))
            .map_or_else(|| "BINARY".into(), |c| c.collation.clone()),
        Expr::Cast(inner, _) | Expr::Unary(_, inner) | Expr::Using(inner, _, _) => expression_collation(inner, schema),
        _ => "BINARY".into(),
    }
}
impl View {
    pub fn compile(db: &Snapshot, name: &str) -> Result<Self> {
        Self::compile_inner(db, name, &mut Vec::new())
    }
    pub(super) fn compile_inner(db: &Snapshot, name: &str, stack: &mut Vec<String>) -> Result<Self> {
        if stack.len() >= 128 || stack.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            return Err(bad("recursive view dependency"));
        }
        let object =
            db.objects.iter().find(|o| o.0 == "view" && o.1.eq_ignore_ascii_case(name)).ok_or_else(|| bad("view does not exist"))?;
        stack.push(object.1.clone());
        let result = Self::compile_sql(db, &object.4, stack);
        stack.pop();
        result
    }
    pub(super) fn compile_sql(db: &Snapshot, sql: &str, stack: &mut Vec<String>) -> Result<Self> {
        let lex::Spanned { tokens, spans } = lex::spanned(sql)?;
        let mut p = Parser { tokens, at: 0, depth: 0 };
        p.expect("CREATE")?;
        p.expect("VIEW")?;
        if p.eat("IF") {
            p.expect("NOT")?;
            p.expect("EXISTS")?;
        }
        p.name()?;
        if p.eat(".") {
            p.name()?;
        }
        let mut names = Vec::new();
        if p.eat("(") {
            loop {
                names.push(p.name()?);
                if !p.eat(",") {
                    break;
                }
            }
            p.expect(")")?;
        }
        p.expect("AS")?;
        if let Some(compound) = Compound::compile(db, sql, &mut p, &spans, stack)? {
            let mut schema = compound.schema();
            let projections: Vec<_> = schema.columns.iter().map(|c| Expression(Expr::Column(vec![c.name.clone()], 1))).collect();
            let order = Order::parse(&mut p)?.into_iter().map(|term| term.bind_compound(&compound, &schema)).collect::<Result<Vec<_>>>()?;
            let window = Window::parse(&mut p)?;
            p.eat(";");
            if p.at != p.tokens.len() {
                return Err(Error::Unsupported("SQLite compound view clause".into()));
            }
            let source_schema = schema.clone();
            Self::rename(&mut schema, &names)?;
            return Ok(Self {
                schema,
                source_schema,
                source: Source::Compound(compound),
                qualifier: None,
                projections,
                predicate: None,
                distinct: false,
                order,
                window,
            });
        }
        p.expect("SELECT")?;
        let distinct = p.eat("DISTINCT");
        if !distinct {
            p.eat("ALL");
        }
        let mut items = Vec::new();
        loop {
            if p.eat("*") {
                items.push(Item::Star(None));
            } else if p.tokens.get(p.at + 1).is_some_and(|t| t.is(".")) && p.tokens.get(p.at + 2).is_some_and(|t| t.is("*")) {
                let qualifier = p.name()?;
                p.at += 2;
                items.push(Item::Star(Some(qualifier)));
            } else {
                let start = p.at;
                let expr = p.expression(0)?;
                Parser::check_depth(&expr)?;
                let end = p.at;
                let aliased = p.eat("AS")
                    || matches!(p.tokens.get(p.at),Some(lex::Token::Name(_,q)) if *q != 0)
                    || matches!(p.tokens.get(p.at),Some(lex::Token::Name(n,0)) if !["FROM","WHERE","GROUP","HAVING","ORDER","LIMIT","UNION","INTERSECT","EXCEPT","WINDOW"].iter().any(|s|n.eq_ignore_ascii_case(s)));
                let label = if aliased { p.name()? } else { sql[spans[start].0..spans[end - 1].1].into() };
                items.push(Item::Expression(Expression(expr), label, aliased));
            }
            if !p.eat(",") {
                break;
            }
        }
        let (source, source_schema, qualifier) =
            if p.eat("FROM") { Join::parse(db, &mut p, stack)? } else { (Source::Unit, empty_schema(), None) };
        let mut predicate = if p.eat("WHERE") {
            let expr = p.expression(0)?;
            Parser::check_depth(&expr)?;
            Some(Expression(expr))
        } else {
            None
        };
        let raw_order = Order::parse(&mut p)?;
        let window = Window::parse(&mut p)?;
        p.eat(";");
        if p.at != p.tokens.len() {
            return Err(Error::Unsupported("SQLite view query clause".into()));
        }
        let allow_rowid = matches!(source, Source::Table(_)) && !source_schema.without_rowid;
        let binding = qualifier.as_deref().unwrap_or("");
        let mut projections = Vec::new();
        let mut schema = empty_schema();
        for item in items {
            match item {
                Item::Star(prefix) => {
                    if let Source::Join(join) = &source {
                        for (name, expr) in join.star(prefix.as_deref())? {
                            schema.columns.push(output_column(&expr.0, &source_schema, name));
                            projections.push(expr);
                        }
                        continue;
                    }
                    if qualifier.is_none() || prefix.as_deref().is_some_and(|s| !s.eq_ignore_ascii_case(binding)) {
                        return Err(bad("unknown star qualifier"));
                    }
                    for column in &source_schema.columns {
                        let expression = Expression(Expr::Column(vec![column.name.clone()], 1));
                        schema.columns.push(output_column(&expression.0, &source_schema, column.name.clone()));
                        projections.push(expression);
                    }
                }
                Item::Expression(mut expr, mut label, aliased) => {
                    if let Source::Join(join) = &source {
                        if !aliased && let Some(name) = join.label(&expr.0)? {
                            label = name;
                        }
                        join.bind(&mut expr.0)?;
                    }
                    Parser::check_depth(&expr.0)?;
                    expr.dependencies(binding, &source_schema, allow_rowid)?;
                    if !matches!(source, Source::Join(_))
                        && !aliased
                        && let Expr::Column(parts, _) = &expr.0
                    {
                        let field = parts.last().unwrap();
                        if let Some(column) = source_schema.columns.iter().find(|c| c.name.eq_ignore_ascii_case(field)) {
                            label = column.name.clone();
                        } else if allow_rowid && ["rowid", "_rowid_", "oid"].iter().any(|s| field.eq_ignore_ascii_case(s)) {
                            label = source_schema.rowid_alias.map_or_else(|| "rowid".into(), |i| source_schema.columns[i].name.clone());
                        }
                    }
                    schema.columns.push(output_column(&expr.0, &source_schema, label));
                    projections.push(expr);
                }
            }
        }
        if let Some(predicate) = &mut predicate {
            if let Source::Join(join) = &source {
                join.bind(&mut predicate.0)?;
            }
            order::expand(&mut predicate.0, &source_schema, &schema, &projections);
            Parser::check_depth(&predicate.0)?;
            predicate.dependencies(binding, &source_schema, allow_rowid)?;
        }
        let order = raw_order
            .into_iter()
            .map(|term| {
                term.bind(
                    &source_schema,
                    &schema,
                    &projections,
                    binding,
                    allow_rowid,
                    if let Source::Join(join) = &source { Some(join.as_ref()) } else { None },
                )
            })
            .collect::<Result<Vec<_>>>()?;
        Self::rename(&mut schema, &names)?;
        if distinct && schema.columns.iter().any(|c| !matches!(c.collation.as_str(), "BINARY" | "NOCASE" | "RTRIM")) {
            return Err(Error::Unsupported("SQLite view DISTINCT collation".into()));
        }
        Ok(Self { schema, source_schema, source, qualifier, projections, predicate, distinct, order, window })
    }
    fn rename(schema: &mut Schema, names: &[String]) -> Result<()> {
        if !names.is_empty() && names.len() != schema.columns.len() {
            return Err(bad("view column count differs from SELECT"));
        }
        for (i, column) in schema.columns.iter_mut().enumerate() {
            if !names.is_empty() {
                column.name = names[i].clone();
            }
        }
        // SQLite disambiguates repeated result names, including explicit lists.
        let mut used = Vec::<String>::new();
        for column in &mut schema.columns {
            let original = column.name.clone();
            let mut n = 0;
            while used.iter().any(|s| s.eq_ignore_ascii_case(&column.name)) {
                n += 1;
                let base = original
                    .rsplit_once(':')
                    .filter(|(_, suffix)| suffix.bytes().all(|b| b.is_ascii_digit()))
                    .map_or(original.as_str(), |(base, _)| base);
                column.name = format!("{base}:{n}");
            }
            used.push(column.name.clone());
        }
        Ok(())
    }
    pub fn visit(&self, db: &Snapshot, bytes: &[u8], visitor: &mut dyn FnMut(&[Value]) -> Result<()>) -> Result<()> {
        self.visit_control(db, bytes, &mut |row| {
            visitor(row)?;
            Ok(true)
        })
    }
    pub(super) fn visit_control(&self, db: &Snapshot, bytes: &[u8], visitor: &mut dyn FnMut(&[Value]) -> Result<bool>) -> Result<()> {
        self.visit_budgeted(db, bytes, &ViewBudget::default(), visitor)
    }
    pub(super) fn visit_budgeted(
        &self,
        db: &Snapshot,
        bytes: &[u8],
        budget: &ViewBudget,
        visitor: &mut dyn FnMut(&[Value]) -> Result<bool>,
    ) -> Result<()> {
        let encoding = u32::from_be_bytes(bytes.get(56..60).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("truncated header"))?);
        let (count, offset) = self.window.evaluate(encoding, db.unicode_case)?;
        if count == Some(0) {
            return Ok(());
        }
        let mut seen = BTreeSet::new();
        let mut distinct_memory = budget.memory();
        let mut skipped = 0;
        let mut emitted = 0;
        let active = Cell::new(true);
        let mut sorter = Sorter::new(&self.order, count.map(|n| n.saturating_add(offset)), encoding, budget);
        let mut project = |row: &[Value], rowid| {
            if !active.get() {
                return Ok(false);
            }
            let context = Context {
                columns: &self.source_schema.columns,
                row,
                rowid,
                table: self.qualifier.as_deref(),
                encoding,
                strict: self.source_schema.strict,
                logical: true,
                unicode_case: db.unicode_case,
            };
            if let Some(predicate) = &self.predicate
                && truth(&predicate.evaluate(&context)?) != Some(true)
            {
                return Ok(true);
            }
            if !self.distinct && self.order.is_empty() && skipped < offset {
                skipped += 1;
                return Ok(true);
            }
            let keys = sorter.keys(&context)?;
            if !self.distinct && !self.order.is_empty() && !sorter.admits(&keys) {
                return Ok(true);
            }
            let result = self.projections.iter().map(|e| e.evaluate(&context)).collect::<Result<Vec<_>>>()?;
            if self.distinct {
                // Reserve before cloning projected values into comparison keys.
                // Three row sizes conservatively cover keys, UTF-16 text and tree overhead.
                let mut pending = budget.memory();
                pending.reserve(ViewBudget::row_bytes(&result).saturating_mul(3))?;
                let key: Vec<_> = result
                    .iter()
                    .zip(&self.schema.columns)
                    .map(|(v, c)| {
                        let mut c = c.clone();
                        c.data_type = "BLOB".into();
                        integrity::atom(v, &c, false).unwrap_or(integrity::Atom::Null)
                    })
                    .collect();
                if !seen.insert(key) {
                    return Ok(true);
                }
                distinct_memory.absorb(pending);
            }
            if !self.order.is_empty() {
                sorter.push(keys, result)?;
                return Ok(true);
            }
            if skipped < offset {
                skipped += 1;
                return Ok(true);
            }
            emitted += 1;
            let more = visitor(&result)? && count.is_none_or(|n| emitted < n);
            active.set(more);
            Ok(more)
        };
        match &self.source {
            Source::Unit => {
                project(&[], None)?;
            }
            Source::Join(join) => join.visit(db, bytes, budget, &mut |row| project(row, None))?,
            Source::Compound(compound) => compound.visit(db, bytes, budget, &mut |row| project(row, None))?,
            Source::View(view) => view.visit_budgeted(db, bytes, budget, &mut |row| project(row, None))?,
            Source::Table(name) => {
                db.visit_table(bytes, name, |rowid, record| project(&db.tables[name].decode(name, record, rowid, encoding)?, rowid))?;
            }
        }
        if !self.order.is_empty() {
            let (rows, _sort_memory) = sorter.finish();
            for row in rows.into_iter().skip(offset).take(count.unwrap_or(usize::MAX)) {
                if !visitor(&row)? {
                    break;
                }
            }
        }
        Ok(())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires GDAL 3.13.3 and real comparison data"]
    fn view_schemas_and_rows_match_sqlite() {
        let directory = tempfile::tempdir().unwrap();
        let data = std::env::var("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA");
        let script = r#"
import json,pathlib,shutil,sqlite3,sys
from osgeo import gdal
gdal.UseExceptions();root=pathlib.Path(sys.argv[1]);data=pathlib.Path(sys.argv[2]);base=root/'base.gpkg';cases=[]
gdal.VectorTranslate(str(base),str(data/'countries.gpkg'),layerName='countries',layerCreationOptions=['SPATIAL_INDEX=NO']).Close()
with sqlite3.connect(base) as db:dump='\n'.join(db.iterdump())
def value(v):return {'blob':v.hex()} if isinstance(v,bytes) else v
queries=[
 'SELECT * FROM sample', 'SELECT s.* FROM sample AS s',
 'SELECT ID,s.V,rowid,_rowid_,oid FROM sample s',
 'SELECT "missing",1 FROM sample',
 'SELECT ID AS identifier,v,w,b,extra FROM sample WHERE id>1',
 'SELECT s.id,s.v FROM sample s WHERE w IS NOT NULL AND w>=0',
 'SELECT id,v FROM main.sample WHERE w IN (0,2,NULL)',
 'SELECT id,v FROM sample WHERE NULL',
 'SELECT id,upper(v) AS uppercase FROM sample WHERE v LIKE \'a%\'',
 'SELECT rowid AS rid,id,extra FROM sample',
 'SELECT DISTINCT v FROM sample', 'SELECT DISTINCT w FROM sample',
 'SELECT DISTINCT v COLLATE BINARY AS v FROM sample',
 'SELECT DISTINCT lower(v) COLLATE RTRIM AS v FROM sample',
 'SELECT id,id,id AS "id:1",id AS ID FROM sample',
 'SELECT id AS "quoted identifier",v AS "水" FROM sample',
 'SELECT id,CASE WHEN w>0 THEN v ELSE NULL END AS computed FROM sample',
 'SELECT id,CAST(w AS TEXT) AS text,CAST(w AS INTEGER) AS integer FROM sample',
 'SELECT id COLLATE NOCASE AS a,CAST(id AS INTEGER) COLLATE RTRIM AS b FROM sample',
 'SELECT id,v,w,b,extra FROM middle WHERE id>1',
 'SELECT 1 AS id,\'constant\' AS value',
 'SELECT id,lower(v) FROM sample',
 'SELECT id,b FROM sample WHERE b IS NOT NULL',
 'SELECT id,CAST(b AS INTEGER),CAST(b AS REAL),CAST(b AS NUMERIC) FROM sample',
 "SELECT CAST(X'31003200' AS TEXT),CAST(X'31003200' AS INTEGER),CAST(X'31003200' AS REAL),CAST(X'31003200' AS NUMERIC)",
 "SELECT CAST(X'31' AS TEXT),CAST(X'31' AS INTEGER),CAST(X'31' AS REAL),CAST(X'31' AS NUMERIC)",

 'SELECT id,v FROM sample WHERE id<4 UNION ALL SELECT id,v FROM sample WHERE id>8',
 'SELECT id,v FROM sample WHERE id<4 UNION SELECT id,v FROM sample WHERE id<6',
 'SELECT id,v FROM sample WHERE id<4 INTERSECT SELECT id,v FROM sample WHERE id>1',
 'SELECT id,v FROM sample WHERE id<4 EXCEPT SELECT id,v FROM sample WHERE id>1',
 'SELECT id,v FROM sample UNION ALL SELECT id,v FROM sample EXCEPT SELECT id,v FROM sample WHERE id<4',
 'SELECT id,v FROM sample WHERE id<4 UNION SELECT id,v FROM sample WHERE id>8 INTERSECT SELECT id,v FROM sample WHERE id<10',
 'SELECT id AS x FROM sample WHERE id<3 UNION ALL SELECT id AS y FROM sample WHERE id>8 ORDER BY y DESC LIMIT 2 OFFSET 1',
 'SELECT s.id FROM sample s UNION SELECT id FROM sample ORDER BY s.id DESC LIMIT 3',
 'SELECT id+1 AS x FROM sample UNION SELECT id+1 AS y FROM sample ORDER BY id+1 DESC LIMIT 2',
 'SELECT id FROM sample UNION ALL SELECT id FROM sample ORDER BY 1 DESC LIMIT 4',
 'SELECT v FROM sample UNION SELECT v FROM sample ORDER BY v DESC LIMIT 5',
 'SELECT v FROM sample INTERSECT SELECT lower(v) FROM sample ORDER BY 1',
 'SELECT w FROM sample UNION SELECT w FROM sample',
 'SELECT b FROM sample UNION SELECT b FROM sample',
 'SELECT w FROM sample INTERSECT SELECT id FROM sample',
 'SELECT id FROM sample UNION SELECT w FROM sample',
 'SELECT w FROM sample UNION ALL SELECT id FROM sample',
 'SELECT id FROM sample WHERE 0 UNION SELECT id FROM sample WHERE id<3',
 'SELECT * FROM compounded ORDER BY id DESC LIMIT 2',
 'SELECT 1 AS x UNION ALL SELECT abs(-9223372036854775808) LIMIT 1',
 "SELECT 'A' AS v INTERSECT SELECT 'a' UNION SELECT 'b' COLLATE NOCASE",
 "SELECT 'A' COLLATE NOCASE AS v UNION ALL SELECT 'a' EXCEPT SELECT 'z'",
 "SELECT 'A' COLLATE NOCASE AS v UNION SELECT 'a' EXCEPT SELECT 'z'",
 "SELECT 'a' COLLATE NOCASE AS v UNION ALL SELECT 'A' INTERSECT SELECT 'a'",
 "SELECT 'A' COLLATE BINARY AS x UNION ALL SELECT 'a' COLLATE NOCASE AS y ORDER BY y",
 "SELECT 'b' COLLATE BINARY AS x UNION ALL SELECT 'A' COLLATE NOCASE AS y UNION ALL SELECT 'a' COLLATE NOCASE ORDER BY y",
 'SELECT 1 AS x UNION ALL SELECT 2 AS y ORDER BY y+0',
 'SELECT id FROM sample UNION SELECT id,v FROM sample',
 'SELECT id FROM sample ORDER BY id UNION SELECT id FROM sample',
 'SELECT id FROM sample UNION SELECT id FROM sample ORDER BY 2',
 'SELECT id FROM sample UNION SELECT id FROM sample ORDER BY wrong.id',
 'SELECT CASE WHEN id>2 THEN id ELSE 0 END AS x FROM sample UNION SELECT 0 ORDER BY CASE WHEN ID>2 THEN ID ELSE 0 END',
 "SELECT 'foo' AS x UNION SELECT 'bar' ORDER BY \"foo\"",

 'SELECT s.id,l.label FROM sample s JOIN lookup l ON s.id=l.id',
 'SELECT s.id,l.label FROM sample s,lookup l WHERE s.id=l.id',
 'SELECT s.id,l.label FROM sample s CROSS JOIN lookup l ON s.id=l.id',
 'SELECT s.id,l.label FROM sample s LEFT JOIN lookup l ON s.id=l.id ORDER BY s.id,l.label',
 'SELECT s.id,l.id,l.label FROM sample s RIGHT JOIN lookup l ON s.id=l.id',
 'SELECT s.id,l.id,l.label FROM sample s FULL OUTER JOIN lookup l ON s.id=l.id',
 'SELECT s.id,l.id,m.id FROM sample s LEFT JOIN lookup l ON s.id=l.id LEFT JOIN lookup m ON l.id=m.id',
 'SELECT s.rowid,l.rowid FROM sample s JOIN lookup l ON s.id=l.id',
 'SELECT main.s.id,l.label FROM sample s JOIN lookup l ON s.id=l.id',
 'SELECT s.id,length(l.label) AS n FROM sample s LEFT JOIN lookup l ON s.id=l.id WHERE n>2 ORDER BY n DESC,s.id',
 'SELECT s.id,length(l.label) AS n FROM sample s LEFT JOIN lookup l ON s.id=l.id ORDER BY n+1,s.id',
 'SELECT s.id AS id,l.label FROM sample s JOIN lookup l ON s.id=l.id ORDER BY id DESC',
 'SELECT s.*,l.* FROM sample s JOIN lookup l ON s.id=l.id',
 'SELECT s.id,l.label FROM sample s JOIN lookup_view l ON s.id=l.id ORDER BY s.id,l.label',
 'SELECT s.id,l.label FROM sample s LEFT JOIN lookup l ON 0',
 'SELECT s.id,l.label FROM sample s RIGHT JOIN lookup l ON 0',
 'SELECT s.id,l.label FROM sample s FULL JOIN lookup l ON 0',
 'SELECT s.id,l.label FROM sample s LEFT JOIN lookup l ON NULL',
 'SELECT DISTINCT s.id FROM sample s JOIN lookup l ON s.id=l.id ORDER BY 1',
 'SELECT s.id,l.id FROM sample s JOIN lookup l USING(id) UNION SELECT id,id FROM lookup ORDER BY 1,2',
 'SELECT r.id,typeof(r.id) AS kind FROM real_joined r',
 'SELECT id FROM sample s JOIN lookup l ON s.id=l.id',
 'SELECT rowid FROM sample s JOIN lookup l ON s.id=l.id',
 'SELECT s.* FROM sample s JOIN lookup s ON 1',
 'SELECT * FROM sample s JOIN lookup s ON 1',
 'SELECT wrong.s.id FROM sample s JOIN lookup l ON s.id=l.id',
 'SELECT s.id FROM sample s JOIN lookup l USING(missing)',
 'SELECT s.id FROM sample s NATURAL JOIN lookup l ON 1',
 'SELECT id,v,2 AS chosen FROM sample WHERE id>1 AND chosen=2 LIMIT 2',
 'SELECT * FROM sample s JOIN lookup s USING(id)',
 'SELECT s.* FROM sample s JOIN disjoint s ON 1',
 'SELECT * FROM sample s JOIN disjoint s ON 1',
 'SELECT rowid FROM sample s JOIN no_rowid w ON s.id=w.id',
 'SELECT rowid FROM sample s JOIN rowid_shadow r ON 1',
 'SELECT _rowid_ FROM sample s JOIN rowid_shadow r ON 1',
 'SELECT r,t,p,typeof(r),typeof(t),typeof(p) FROM affinity_precedence',
 'SELECT id FROM empty',
 'SELECT id,v,w FROM sample ORDER BY v DESC NULLS FIRST,w ASC,id DESC',
 'SELECT id,v FROM sample ORDER BY lower(v) COLLATE RTRIM DESC LIMIT 2 OFFSET 1',
 'SELECT id,v FROM sample ORDER BY 2,1 DESC LIMIT 2',
 'SELECT id,v FROM sample ORDER BY 2 COLLATE BINARY DESC LIMIT 3',
 'SELECT id,v FROM sample ORDER BY +2 DESC LIMIT 3',
 'SELECT id,v FROM sample ORDER BY 2147483648 LIMIT 3',
 'SELECT id,v FROM sample ORDER BY -2147483648 LIMIT 3',
 'SELECT id,-id AS v FROM sample ORDER BY v',
 'SELECT id,-id AS v FROM sample ORDER BY +v',
 'SELECT id,-id AS v FROM sample ORDER BY abs(v),id DESC',
 'SELECT id,-id AS x FROM sample ORDER BY +x',
 'SELECT id,-id AS x FROM sample ORDER BY abs(x) DESC LIMIT 2',
 'SELECT id FROM sample ORDER BY w ASC NULLS LAST',
 'SELECT id FROM sample ORDER BY w DESC NULLS FIRST',
 'SELECT id,b FROM sample ORDER BY b DESC,id',
 'SELECT id,v FROM sample ORDER BY v COLLATE BINARY',
 'SELECT id,v FROM sample ORDER BY v COLLATE RTRIM',
 'SELECT id,v FROM sample ORDER BY v COLLATE NOCASE',
 'SELECT DISTINCT v FROM sample ORDER BY v DESC LIMIT 2 OFFSET 1',
 'SELECT DISTINCT lower(v COLLATE RTRIM) AS v FROM sample ORDER BY v',
 'SELECT DISTINCT v FROM sample LIMIT 2 OFFSET 1',
 'SELECT id FROM sample LIMIT 2',
 'SELECT id FROM sample LIMIT -1 OFFSET 2',
 'SELECT id FROM sample LIMIT 2 OFFSET -5',
 'SELECT id FROM sample LIMIT 1,2',
 "SELECT id FROM sample LIMIT '2.0' OFFSET 1.0",
 "SELECT id FROM sample LIMIT 0 OFFSET 'bad'",
 "SELECT id FROM sample ORDER BY id LIMIT 0 OFFSET NULL",
 'SELECT id FROM sample LIMIT 100 OFFSET 100',
 'SELECT id FROM sample LIMIT abs(-2) OFFSET 1+1',
 'SELECT id FROM sample LIMIT 9223372036854775807 OFFSET 9223372036854775807',
 'SELECT id FROM sample WHERE CASE WHEN id=3 THEN abs(-9223372036854775808) ELSE 1 END LIMIT 1',
 'SELECT CASE WHEN id=1 THEN abs(-9223372036854775808) ELSE id END AS x FROM sample LIMIT 1 OFFSET 1',
 'SELECT CASE WHEN id=3 THEN abs(-9223372036854775808) ELSE id END AS x FROM sample ORDER BY id LIMIT 1',
 'SELECT CASE WHEN id=3 THEN abs(-9223372036854775808) ELSE id END AS x FROM sample ORDER BY id LIMIT 1 OFFSET 1',
 'SELECT id FROM limited ORDER BY id DESC LIMIT 1',
 'SELECT id FROM sample ORDER BY 0',
 'SELECT id FROM sample ORDER BY 2',
 'SELECT id FROM sample LIMIT NULL',
 "SELECT id FROM sample LIMIT 'bad'",
 'SELECT id FROM sample LIMIT 1.5',
 "SELECT id FROM sample LIMIT X'31'",
 'SELECT id FROM sample LIMIT 9223372036854775808',
 'SELECT id FROM sample LIMIT 2 OFFSET NULL',
 'SELECT id FROM sample LIMIT id',
 'SELECT id FROM sample ORDER BY missing',

]
for join in ['JOIN','LEFT JOIN','RIGHT JOIN','FULL JOIN','NATURAL JOIN','NATURAL LEFT JOIN','NATURAL FULL JOIN']:
 suffix='' if join.startswith('NATURAL') else ' USING(id)'
 queries.extend(['SELECT * FROM sample s '+join+' lookup l'+suffix,
                 'SELECT id,v,label FROM sample s '+join+' lookup l'+suffix+' ORDER BY id,label',
                 'SELECT s.id,l.id FROM sample s '+join+' lookup l'+suffix])
for join in ['JOIN','LEFT JOIN','RIGHT JOIN','FULL JOIN']:
 queries.append('SELECT k,a,b FROM caseleft '+join+' caseright USING(k) ORDER BY k,a,b')
for left,right in [('NULL','NULL'),('1','1.0'),("'1'",'1'),("X'31'","'1'"),("'A'","'a' COLLATE NOCASE"),("'A' COLLATE BINARY","'a' COLLATE NOCASE"),("'Ā'","'é'"),("'a '","'a' COLLATE RTRIM")]:
 for op in ['UNION','UNION ALL','INTERSECT','EXCEPT']:
  query='SELECT '+left+' AS x '+op+' SELECT '+right
  queries.extend([query,query+' ORDER BY 1',query+' ORDER BY 1 DESC'])
for encoding in ['UTF-8','UTF-16le','UTF-16be']:
 template=root/(encoding+'.gpkg')
 with sqlite3.connect(template) as db:
  db.execute("PRAGMA encoding='"+encoding+"'");db.executescript(dump)
  db.executescript('CREATE TABLE sample(id INTEGER PRIMARY KEY,v TEXT COLLATE NOCASE,w REAL,b BLOB,extra INTEGER AS(id+1));CREATE TABLE empty(id INTEGER PRIMARY KEY);CREATE VIEW middle AS SELECT * FROM sample;CREATE VIEW limited AS SELECT * FROM sample ORDER BY id DESC LIMIT 3 OFFSET 1;CREATE VIEW compounded AS SELECT id,v FROM sample WHERE id<3 UNION ALL SELECT id,v FROM sample WHERE id>8;CREATE TABLE lookup(id INTEGER,label TEXT);CREATE TABLE no_rowid(id INTEGER PRIMARY KEY) WITHOUT ROWID;INSERT INTO no_rowid VALUES(1);CREATE TABLE rowid_shadow(rowid TEXT);INSERT INTO rowid_shadow VALUES(\'shadow\');CREATE TABLE disjoint(z INTEGER);INSERT INTO disjoint VALUES(100);CREATE VIEW lookup_view AS SELECT * FROM lookup ORDER BY label LIMIT 4;CREATE TABLE caseleft(k TEXT COLLATE NOCASE,a INT);CREATE TABLE caseright(k TEXT COLLATE RTRIM,b INT);CREATE TABLE realleft(id REAL);CREATE TABLE realright(id INTEGER);INSERT INTO realright VALUES(1);CREATE VIEW real_joined AS SELECT id FROM realleft FULL JOIN realright USING(id);')
  db.executemany('INSERT INTO sample(id,v,w,b) VALUES(?,?,?,?)',[(1,'a',2,b'\x00\xff'),(2,'A',0,b''),(3,'b ',None,None),(4,None,2,None),(5,'Ā',1.5,b'zz'),(6,'é','text',b'a'),(7,'水',b'\x01',None),(8,'😁',-1,b'00'),(9,'a\x00x',.5,None),(10,'a\x00y',1.5,None)])
  db.executescript("CREATE TABLE affinity_precedence(r BLOB,t BLOB,p BLOB);INSERT INTO affinity_precedence VALUES(1,1,1);PRAGMA writable_schema=ON;UPDATE sqlite_master SET sql='CREATE TABLE affinity_precedence(r REAL,t TEXTREAL,p FLOATING POINT)' WHERE name='affinity_precedence';PRAGMA writable_schema=OFF;")
  db.executemany('INSERT INTO lookup VALUES(?,?)',[(1,'one'),(2,'two'),(2,'duplicate'),(12,'unmatched'),(None,'null')])
  db.executemany('INSERT INTO caseleft VALUES(?,?)',[('A',1),('a ',2),('B',3),(None,4)])
  db.executemany('INSERT INTO caseright VALUES(?,?)',[('a',1),('A ',2),('b',3),(None,4)])
 for query in queries:
  path=root/(str(len(cases))+'.gpkg');shutil.copyfile(template,path)
  with sqlite3.connect(path) as db:
   try:db.execute('CREATE VIEW v AS '+query)
   except sqlite3.Error:
    db.execute('CREATE VIEW v AS SELECT 1');db.execute('PRAGMA writable_schema=ON')
    db.execute("UPDATE sqlite_master SET sql=? WHERE name='v'",('CREATE VIEW v AS '+query,))
    cases.append(dict(name=encoding+' '+query,path=str(path),columns=[],rows=[],error=True));continue
   try:
    columns=[[r[1],r[2]] for r in db.execute('PRAGMA table_info(v)')]
    rows=[[value(v) for v in row] for row in db.execute('SELECT * FROM v')];error=False
   except sqlite3.Error:columns=[];rows=[];error=True
  cases.append(dict(name=encoding+' '+query,path=str(path),columns=columns,rows=rows,error=error,ordered=' ORDER BY ' in query.upper() or ' LIMIT ' in query.upper()))
 path=root/(str(len(cases))+'.gpkg');shutil.copyfile(template,path)
 with sqlite3.connect(path) as db:
  db.execute('CREATE VIEW v("renamed id","renamed value") AS SELECT id,v FROM sample')
  cases.append(dict(name=encoding+' explicit columns',path=str(path),columns=[[r[1],r[2]] for r in db.execute('PRAGMA table_info(v)')],rows=[[value(v) for v in row] for row in db.execute('SELECT * FROM v')]))
print(json.dumps(cases))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).arg(directory.path()).arg(data).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        for case in &cases {
            let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
            let db = Snapshot::read(&bytes).unwrap();
            assert!(db.tables["sample"].rows.is_empty());
            let plan = View::compile(&db, "v");
            if case["error"].as_bool().unwrap_or(false) {
                let result = plan.and_then(|view| view.visit(&db, &bytes, &mut |_| Ok(())));
                assert!(result.is_err(), "{} incorrectly succeeded", case["name"]);
                continue;
            }
            let view = plan.unwrap_or_else(|e| panic!("{}: {e}", case["name"]));
            let columns: Vec<_> = view.schema.columns.iter().map(|c| serde_json::json!([c.name, c.data_type])).collect();
            assert_eq!(serde_json::json!(columns), case["columns"], "{}", case["name"]);
            let mut rows = Vec::new();
            view.visit(&db, &bytes, &mut |row| {
                rows.push(
                    row.iter()
                        .map(|v| match v {
                            Value::Null => serde_json::Value::Null,
                            Value::Int(v) => serde_json::json!(v),
                            Value::Real(v) => serde_json::json!(v),
                            Value::Text(v) => serde_json::json!(v),
                            Value::Blob(v) => serde_json::json!({"blob":v.iter().map(|b|format!("{b:02x}")).collect::<String>()}),
                        })
                        .collect::<Vec<_>>(),
                );
                Ok(())
            })
            .unwrap();
            let mut actual = rows.into_iter().map(|row| serde_json::json!(row)).collect::<Vec<_>>();
            let mut expected = case["rows"].as_array().unwrap().clone();
            if !case["ordered"].as_bool().unwrap_or(false) {
                actual.sort_by_key(|row| row.to_string());
                expected.sort_by_key(|row| row.to_string());
            }
            assert_eq!(actual, expected, "{}", case["name"]);
        }
        eprintln!("{} SQLite view schema/row comparisons passed", cases.len());
    }
}
