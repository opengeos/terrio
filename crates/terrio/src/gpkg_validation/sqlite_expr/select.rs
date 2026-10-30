//! Read-only, streaming view projections. Query forms not implemented here are
//! rejected rather than silently omitting a registered view from validation.
use super::*;
use order::{Order, Sorter, Window};
use parse::Parser;
use std::cell::Cell;
use std::collections::BTreeSet;

enum Source {
    Table(String),
    View(Box<View>),
    Unit,
}
enum Item {
    Star(Option<String>),
    Expression(Expression, String, bool),
}
pub(in super::super) struct View {
    pub schema: Schema,
    source_schema: Schema,
    source: Source,
    qualifier: Option<String>,
    projections: Vec<Expression>,
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
        rowid_alias: None,
        virtual_module: None,
        strict: false,
        checks: vec![],
    }
}
fn output_column(expr: &Expr, schema: &Schema, name: String) -> schema::Column {
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
        column.data_type.clone()
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
        Expr::Cast(inner, _) | Expr::Unary(_, inner) => expression_collation(inner, schema),
        _ => "BINARY".into(),
    }
}
impl View {
    pub fn compile(db: &Snapshot, name: &str) -> Result<Self> {
        Self::compile_inner(db, name, &mut Vec::new())
    }
    fn compile_inner(db: &Snapshot, name: &str, stack: &mut Vec<String>) -> Result<Self> {
        if stack.len() >= 128 || stack.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            return Err(bad("recursive view dependency"));
        }
        let object =
            db.objects.iter().find(|o| o.0 == "view" && o.1.eq_ignore_ascii_case(name)).ok_or_else(|| bad("view does not exist"))?;
        stack.push(object.1.clone());
        let sql = &object.4;
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
        let (source, source_schema, qualifier) = if p.eat("FROM") {
            let mut table = p.name()?;
            if p.eat(".") {
                if !table.eq_ignore_ascii_case("main") {
                    return Err(bad("view references another database"));
                }
                table = p.name()?;
            }
            let alias = if p.eat("AS")
                || matches!(p.tokens.get(p.at),Some(lex::Token::Name(n,q)) if *q != 0 || !["WHERE","GROUP","HAVING","ORDER","LIMIT","UNION","INTERSECT","EXCEPT","WINDOW","JOIN","LEFT","RIGHT","FULL","INNER","CROSS","NATURAL","ON","USING"].iter().any(|s|n.eq_ignore_ascii_case(s)))
            {
                p.name()?
            } else {
                table.clone()
            };
            if let Some((name, table)) = db.tables.iter().find(|(n, _)| n.eq_ignore_ascii_case(&table)) {
                if table.schema.virtual_module.is_some() {
                    return Err(Error::Unsupported("virtual-table view source".into()));
                }
                (Source::Table(name.clone()), table.schema.clone(), Some(alias))
            } else {
                let view = Self::compile_inner(db, &table, stack)?;
                let schema = view.schema.clone();
                (Source::View(Box::new(view)), schema, Some(alias))
            }
        } else {
            (Source::Unit, empty_schema(), None)
        };
        let predicate = if p.eat("WHERE") {
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
                    if qualifier.is_none() || prefix.as_deref().is_some_and(|s| !s.eq_ignore_ascii_case(binding)) {
                        return Err(bad("unknown star qualifier"));
                    }
                    for column in &source_schema.columns {
                        let expression = Expression(Expr::Column(vec![column.name.clone()], 1));
                        schema.columns.push(output_column(&expression.0, &source_schema, column.name.clone()));
                        projections.push(expression);
                    }
                }
                Item::Expression(expr, mut label, aliased) => {
                    expr.dependencies(binding, &source_schema, allow_rowid)?;
                    if !aliased && let Expr::Column(parts, _) = &expr.0 {
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
        if let Some(predicate) = &predicate {
            predicate.dependencies(binding, &source_schema, allow_rowid)?;
        }
        let order = raw_order
            .into_iter()
            .map(|term| term.bind(&source_schema, &schema, &projections, binding, allow_rowid))
            .collect::<Result<Vec<_>>>()?;
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
        if distinct && schema.columns.iter().any(|c| !matches!(c.collation.as_str(), "BINARY" | "NOCASE" | "RTRIM")) {
            return Err(Error::Unsupported("SQLite view DISTINCT collation".into()));
        }
        stack.pop();
        Ok(Self { schema, source_schema, source, qualifier, projections, predicate, distinct, order, window })
    }
    pub fn visit(&self, db: &Snapshot, bytes: &[u8], visitor: &mut dyn FnMut(&[Value]) -> Result<()>) -> Result<()> {
        self.visit_control(db, bytes, &mut |row| {
            visitor(row)?;
            Ok(true)
        })
    }
    fn visit_control(&self, db: &Snapshot, bytes: &[u8], visitor: &mut dyn FnMut(&[Value]) -> Result<bool>) -> Result<()> {
        let encoding = u32::from_be_bytes(bytes.get(56..60).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("truncated header"))?);
        let (count, offset) = self.window.evaluate(encoding)?;
        if count == Some(0) {
            return Ok(());
        }
        let mut seen = BTreeSet::new();
        let mut skipped = 0;
        let mut emitted = 0;
        let active = Cell::new(true);
        let mut sorter = Sorter::new(&self.order, count.map(|n| n.saturating_add(offset)), encoding);
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
                unicode_case: true,
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
            }
            if !self.order.is_empty() {
                sorter.push(keys, result);
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
            Source::View(view) => view.visit_control(db, bytes, &mut |row| project(row, None))?,
            Source::Table(name) => {
                sqlite_repack::inspect(bytes, |entry, rowid, record| {
                    if active.get() && entry.0 == "table" && entry.1 == *name {
                        project(&db.tables[name].decode(name, record, rowid, encoding)?, rowid)?;
                    }
                    Ok(())
                })?;
            }
        }
        if !self.order.is_empty() {
            for row in sorter.finish().into_iter().skip(offset).take(count.unwrap_or(usize::MAX)) {
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
for encoding in ['UTF-8','UTF-16le','UTF-16be']:
 template=root/(encoding+'.gpkg')
 with sqlite3.connect(template) as db:
  db.execute("PRAGMA encoding='"+encoding+"'");db.executescript(dump)
  db.executescript('CREATE TABLE sample(id INTEGER PRIMARY KEY,v TEXT COLLATE NOCASE,w REAL,b BLOB,extra INTEGER AS(id+1));CREATE TABLE empty(id INTEGER PRIMARY KEY);CREATE VIEW middle AS SELECT * FROM sample;CREATE VIEW limited AS SELECT * FROM sample ORDER BY id DESC LIMIT 3 OFFSET 1;')
  db.executemany('INSERT INTO sample(id,v,w,b) VALUES(?,?,?,?)',[(1,'a',2,b'\x00\xff'),(2,'A',0,b''),(3,'b ',None,None),(4,None,2,None),(5,'Ā',1.5,b'zz'),(6,'é','text',b'a'),(7,'水',b'\x01',None),(8,'😁',-1,b'00'),(9,'a\x00x',.5,None),(10,'a\x00y',1.5,None)])
 for query in queries:
  path=root/(str(len(cases))+'.gpkg');shutil.copyfile(template,path)
  with sqlite3.connect(path) as db:
   db.execute('CREATE VIEW v AS '+query)
   try:
    columns=[[r[1],r[2]] for r in db.execute('PRAGMA table_info(v)')]
    rows=[[value(v) for v in row] for row in db.execute('SELECT * FROM v')];error=False
   except sqlite3.Error:columns=[];rows=[];error=True
  cases.append(dict(name=encoding+' '+query,path=str(path),columns=columns,rows=rows,error=error))
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
            assert_eq!(serde_json::json!(rows), case["rows"], "{}", case["name"]);
        }
        eprintln!("{} SQLite view schema/row comparisons passed", cases.len());
    }
}
