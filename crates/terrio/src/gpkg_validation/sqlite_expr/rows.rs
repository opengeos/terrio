use super::*;

/// Expressions are parsed once per table. Virtual generated columns are evaluated
/// in dependency order; stored generated columns retain their on-disk values as
/// SQLite does while reading them and checking constraints/indexes.
pub(in super::super) struct RowEvaluator {
    pub checks: Vec<Expression>,
    virtuals: Vec<(usize, Expression)>,
}
impl Expression {
    pub(in super::super) fn dependencies(&self, name: &str, schema: &Schema, allow_rowid: bool) -> Result<Vec<usize>> {
        let mut deps = Vec::new();
        let mut stack = vec![&self.0];
        while let Some(expr) = stack.pop() {
            if let Expr::Column(parts, quoted) = expr {
                let field = parts.last().unwrap();
                if parts.len() > 1 && !parts[parts.len() - 2].eq_ignore_ascii_case(name) {
                    return Err(bad("unknown table qualifier"));
                }
                if let Some(i) = schema.columns.iter().position(|c| c.name.eq_ignore_ascii_case(field)) {
                    deps.push(i);
                } else if !(*quoted == 2 && parts.len() == 1
                    || *quoted == 0 && parts.len() == 1 && ["true", "false"].iter().any(|s| field.eq_ignore_ascii_case(s))
                    || allow_rowid && ["rowid", "_rowid_", "oid"].iter().any(|s| field.eq_ignore_ascii_case(s)))
                {
                    return Err(bad(&format!("no column named {field}")));
                }
            }
            stack.extend(expr.children());
        }
        Ok(deps)
    }
}
impl RowEvaluator {
    pub fn new(name: &str, schema: &Schema) -> Result<Self> {
        let mut checks = Vec::new();
        for sql in &schema.checks {
            let expression = Expression::parse(sql)?;
            expression.dependencies(name, schema, !schema.without_rowid)?;
            checks.push(expression);
        }
        let mut generated = Vec::new();
        for (i, column) in schema.columns.iter().enumerate() {
            if let Some(g) = &column.generated {
                let expression = Expression::parse(&g.expression)?;
                let deps = expression.dependencies(name, schema, false)?;
                if !g.stored {
                    generated.push((i, expression, deps));
                }
            }
        }
        let mut virtuals = Vec::new();
        while !generated.is_empty() {
            let Some(at) = generated.iter().position(|(_, _, deps)| deps.iter().all(|dep| !generated.iter().any(|(i, _, _)| i == dep)))
            else {
                return Err(bad("generated column dependency cycle"));
            };
            let (i, expr, _) = generated.remove(at);
            virtuals.push((i, expr));
        }
        Ok(Self { checks, virtuals })
    }
    pub fn row(&self, name: &str, schema: &Schema, mut row: Vec<Value>, rowid: Option<i64>, encoding: u32) -> Result<Vec<Value>> {
        for (i, expr) in &self.virtuals {
            let value = expr.evaluate(&Context {
                columns: &schema.columns,
                row: &row,
                rowid,
                table: Some(name),
                strict: schema.strict,
                unicode_case: true,
                encoding,
            })?;
            row[*i] =
                if schema.strict && schema.columns[*i].data_type == "ANY" { value } else { column_value(value, &schema.columns[*i])? };
        }
        Ok(row)
    }
}
