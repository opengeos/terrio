//! SQLite ORDER BY binding, LIMIT conversion and a bounded top-k sorter.
use super::*;
use parse::Parser;
use std::collections::BinaryHeap;

pub(super) struct Order {
    expr: Expression,
    descending: bool,
    nulls_last: bool,
    collation: String,
}
fn ordinal(expr: &Expr) -> Option<i32> {
    match expr {
        Expr::Literal(Value::Int(n)) => (*n).try_into().ok(),
        Expr::Unary(op, inner) if op == "+" => ordinal(inner),
        Expr::Unary(op, inner) if op == "-" => ordinal(inner)?.checked_neg(),
        Expr::Collate(inner, _) => ordinal(inner),
        _ => None,
    }
}
fn alias<'a>(expr: &Expr, schema: &Schema, projections: &'a [Expression]) -> Option<&'a Expression> {
    match expr {
        Expr::Column(parts, _) if parts.len() == 1 => {
            schema.columns.iter().position(|c| c.name.eq_ignore_ascii_case(&parts[0])).map(|i| &projections[i])
        }
        Expr::Collate(inner, _) => alias(inner, schema, projections),
        _ => None,
    }
}
pub(super) fn expand(expr: &mut Expr, source: &Schema, output: &Schema, projections: &[Expression]) {
    // Within an expression, source columns take precedence over SELECT aliases.
    if let Expr::Column(parts, _) = expr
        && parts.len() == 1
        && !source.columns.iter().any(|c| c.name.eq_ignore_ascii_case(&parts[0]))
        && let Some(i) = output.columns.iter().position(|c| c.name.eq_ignore_ascii_case(&parts[0]))
    {
        *expr = projections[i].0.clone();
        return;
    }
    for child in expr.children_mut() {
        expand(child, source, output, projections);
    }
}
impl Order {
    pub fn parse(p: &mut Parser) -> Result<Vec<Self>> {
        let mut terms = Vec::new();
        if p.eat("ORDER") {
            p.expect("BY")?;
            loop {
                let expr = Expression(p.expression(0)?);
                Parser::check_depth(&expr.0)?;
                let descending = p.eat("DESC");
                if !descending {
                    p.eat("ASC");
                }
                let nulls_last = if p.eat("NULLS") {
                    if p.eat("FIRST") {
                        false
                    } else {
                        p.expect("LAST")?;
                        true
                    }
                } else {
                    descending
                };
                terms.push(Self { expr, descending, nulls_last, collation: "BINARY".into() });
                if !p.eat(",") {
                    break;
                }
            }
        }
        Ok(terms)
    }
    pub fn bind_compound(mut self, compound: &compound::Compound, output: &Schema) -> Result<Self> {
        fn normalize(expr: &mut Expr, schema: &Schema, binding: &str) {
            if let Expr::Column(parts, quoted) = expr {
                if parts.len() > 1 && !parts[parts.len() - 2].eq_ignore_ascii_case(binding) {
                    return;
                }
                if let Some(i) = schema.columns.iter().position(|c| c.name.eq_ignore_ascii_case(parts.last().unwrap())) {
                    *expr = Expr::Column(vec![i.to_string()], 1);
                    return;
                }
                if *quoted == 2 && parts.len() == 1 {
                    *expr = Expr::Literal(Value::Text(parts[0].clone()));
                    return;
                }
            }
            for child in expr.children_mut() {
                normalize(child, schema, binding);
            }
        }
        fn stripped(expr: &Expr) -> &Expr {
            if let Expr::Collate(inner, _) = expr { stripped(inner) } else { expr }
        }
        let index = if let Some(n) = ordinal(&self.expr.0) {
            usize::try_from(n).ok().and_then(|n| n.checked_sub(1)).filter(|i| *i < output.columns.len())
        } else {
            compound.arms.iter().find_map(|arm| {
                if let Expr::Column(parts, _) = stripped(&self.expr.0)
                    && parts.len() == 1
                    && let Some(i) = arm.schema.columns.iter().position(|c| c.name.eq_ignore_ascii_case(&parts[0]))
                {
                    return Some(i);
                }
                let mut needle = stripped(&self.expr.0).clone();
                if let select::Source::Join(join) = &arm.source
                    && join.bind(&mut needle).is_err()
                {
                    return None;
                }
                normalize(&mut needle, &arm.source_schema, arm.qualifier.as_deref().unwrap_or(""));
                arm.projections.iter().position(|p| {
                    let mut p = stripped(&p.0).clone();
                    normalize(&mut p, &arm.source_schema, arm.qualifier.as_deref().unwrap_or(""));
                    p == needle
                })
            })
        }
        .ok_or_else(|| bad("compound ORDER BY term does not match a result column"))?;
        self.collation = self.expr.0.explicit_collation().map_or_else(|| compound.column_collation(index).into(), str::to_owned);
        self.expr = Expression(Expr::Column(vec![output.columns[index].name.clone()], 1));
        Ok(self)
    }
    pub fn bind(
        mut self,
        source: &Schema,
        output: &Schema,
        projections: &[Expression],
        binding: &str,
        rowid: bool,
        join: Option<&joins::Join>,
    ) -> Result<Self> {
        let explicit = self.expr.0.explicit_collation().map(str::to_owned);
        if let Some(n) = ordinal(&self.expr.0) {
            let expr = usize::try_from(n)
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|i| projections.get(i))
                .ok_or_else(|| bad("ORDER BY position out of range"))?;
            self.expr = expr.clone();
        } else if let Some(expr) = alias(&self.expr.0, output, projections) {
            self.expr = expr.clone();
        } else {
            if let Some(join) = join {
                join.bind(&mut self.expr.0)?;
            }
            expand(&mut self.expr.0, source, output, projections);
        }
        Parser::check_depth(&self.expr.0)?;
        self.expr.dependencies(binding, source, rowid)?;
        self.collation = explicit.unwrap_or_else(|| select::expression_collation(&self.expr.0, source));
        if !matches!(self.collation.as_str(), "BINARY" | "NOCASE" | "RTRIM") {
            return Err(Error::Unsupported("SQLite ORDER BY collation".into()));
        }
        Ok(self)
    }
}
pub(super) fn storage_atom(value: &Value, collation: &str, encoding: u32) -> integrity::Atom {
    if let Value::Text(s) = value
        && collation == "BINARY"
        && matches!(encoding, 2 | 3)
    {
        return integrity::Atom::Text(
            s.encode_utf16().flat_map(|u| if encoding == 2 { u.to_le_bytes() } else { u.to_be_bytes() }).collect(),
            0,
        );
    }
    let metadata = schema::Column {
        name: String::new(),
        data_type: "BLOB".into(),
        not_null: false,
        primary_key: 0,
        default: None,
        collation: collation.into(),
        not_null_declared: false,
        generated: None,
    };
    integrity::atom(value, &metadata, false).unwrap_or(integrity::Atom::Null)
}
#[derive(Default)]
pub(super) struct Window {
    count: Option<Expression>,
    offset: Option<Expression>,
}
impl Window {
    pub fn parse(p: &mut Parser) -> Result<Self> {
        let mut plan = Self::default();
        if p.eat("LIMIT") {
            plan.count = Some(Expression(p.expression(0)?));
            if p.eat("OFFSET") {
                plan.offset = Some(Expression(p.expression(0)?));
            } else if p.eat(",") {
                plan.offset = plan.count.take();
                plan.count = Some(Expression(p.expression(0)?));
            }
            for expr in plan.count.iter().chain(&plan.offset) {
                Parser::check_depth(&expr.0)?;
                expr.dependencies("", &select::empty_schema(), false)?;
            }
        }
        Ok(plan)
    }
    pub fn evaluate(&self, encoding: u32) -> Result<(Option<usize>, usize)> {
        fn integer(expr: &Expression, encoding: u32) -> Result<i64> {
            let v = expr.evaluate(&Context {
                columns: &[],
                row: &[],
                rowid: None,
                table: None,
                encoding,
                strict: false,
                logical: false,
                unicode_case: true,
            })?;
            let v = if let Value::Text(s) = v { integrity::numeric(&s).unwrap_or(Value::Null) } else { v };
            match v {
                Value::Int(n) => Ok(n),
                Value::Real(n) => metadata::integer(&Value::Real(n)).ok_or_else(|| bad("LIMIT/OFFSET datatype mismatch")),
                _ => Err(bad("LIMIT/OFFSET datatype mismatch")),
            }
        }
        let count = self
            .count
            .as_ref()
            .map(|e| integer(e, encoding))
            .transpose()?
            .filter(|n| *n >= 0)
            .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
        if count == Some(0) {
            return Ok((count, 0));
        }
        let offset = self.offset.as_ref().map(|e| integer(e, encoding)).transpose()?.unwrap_or(0).max(0);
        Ok((count, usize::try_from(offset).unwrap_or(usize::MAX)))
    }
}
#[derive(PartialEq, Eq)]
pub(super) struct Key {
    atom: integrity::Atom,
    descending: bool,
    nulls_last: bool,
}
impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        let a = matches!(self.atom, integrity::Atom::Null);
        let b = matches!(other.atom, integrity::Atom::Null);
        if a || b {
            return if self.nulls_last { a.cmp(&b) } else { b.cmp(&a) };
        }
        let order = constraints::compare_atom(&self.atom, &other.atom);
        if self.descending { order.reverse() } else { order }
    }
}
impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
struct Record {
    keys: Vec<Key>,
    row: Vec<Value>,
    ordinal: usize,
}
impl PartialEq for Record {
    fn eq(&self, other: &Self) -> bool {
        self.keys == other.keys && self.ordinal == other.ordinal
    }
}
impl Eq for Record {}
impl Ord for Record {
    fn cmp(&self, other: &Self) -> Ordering {
        self.keys.cmp(&other.keys).then(self.ordinal.cmp(&other.ordinal))
    }
}
impl PartialOrd for Record {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
pub(super) struct Sorter<'a> {
    terms: &'a [Order],
    limit: Option<usize>,
    encoding: u32,
    heap: BinaryHeap<Record>,
    sequence: usize,
}
impl<'a> Sorter<'a> {
    pub fn new(terms: &'a [Order], limit: Option<usize>, encoding: u32) -> Self {
        Self { terms, limit, encoding, heap: BinaryHeap::new(), sequence: 0 }
    }
    pub fn keys(&self, context: &Context<'_>) -> Result<Vec<Key>> {
        self.terms
            .iter()
            .map(|term| {
                let value = term.expr.evaluate(context)?;
                let atom = storage_atom(&value, &term.collation, self.encoding);
                Ok(Key { atom, descending: term.descending, nulls_last: term.nulls_last })
            })
            .collect()
    }
    pub fn admits(&self, keys: &[Key]) -> bool {
        self.limit.is_none_or(|n| self.heap.len() < n || self.heap.peek().is_some_and(|old| keys < old.keys.as_slice()))
    }
    pub fn push(&mut self, keys: Vec<Key>, row: Vec<Value>) {
        if !self.admits(&keys) {
            return;
        }
        self.heap.push(Record { keys, row, ordinal: self.sequence });
        self.sequence += 1;
        if self.limit.is_some_and(|n| self.heap.len() > n) {
            self.heap.pop();
        }
    }
    pub fn finish(self) -> Vec<Vec<Value>> {
        self.heap.into_sorted_vec().into_iter().map(|r| r.row).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sorted_limits_retain_only_the_best_needed_rows() {
        let schema = Schema::parse("CREATE TABLE sample(v INTEGER)").unwrap();
        let terms = [Order { expr: Expression::parse("v").unwrap(), descending: false, nulls_last: false, collation: "BINARY".into() }];
        let mut sorter = Sorter::new(&terms, Some(5), 1);
        for n in (0..1000).rev() {
            let row = [Value::Int(n)];
            let context = Context {
                columns: &schema.columns,
                row: &row,
                rowid: None,
                table: Some("sample"),
                encoding: 1,
                strict: false,
                logical: false,
                unicode_case: true,
            };
            let keys = sorter.keys(&context).unwrap();
            sorter.push(keys, row.to_vec());
            assert!(sorter.heap.len() <= 5);
        }
        assert_eq!(sorter.finish(), (0..5).map(|n| vec![Value::Int(n)]).collect::<Vec<_>>());
    }
}
