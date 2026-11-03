//! Attribute GROUP BY with SQLite-style keys, checked against GDAL 3.13.3.
use super::select::{Aggregate, Item, add_output_field, bound, field_value, key};
use super::*;
use std::collections::HashMap;

pub(super) fn execute(layer: &Layer, s: &Select, limit: Option<usize>) -> Result<(Layer, Option<usize>)> {
    if s.distinct || !s.joins.is_empty() {
        return Err(syntax("GROUP BY does not support DISTINCT or joins".into()));
    }
    let keys = s.group_by.iter().map(|n| bound(&Expr::Field(n.clone()), &layer.schema, s)).collect::<Result<Vec<_>>>()?;
    if keys.iter().any(|e| !matches!(e, Expr::Index(_) | Expr::Fid)) {
        return Err(syntax("GROUP BY requires attribute fields".into()));
    }
    let filter = s.filter.as_ref().map(|e| scalar(bound(&e.0, &layer.schema, s)?)).transpose()?;
    let mut out = Layer::new(layer.name.clone());
    let mut plans = Vec::new();
    // Each output is a group key or an independent aggregate accumulator.
    enum Column {
        Key(usize),
        Summary(usize),
    }
    let mut columns = Vec::new();
    for item in &s.items {
        match item {
            Item::Column(e @ Expr::Field(name), alias) => {
                let e = bound(e, &layer.schema, s)?;
                let i = keys.iter().position(|k| *k == e).ok_or_else(|| syntax("non-summary columns must be GROUP BY fields".into()))?;
                let mut def = match e {
                    Expr::Index(j) => layer.schema.fields()[j].clone(),
                    _ => wbvector::FieldDef::new(name.clone(), wbvector::FieldType::Integer),
                };
                def.name = alias.clone().unwrap_or_else(|| name.clone());
                add_output_field(&mut out.schema, def, true)?;
                columns.push(Column::Key(i));
            }
            Item::Aggregate(op, field, distinct, alias) => {
                let (def, plan) = Aggregate::plan(op, field, *distinct, alias, &layer.schema, s)?;
                add_output_field(&mut out.schema, def, true)?;
                columns.push(Column::Summary(plans.len()));
                plans.push(plan);
            }
            _ => return Err(syntax("GROUP BY selects only grouping fields and summaries".into())),
        }
    }
    let order = s
        .order_by
        .as_ref()
        .map(|(n, desc)| {
            // Qualified names denote source fields; unqualified names prefer aliases.
            let alias = if n.contains('.') { None } else { field_index(&out.schema, n) };
            let selected = if alias.is_some() {
                alias
            } else {
                let source = bound(&Expr::Field(n.clone()), &layer.schema, s)?;
                columns.iter().position(|column| matches!(column, Column::Key(i) if keys[*i] == source))
            };
            selected.map(|i| (i, *desc)).ok_or_else(|| syntax("grouped ORDER BY must name a selected column or alias".into()))
        })
        .transpose()?;
    let mut index = HashMap::<Vec<String>, usize>::new();
    let mut groups: Vec<(Vec<Value>, Vec<Aggregate>)> = Vec::new();
    for f in &layer.features {
        if filter.as_ref().is_some_and(|e| !truth(e, f).value) {
            continue;
        }
        let values: Vec<_> = keys
            .iter()
            .map(|e| match eval(e, f) {
                Value::Float(x) if x.is_nan() => Value::Null,
                v => v,
            })
            .collect();
        let k = values.iter().map(group_key).collect();
        let i = *index.entry(k).or_insert_with(|| {
            groups.push((values, plans.clone()));
            groups.len() - 1
        });
        for agg in &mut groups[i].1 {
            agg.push(f)?;
        }
    }
    for (values, aggregates) in groups {
        let summaries: Vec<_> = aggregates.into_iter().map(Aggregate::finish).collect();
        let attributes = columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let v = match c {
                    Column::Key(j) => &values[*j],
                    Column::Summary(j) => &summaries[*j],
                };
                field_value(v.clone(), out.schema.fields()[i].field_type)
            })
            .collect();
        out.features.push(Feature { fid: out.features.len() as u64, geometry: None, attributes });
    }
    if let Some((i, desc)) = order {
        let mut sorted: Vec<_> = out.features.into_iter().map(|feature| (Value::from_field(&feature.attributes[i]), feature)).collect();
        sorted.sort_by(|(a, _), (b, _)| {
            let o = match (a, b) {
                (Value::Null, Value::Null) => Ordering::Equal,
                (Value::Null, _) => Ordering::Less,
                (_, Value::Null) => Ordering::Greater,
                _ => compare(a, b).unwrap_or(Ordering::Equal),
            };
            if desc { o.reverse() } else { o }
        });
        out.features = sorted.into_iter().map(|(_, feature)| feature).collect();
    }
    if let Some(n) = limit {
        out.features.truncate(n);
    }
    Ok((out, s.limit))
}

// SQLite groups an exactly integral real with the corresponding integer, while
// retaining distinct integers beyond f64's exact integer range.
fn group_key(v: &Value) -> String {
    if let Value::Float(f) = v {
        if f.is_nan() {
            return key(&Value::Null);
        }
        if *f >= i64::MIN as f64 && *f < -(i64::MIN as f64) && f.fract() == 0.0 {
            return key(&Value::Int(*f as i64));
        }
    }
    key(v)
}
