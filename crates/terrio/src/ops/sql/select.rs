//! SELECT planning and execution. Input features are borrowed until a result row is emitted.
use super::*;
use wbvector::{FieldDef, FieldType};

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Item {
    Star(Option<String>),
    Column(Expr, Option<String>),
    Aggregate(String, Option<String>, bool, Option<String>),
}

impl Item {
    pub(super) fn parse(p: &mut Parser) -> Result<Self> {
        if p.eat(&Token::Op("*")) {
            return Ok(Self::Star(None));
        }
        if let Some(Token::Ident(s)) = p.peek() {
            if let Some(prefix) = s.strip_suffix('.') {
                let prefix = prefix.to_owned();
                if p.tokens.get(p.pos + 1) == Some(&Token::Op("*")) {
                    p.pos += 2;
                    return Ok(Self::Star(Some(prefix)));
                }
            }
            if ["COUNT", "SUM", "AVG", "MIN", "MAX"].iter().any(|f| s.eq_ignore_ascii_case(f))
                && p.tokens.get(p.pos + 1) == Some(&Token::LParen)
            {
                let op = p.identifier()?.to_ascii_uppercase();
                p.pos += 1;
                let distinct = p.eat_keyword("DISTINCT");
                let field = if p.eat(&Token::Op("*")) { None } else { Some(p.identifier()?) };
                if !p.eat(&Token::RParen) {
                    return Err(syntax("summary arguments must be fields".into()));
                }
                if (field.is_none() && (op != "COUNT" || distinct)) || (distinct && op != "COUNT") {
                    return Err(syntax("only COUNT supports * or DISTINCT".into()));
                }
                let alias = if p.eat_keyword("AS") { Some(p.identifier()?) } else { None };
                return Ok(Self::Aggregate(op, field, distinct, alias));
            }
        }
        let expr = p.expr()?;
        let alias = if p.eat_keyword("AS") { Some(p.identifier()?) } else { None };
        Ok(Self::Column(expr, alias))
    }
}

pub(super) fn table_alias(p: &mut Parser) -> Result<Option<String>> {
    if p.eat_keyword("AS") {
        return Ok(Some(p.identifier()?));
    }
    if matches!(p.peek(), Some(Token::Quoted(_)))
        || matches!(p.peek(), Some(Token::Ident(n)) if !RESERVED.iter().any(|k| n.eq_ignore_ascii_case(k)))
    {
        return Ok(Some(p.identifier()?));
    }
    Ok(None)
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Join {
    pub layer: String,
    pub alias: Option<String>,
    pub left: String,
    pub right: String,
}

fn primary_name<'a>(name: &'a str, s: &Select) -> &'a str {
    if let Some((prefix, field)) = name.split_once('.')
        && prefix.eq_ignore_ascii_case(s.alias.as_deref().unwrap_or(&s.from))
    {
        return field;
    }
    name
}

fn normalize(e: Expr, s: &Select) -> Expr {
    match e {
        Expr::Field(n) => Expr::Field(primary_name(&n, s).to_owned()),
        Expr::Not(a) => Expr::Not(Box::new(normalize(*a, s))),
        Expr::Neg(a) => Expr::Neg(Box::new(normalize(*a, s))),
        Expr::Binary(op, a, b) => Expr::Binary(op, Box::new(normalize(*a, s)), Box::new(normalize(*b, s))),
        Expr::Like { expr, pattern, escape, negated, insensitive } => {
            Expr::Like { expr: Box::new(normalize(*expr, s)), pattern: Box::new(normalize(*pattern, s)), escape, negated, insensitive }
        }
        Expr::In { expr, list, negated } => {
            Expr::In { expr: Box::new(normalize(*expr, s)), list: list.into_iter().map(|e| normalize(e, s)).collect(), negated }
        }
        Expr::Between { expr, low, high, negated } => Expr::Between {
            expr: Box::new(normalize(*expr, s)),
            low: Box::new(normalize(*low, s)),
            high: Box::new(normalize(*high, s)),
            negated,
        },
        Expr::IsNull { expr, negated } => Expr::IsNull { expr: Box::new(normalize(*expr, s)), negated },
        e => e,
    }
}
fn bound(e: &Expr, schema: &Schema, s: &Select) -> Result<Expr> {
    bind(&normalize(e.clone(), s), schema)
}

fn expression_type(e: &Expr, schema: &Schema) -> Result<FieldType> {
    Ok(match e {
        Expr::Index(i) => schema.fields()[*i].field_type,
        Expr::Fid | Expr::Literal(Value::Int(_)) => FieldType::Integer,
        Expr::Literal(Value::Text(_)) | Expr::Binary(BinOp::Concat, _, _) => FieldType::Text,
        Expr::Literal(Value::Float(_) | Value::Null) => FieldType::Float,
        Expr::Literal(Value::Blob(_)) => FieldType::Blob,
        Expr::Neg(e) => {
            let ty = expression_type(e, schema)?;
            if !matches!(ty, FieldType::Integer | FieldType::Float | FieldType::Boolean) {
                return Err(syntax("arithmetic requires numeric fields".into()));
            }
            ty
        }
        Expr::Binary(BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod, a, b) => {
            let (a, b) = (expression_type(a, schema)?, expression_type(b, schema)?);
            if !matches!(a, FieldType::Integer | FieldType::Float | FieldType::Boolean)
                || !matches!(b, FieldType::Integer | FieldType::Float | FieldType::Boolean)
            {
                return Err(syntax("arithmetic requires numeric fields".into()));
            }
            if a == FieldType::Float || b == FieldType::Float { FieldType::Float } else { FieldType::Integer }
        }
        _ => FieldType::Integer,
    })
}
fn field_value(v: Value, ty: FieldType) -> FieldValue {
    match v {
        Value::Null => FieldValue::Null,
        Value::Blob(b) => FieldValue::Blob(b),
        Value::Int(i) if ty == FieldType::Float => FieldValue::Float(i as f64),
        Value::Int(i) if ty == FieldType::Boolean => FieldValue::Boolean(i != 0),
        Value::Int(i) => FieldValue::Integer(i),
        Value::Float(f) => FieldValue::Float(f),
        Value::Text(s) if ty == FieldType::Date => FieldValue::Date(s),
        Value::Text(s) if ty == FieldType::DateTime => FieldValue::DateTime(s),
        Value::Text(s) => FieldValue::Text(s),
    }
}
fn key(v: &Value) -> String {
    // DISTINCT uses exact, case-sensitive values in current OGRSQL (GDAL 3.12).
    match v {
        Value::Null => "n".into(),
        Value::Blob(b) => format!("b{b:?}"),
        Value::Text(s) => format!("s{s}"),
        Value::Int(i) => format!("i{i}"),
        Value::Float(f) => format!("f{:016x}", if *f == 0.0 { 0 } else { f.to_bits() }),
    }
}

pub(crate) fn execute(layers: &[Layer], s: &Select, copy_limit: Option<usize>) -> Result<(Layer, Option<usize>)> {
    let layer = layers
        .iter()
        .find(|l| l.name.eq_ignore_ascii_case(&s.from))
        .or_else(|| (layers.len() == 1).then(|| &layers[0]))
        .ok_or_else(|| syntax(format!("no layer named '{}'", s.from)))?;
    let filter = s.filter.as_ref().map(|e| bound(&e.0, &layer.schema, s)).transpose()?;
    let mut schema = layer.schema.clone();
    let mut joins = Vec::new();
    let mut qualifiers = vec![s.alias.as_deref().unwrap_or(&s.from).to_ascii_lowercase()];
    for join in &s.joins {
        let rhs = layers
            .iter()
            .find(|l| l.name.eq_ignore_ascii_case(&join.layer))
            .ok_or_else(|| syntax(format!("no layer named '{}'", join.layer)))?;
        let qualifier = join.alias.as_deref().unwrap_or(&join.layer);
        if qualifiers.contains(&qualifier.to_ascii_lowercase()) {
            return Err(syntax("table aliases must be unique".into()));
        }
        qualifiers.push(qualifier.to_ascii_lowercase());
        let li = field_index(&layer.schema, primary_name(&join.left, s))
            .ok_or_else(|| syntax("JOIN left key must belong to the primary layer".into()))?;
        let Some((prefix, field)) = join.right.split_once('.') else {
            return Err(syntax("JOIN right key must be qualified by its table".into()));
        };
        if !prefix.eq_ignore_ascii_case(qualifier) {
            return Err(syntax("JOIN right key must belong to its joined layer".into()));
        }
        let ri = field_index(&rhs.schema, field).ok_or_else(|| syntax(format!("no field named '{}'", join.right)))?;
        for f in rhs.schema.fields() {
            let mut def = f.clone();
            def.name = format!("{qualifier}.{}", f.name);
            schema.add_field(def);
        }
        joins.push((rhs, li, ri));
    }
    let summary = s.items.iter().any(|i| matches!(i, Item::Aggregate(..)));
    if summary && s.items.iter().any(|i| !matches!(i, Item::Aggregate(..))) {
        return Err(syntax("all summary columns must be aggregate functions".into()));
    }
    if s.distinct && (s.items.len() != 1 || !matches!(&s.items[0], Item::Column(Expr::Field(_), _))) {
        return Err(syntax("DISTINCT requires one attribute field".into()));
    }
    if summary && (s.distinct || !joins.is_empty() || s.order_by.is_some()) {
        return Err(syntax("summary queries do not support joins, DISTINCT or ORDER BY".into()));
    }
    let mut out = Layer::new(layer.name.clone());
    if !summary && !s.distinct {
        out.geom_type = layer.geom_type;
        out.crs = layer.crs.clone();
    }
    let mut expressions = Vec::new();
    let mut aggregates = Vec::new();
    for item in &s.items {
        match item {
            Item::Star(prefix) => {
                let primary = prefix.as_ref().is_none_or(|p| p.eq_ignore_ascii_case(s.alias.as_deref().unwrap_or(&s.from)));
                let mut found = false;
                for (i, f) in schema.fields().iter().enumerate() {
                    if prefix.is_none()
                        || (i < layer.schema.len() && primary)
                        || f.name.split_once('.').is_some_and(|(p, _)| prefix.as_ref().is_some_and(|q| p.eq_ignore_ascii_case(q)))
                    {
                        let mut def = f.clone();
                        if let Some(p) = prefix
                            && i < layer.schema.len()
                        {
                            def.name = format!("{p}.{}", f.name);
                        }
                        if prefix.is_none() && i >= layer.schema.len() {
                            let short = f.name.split_once('.').unwrap().1;
                            if field_index(&out.schema, short).is_none() {
                                def.name = short.into();
                            }
                        }
                        out.schema.add_field(def);
                        expressions.push(Expr::Index(i));
                        found = true;
                    }
                }
                if !found && prefix.as_ref().is_some_and(|p| !qualifiers.contains(&p.to_ascii_lowercase())) {
                    return Err(syntax("unknown table wildcard".into()));
                }
            }
            Item::Column(e, alias) => {
                let e_bound = bound(e, &schema, s)?;
                let name = alias
                    .clone()
                    .unwrap_or_else(|| if let Expr::Field(n) = e { n.clone() } else { format!("FIELD_{}", expressions.len() + 1) });
                let def = if let Expr::Index(i) = e_bound {
                    let mut d = schema.fields()[i].clone();
                    d.name = name;
                    d
                } else {
                    FieldDef::new(name, expression_type(&e_bound, &schema)?)
                };
                out.schema.add_field(def);
                expressions.push(e_bound);
            }
            Item::Aggregate(op, field, distinct, alias) => {
                let e = field.as_ref().map(|n| bound(&Expr::Field(n.clone()), &schema, s)).transpose()?;
                let ty = e.as_ref().map(|e| expression_type(e, &schema)).transpose()?.unwrap_or(FieldType::Integer);
                if matches!(op.as_str(), "SUM" | "AVG") && !matches!(ty, FieldType::Integer | FieldType::Float) {
                    return Err(syntax("SUM and AVG require numeric fields".into()));
                }
                let out_ty = if op == "AVG" {
                    FieldType::Float
                } else if op == "COUNT" {
                    FieldType::Integer
                } else {
                    ty
                };
                out.schema
                    .add_field(FieldDef::new(alias.clone().unwrap_or_else(|| format!("{op}_{}", field.as_deref().unwrap_or("*"))), out_ty));
                aggregates.push(Aggregate {
                    op: op.clone(),
                    expr: e,
                    distinct: *distinct,
                    seen: Default::default(),
                    count: 0,
                    value: Value::Null,
                });
            }
        }
    }
    let order = s
        .order_by
        .as_ref()
        .map(|(name, desc)| {
            let e = bound(&Expr::Field(name.clone()), &layer.schema, s)?;
            if s.distinct
                && !matches!(&s.items[0],Item::Column(Expr::Field(n),_) if primary_name(n,s).eq_ignore_ascii_case(primary_name(name,s)))
            {
                return Err(syntax("DISTINCT ORDER BY must use the selected field".into()));
            }
            Ok((e, *desc))
        })
        .transpose()?;
    let mut sorted;
    let features: Box<dyn Iterator<Item = &Feature>> = if let Some((e, desc)) = &order {
        sorted = layer.features.iter().filter(|f| filter.as_ref().is_none_or(|e| truth(e, f).value)).collect::<Vec<_>>();
        sorted.sort_by(|a, b| {
            let a = eval(e, a);
            let b = eval(e, b);
            let o = match (&a, &b) {
                (Value::Null, Value::Null) => Ordering::Equal,
                (Value::Null, _) => Ordering::Less,
                (_, Value::Null) => Ordering::Greater,
                _ => compare(&a, &b).unwrap_or(Ordering::Equal),
            };
            if *desc { o.reverse() } else { o }
        });
        Box::new(sorted.into_iter())
    } else {
        Box::new(layer.features.iter().filter(|f| filter.as_ref().is_none_or(|e| truth(e, f).value)))
    };
    let mut seen = std::collections::HashSet::new();
    for f in features {
        if summary {
            for agg in &mut aggregates {
                agg.push(f);
            }
            continue;
        }
        if copy_limit.is_some_and(|n| out.features.len() >= n) {
            break;
        }
        let mut joined;
        let row = if joins.is_empty() {
            f
        } else {
            joined = Feature { fid: f.fid, geometry: None, attributes: f.attributes.clone() };
            joined.attributes.resize(layer.schema.len(), FieldValue::Null);
            for (rhs, li, ri) in &joins {
                let v = f.attributes.get(*li).map(Value::from_field).unwrap_or(Value::Null);
                let matched = rhs.features.iter().find(|r| {
                    let other = r.attributes.get(*ri).map(Value::from_field).unwrap_or(Value::Null);
                    match (&v, &other) {
                        (Value::Text(a), Value::Text(b)) => a.eq_ignore_ascii_case(b),
                        _ => compare(&v, &other) == Some(Ordering::Equal),
                    }
                });
                for i in 0..rhs.schema.len() {
                    joined.attributes.push(matched.and_then(|r| r.attributes.get(i)).cloned().unwrap_or(FieldValue::Null));
                }
            }
            &joined
        };
        let values: Vec<_> = expressions.iter().map(|e| eval(e, row)).collect();
        if s.distinct && !seen.insert(key(&values[0])) {
            continue;
        }
        let attributes = expressions
            .iter()
            .zip(values)
            .enumerate()
            .map(|(i, (e, v))| {
                if let Expr::Index(j) = e {
                    row.attributes.get(*j).cloned().unwrap_or(FieldValue::Null)
                } else {
                    field_value(v, out.schema.fields()[i].field_type)
                }
            })
            .collect();
        out.features.push(Feature {
            fid: if s.distinct { out.features.len() as u64 } else { f.fid },
            geometry: if s.distinct { None } else { f.geometry.clone() },
            attributes,
        });
    }
    if summary {
        let attributes = aggregates.into_iter().zip(out.schema.fields()).map(|(a, f)| field_value(a.finish(), f.field_type)).collect();
        if copy_limit != Some(0) {
            out.features.push(Feature { fid: 0, geometry: None, attributes });
        }
    }
    Ok((out, s.limit))
}

struct Aggregate {
    op: String,
    expr: Option<Expr>,
    distinct: bool,
    seen: std::collections::HashSet<String>,
    count: u64,
    value: Value,
}
impl Aggregate {
    fn push(&mut self, f: &Feature) {
        let mut v = self.expr.as_ref().map(|e| eval(e, f)).unwrap_or(Value::Int(1));
        if self.op == "AVG"
            && let Some(x) = v.as_f64()
        {
            v = Value::Float(x);
        }
        if v == Value::Null || (self.distinct && !self.seen.insert(key(&v))) {
            return;
        }
        self.count += 1;
        self.value = match self.op.as_str() {
            "COUNT" => Value::Null,
            "SUM" | "AVG" => {
                if self.value == Value::Null {
                    v
                } else {
                    arith(BinOp::Add, self.value.clone(), v)
                }
            }
            "MIN" => {
                if self.value == Value::Null || compare(&v, &self.value) == Some(Ordering::Less) {
                    v
                } else {
                    self.value.clone()
                }
            }
            "MAX" => {
                if self.value == Value::Null || compare(&v, &self.value) == Some(Ordering::Greater) {
                    v
                } else {
                    self.value.clone()
                }
            }
            _ => unreachable!(),
        };
    }
    fn finish(self) -> Value {
        match self.op.as_str() {
            "COUNT" => Value::Int(self.count as i64),
            "AVG" if self.count > 0 => Value::Float(self.value.as_f64().unwrap() / self.count as f64),
            _ => self.value,
        }
    }
}
