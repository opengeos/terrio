//! SQLite CREATE TABLE declarations, inspected without executing SQL or loading extensions.
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Column {
    pub name: String,
    pub data_type: String,
    pub not_null: bool,
    pub primary_key: usize,
    pub default: Option<String>,
    pub collation: String,
    pub not_null_declared: bool,
    pub generated: Option<GeneratedColumn>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GeneratedColumn {
    pub expression: String,
    pub stored: bool,
}
#[derive(Debug, Clone)]
pub(super) struct UniqueKey {
    pub columns: Vec<String>,
    pub collations: Vec<Option<String>>,
    pub descending: Vec<bool>,
    pub primary: bool,
}
#[derive(Debug, Clone)]
pub(super) struct IndexDeclaration {
    pub key: UniqueKey,
    pub unique: bool,
}
#[derive(Debug, Clone)]
pub(super) struct ForeignKey {
    pub columns: Vec<String>,
    pub table: String,
    pub parent_columns: Vec<String>,
}
#[derive(Debug, Clone)]
pub(super) struct Schema {
    pub columns: Vec<Column>,
    pub foreign_keys: Vec<ForeignKey>,
    pub unique: Vec<UniqueKey>,
    pub without_rowid: bool,
    pub rowid_alias: Option<usize>,
    pub virtual_module: Option<String>,
    pub strict: bool,
}
#[derive(Debug, Clone)]
struct Token {
    value: String,
    quoted: bool,
    start: usize,
    end: usize,
}
impl Token {
    fn is(&self, value: &str) -> bool {
        !self.quoted && self.value.eq_ignore_ascii_case(value)
    }
}
fn bad(message: &str) -> Error {
    Error::InvalidData(format!("GeoPackage table declaration: {message}"))
}
fn tokens(sql: &str) -> Result<Vec<Token>> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at].is_ascii_whitespace() {
            at += 1;
            continue;
        }
        if bytes[at..].starts_with(b"--") {
            at += bytes[at..].iter().position(|b| *b == b'\n').unwrap_or(bytes.len() - at);
            continue;
        }
        if bytes[at..].starts_with(b"/*") {
            let length = bytes[at + 2..].windows(2).position(|pair| pair == b"*/").ok_or_else(|| bad("unterminated comment"))?;
            at += length + 4;
            continue;
        }
        let start = at;
        let quote = match bytes[at] {
            b'\'' | b'"' | b'`' => Some(bytes[at]),
            b'[' => Some(b']'),
            _ => None,
        };
        if let Some(close) = quote {
            at += 1;
            let mut value = Vec::new();
            loop {
                let byte = *bytes.get(at).ok_or_else(|| bad("unterminated quoted token"))?;
                at += 1;
                if byte == close {
                    if close != b']' && bytes.get(at) == Some(&close) {
                        value.push(close);
                        at += 1;
                    } else {
                        break;
                    }
                } else {
                    value.push(byte);
                }
            }
            out.push(Token { value: String::from_utf8(value).map_err(|_| bad("invalid UTF-8 identifier"))?, quoted: true, start, end: at });
        } else {
            if b"(),;+-".contains(&bytes[at]) {
                at += 1;
            } else {
                while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
                    let exponent_sign = matches!(bytes[at], b'+' | b'-')
                        && at > start
                        && matches!(bytes[at - 1], b'e' | b'E')
                        && (bytes[start].is_ascii_digit() || bytes[start] == b'.');
                    if b"(),;+-'\"`[]".contains(&bytes[at]) && !exponent_sign {
                        break;
                    }
                    at += 1;
                }
                if at == start {
                    return Err(bad("unexpected punctuation"));
                }
            }
            out.push(Token { value: sql[start..at].to_string(), quoted: false, start, end: at });
        }
    }
    Ok(out)
}
fn close(tokens: &[Token], open: usize) -> Result<usize> {
    let mut depth = 0;
    for (at, token) in tokens.iter().enumerate().skip(open) {
        if token.is("(") {
            depth += 1;
        }
        if token.is(")") {
            depth -= 1;
            if depth == 0 {
                return Ok(at);
            }
        }
    }
    Err(bad("unclosed parentheses"))
}
fn names(tokens: &[Token]) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let mut at = 0;
    while at < tokens.len() {
        names.push(tokens[at].value.clone());
        at += 1;
        while at < tokens.len() && !tokens[at].is(",") {
            at += 1;
        }
        at += 1;
    }
    if names.is_empty() {
        return Err(bad("empty column list"));
    }
    Ok(names)
}
fn reference(tokens: &[Token], columns: Vec<String>) -> Result<ForeignKey> {
    let table = tokens.first().ok_or_else(|| bad("missing referenced table"))?.value.clone();
    let parent_columns = if tokens.get(1).is_some_and(|token| token.is("(")) { names(&tokens[2..close(tokens, 1)?])? } else { Vec::new() };
    Ok(ForeignKey { columns, table, parent_columns })
}
fn unique_key(tokens: &[Token]) -> Option<UniqueKey> {
    let mut key = UniqueKey { columns: Vec::new(), collations: Vec::new(), descending: Vec::new(), primary: false };
    let mut at = 0;
    while at < tokens.len() {
        let start = at;
        while at < tokens.len() && !tokens[at].is(",") {
            if tokens[at].is("(") {
                at = close(tokens, at).ok()?;
            }
            at += 1;
        }
        let mut term = &tokens[start..at];
        at += 1;
        key.descending.push(term.last().is_some_and(|t| t.is("DESC")));
        if term.last().is_some_and(|t| t.is("ASC") || t.is("DESC")) {
            term = &term[..term.len() - 1];
        }
        let mut collation = None;
        if term.len() >= 3 && term[term.len() - 2].is("COLLATE") {
            collation = Some(term.last()?.value.to_ascii_uppercase());
            term = &term[..term.len() - 2];
        }
        while term.first().is_some_and(|t| t.is("(")) && close(term, 0).ok()? == term.len() - 1 {
            term = &term[1..term.len() - 1];
        }
        if term.len() != 1 {
            return None;
        }
        key.columns.push(term[0].value.clone());
        key.collations.push(collation);
    }
    (!key.columns.is_empty()).then_some(key)
}
impl Schema {
    pub(super) fn unique_index(sql: &str) -> Result<Option<UniqueKey>> {
        Ok(Self::index(sql)?.filter(|index| index.unique).map(|index| index.key))
    }
    /// Plain-column indexes; expression and partial indexes need the SQL evaluator.
    pub(super) fn index(sql: &str) -> Result<Option<IndexDeclaration>> {
        let all = tokens(sql)?;
        let keyword = if all.get(1).is_some_and(|t| t.is("UNIQUE")) { 2 } else { 1 };
        if !all.first().is_some_and(|t| t.is("CREATE")) || !all.get(keyword).is_some_and(|t| t.is("INDEX")) {
            return Err(bad("malformed index declaration"));
        }
        let Some(open) = all.iter().position(|t| t.is("(")) else { return Ok(None) };
        let end = close(&all, open)?;
        if all[end + 1..].iter().any(|t| t.is("WHERE")) {
            return Ok(None);
        }
        Ok(unique_key(&all[open + 1..end]).map(|key| IndexDeclaration { key, unique: all.get(1).is_some_and(|t| t.is("UNIQUE")) }))
    }
    pub fn parse(sql: &str) -> Result<Self> {
        let all = tokens(sql)?;
        let open = all.iter().position(|token| token.is("(")).ok_or_else(|| bad("no table body"))?;
        let end = close(&all, open)?;
        let mut schema = Self {
            columns: Vec::new(),
            foreign_keys: Vec::new(),
            unique: Vec::new(),
            without_rowid: all[end + 1..].windows(2).any(|p| p[0].is("WITHOUT") && p[1].is("ROWID")),
            rowid_alias: None,
            virtual_module: all[..open].windows(2).find(|pair| pair[0].is("USING")).map(|pair| pair[1].value.to_ascii_lowercase()),
            strict: all[end + 1..].iter().any(|token| token.is("STRICT")),
        };
        let mut primary = Vec::new();
        let mut descending_aliases = Vec::new();
        let mut at = open + 1;
        while at < end {
            let start = at;
            while at < end && !all[at].is(",") {
                if all[at].is("(") {
                    at = close(&all, at)?;
                }
                at += 1;
            }
            let mut part = &all[start..at];
            at += 1;
            if part.first().is_some_and(|t| t.is("CONSTRAINT")) {
                part = part.get(2..).ok_or_else(|| bad("missing named constraint"))?;
            }
            let first = part.first().ok_or_else(|| bad("empty table item"))?;
            if first.is("PRIMARY") || first.is("UNIQUE") || first.is("FOREIGN") {
                let begin = part.iter().position(|t| t.is("(")).ok_or_else(|| bad("constraint column list missing"))?;
                let finish = close(part, begin)?;
                let columns = names(&part[begin + 1..finish])?;
                if first.is("PRIMARY") {
                    primary = columns.clone();
                }
                if (first.is("UNIQUE") || first.is("PRIMARY"))
                    && let Some(mut key) = unique_key(&part[begin + 1..finish])
                {
                    key.primary = first.is("PRIMARY");
                    schema.unique.push(key);
                }
                if first.is("FOREIGN") {
                    let index = part.iter().position(|t| t.is("REFERENCES")).ok_or_else(|| bad("foreign key reference missing"))?;
                    schema.foreign_keys.push(reference(&part[index + 1..], columns)?);
                }
                continue;
            }
            if first.is("CHECK") {
                continue;
            }
            let name = first.value.clone();
            let mut kind_end = 1;
            while kind_end < part.len() {
                if ["PRIMARY", "NOT", "NULL", "UNIQUE", "CHECK", "DEFAULT", "COLLATE", "REFERENCES", "CONSTRAINT", "GENERATED", "AS"]
                    .iter()
                    .any(|word| part[kind_end].is(word))
                {
                    break;
                }
                if part[kind_end].is("(") {
                    kind_end = close(part, kind_end)?;
                }
                kind_end += 1;
            }
            let data_type = part[1..kind_end]
                .iter()
                .map(|t| t.value.as_str())
                .collect::<Vec<_>>()
                .join(" ")
                .replace(" (", "(")
                .replace("( ", "(")
                .replace(" )", ")")
                .to_ascii_uppercase();
            let rest = &part[kind_end..];
            let mut column = Column {
                name: name.clone(),
                data_type,
                not_null: false,
                primary_key: 0,
                default: None,
                collation: "BINARY".into(),
                not_null_declared: false,
                generated: None,
            };
            let mut i = 0;
            while i < rest.len() {
                if rest[i].is("NOT") && rest.get(i + 1).is_some_and(|t| t.is("NULL")) {
                    column.not_null = true;
                    column.not_null_declared = true;
                }
                if rest[i].is("PRIMARY") && rest.get(i + 1).is_some_and(|t| t.is("KEY")) {
                    primary.push(name.clone());
                    if rest.get(i + 2).is_some_and(|t| t.is("DESC")) {
                        descending_aliases.push(name.clone());
                    }
                    schema.unique.push(UniqueKey {
                        columns: vec![name.clone()],
                        collations: vec![None],
                        descending: vec![rest.get(i + 2).is_some_and(|t| t.is("DESC"))],
                        primary: true,
                    });
                }
                if rest[i].is("UNIQUE") {
                    schema.unique.push(UniqueKey {
                        columns: vec![name.clone()],
                        collations: vec![None],
                        descending: vec![false],
                        primary: false,
                    });
                }
                if rest[i].is("COLLATE") {
                    column.collation = rest.get(i + 1).ok_or_else(|| bad("missing collation"))?.value.to_ascii_uppercase();
                }
                if rest[i].is("REFERENCES") {
                    schema.foreign_keys.push(reference(&rest[i + 1..], vec![name.clone()])?);
                }
                if rest[i].is("AS") && rest.get(i + 1).is_some_and(|t| t.is("(")) {
                    let end = close(rest, i + 1)?;
                    column.generated = Some(GeneratedColumn {
                        expression: sql[rest[i + 1].end..rest[end].start].trim().into(),
                        stored: rest[end + 1..].iter().any(|t| t.is("STORED")),
                    });
                }
                if rest[i].is("DEFAULT") {
                    i += 1;
                    let first = rest.get(i).ok_or_else(|| bad("default value missing"))?;
                    let begin;
                    let finish;
                    if first.is("(") {
                        let end = close(rest, i)?;
                        begin = first.end;
                        finish = rest[end].start;
                        i = end;
                    } else {
                        begin = first.start;
                        let blob_literal = first.value.eq_ignore_ascii_case("x")
                            && rest
                                .get(i + 1)
                                .is_some_and(|next| next.quoted && next.start == first.end && sql.as_bytes()[next.start] == b'\'');
                        if first.is("+") || first.is("-") || blob_literal {
                            i += 1;
                        }
                        finish = rest.get(i).ok_or_else(|| bad("default value missing"))?.end;
                    }
                    column.default = Some(sql[begin..finish].trim().to_string());
                } else if rest[i].is("(") {
                    i = close(rest, i)?;
                }
                i += 1;
            }
            schema.columns.push(column);
        }
        for (index, name) in primary.iter().enumerate() {
            let column =
                schema.columns.iter_mut().find(|c| c.name.eq_ignore_ascii_case(name)).ok_or_else(|| bad("unknown primary key column"))?;
            column.primary_key = index + 1;
            if schema.without_rowid || primary.len() == 1 && column.data_type == "INTEGER" {
                column.not_null = true;
            }
        }
        if !schema.without_rowid && primary.len() == 1 && !descending_aliases.contains(&primary[0]) {
            schema.rowid_alias = schema.columns.iter().position(|column| column.primary_key == 1 && column.data_type == "INTEGER");
        }
        Ok(schema)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn table_constraints_quotes_comments_and_defaults() {
        let sql = r#"CREATE TABLE "a,b" ("x" INTEGER /* pk */ PRIMARY KEY, "y" TEXT DEFAULT 'NOT NULL, UNIQUE', z DATETIME NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), CONSTRAINT "a''b" UNIQUE (y,z), FOREIGN KEY (y,z) REFERENCES "other" ("a", "b"))"#;
        let schema = Schema::parse(sql).unwrap();
        assert_eq!(schema.columns.len(), 3);
        assert_eq!(schema.columns[0].primary_key, 1);
        assert!(schema.columns[0].not_null);
        assert!(!schema.columns[1].not_null);
        assert_eq!(schema.columns[1].default.as_deref(), Some("'NOT NULL, UNIQUE'"));
        assert_eq!(schema.columns[2].default.as_deref(), Some("strftime('%Y-%m-%dT%H:%M:%fZ','now')"));
        assert_eq!(schema.foreign_keys[0].columns, ["y", "z"]);
        assert_eq!(schema.foreign_keys[0].parent_columns, ["a", "b"]);
        assert_eq!(schema.foreign_keys[0].table, "other");
    }
    #[test]
    fn default_literals_match_sqlite_pragma_spelling() {
        let schema = Schema::parse("CREATE TABLE sample(id INTEGER PRIMARY KEY, scientific DOUBLE DEFAULT 1e-3, payload BLOB DEFAULT X'00ff', quoted TEXT DEFAULT 'a''b')").unwrap();
        assert_eq!(schema.columns[1].default.as_deref(), Some("1e-3"));
        assert_eq!(schema.columns[2].default.as_deref(), Some("X'00ff'"));
        assert_eq!(schema.columns[3].default.as_deref(), Some("'a''b'"));
    }
    #[test]
    fn composite_primary_key_and_escaped_identifiers() {
        let schema = Schema::parse(r#"CREATE TABLE t ("a""b" TEXT, [c] INTEGER, PRIMARY KEY ("a""b", c)) WITHOUT ROWID"#).unwrap();
        assert_eq!(schema.columns[0].name, "a\"b");
        assert_eq!(schema.columns[1].primary_key, 2);
        assert!(schema.columns.iter().all(|c| c.not_null));
    }
}
