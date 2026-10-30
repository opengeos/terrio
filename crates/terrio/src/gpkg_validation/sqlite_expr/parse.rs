use super::*;
use lex::Token;

pub(super) struct Parser {
    tokens: Vec<Token>,
    at: usize,
    depth: usize,
}
impl Parser {
    fn is(&self, text: &str) -> bool {
        self.tokens.get(self.at).is_some_and(|t| t.is(text))
    }
    fn eat(&mut self, text: &str) -> bool {
        if self.is(text) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, text: &str) -> Result<()> {
        if self.eat(text) { Ok(()) } else { Err(bad(&format!("expected {text}"))) }
    }
    fn name(&mut self) -> Result<String> {
        match self.tokens.get(self.at) {
            Some(Token::Name(name, _)) | Some(Token::Literal(Value::Text(name), _)) => {
                let name = name.clone();
                self.at += 1;
                Ok(name)
            }
            _ => Err(bad("expected identifier")),
        }
    }
    pub fn parse(sql: &str) -> Result<Expr> {
        Self::parse_inner(sql, false).map(|(expr, _)| expr)
    }
    pub fn index_term(sql: &str) -> Result<(Expr, bool)> {
        Self::parse_inner(sql, true)
    }
    fn parse_inner(sql: &str, index: bool) -> Result<(Expr, bool)> {
        let mut parser = Self { tokens: lex::tokens(sql)?, at: 0, depth: 0 };
        let root = parser.expression(0)?;
        let descending = index && parser.eat("DESC");
        if index && !descending {
            parser.eat("ASC");
        }
        if parser.at != parser.tokens.len() {
            return Err(bad("unexpected trailing SQL"));
        }
        let mut todo = vec![(&root, 0)];
        while let Some((expr, depth)) = todo.pop() {
            if depth > 128 {
                return Err(Error::Unsupported("SQLite expression depth exceeds 128".into()));
            }
            todo.extend(expr.children().into_iter().map(|e| (e, depth + 1)));
        }
        Ok((root, descending))
    }
    fn expression(&mut self, min: u8) -> Result<Expr> {
        self.depth += 1;
        if self.depth > 128 {
            return Err(Error::Unsupported("SQLite expression depth exceeds 128".into()));
        }
        let mut left = self.prefix()?;
        loop {
            if min <= 4
                && (self.is("ISNULL") || self.is("NOTNULL") || self.is("NOT") && self.tokens.get(self.at + 1).is_some_and(|t| t.is("NULL")))
            {
                let not = !self.is("ISNULL");
                let two = self.is("NOT");
                self.at += if two { 2 } else { 1 };
                left = Expr::Binary(if not { "IS NOT" } else { "IS" }.into(), Box::new(left), Box::new(Expr::Literal(Value::Null)));
                continue;
            }
            if self.is("COLLATE") && min <= 10 {
                self.at += 1;
                let collation = self.name()?.to_ascii_uppercase();
                if !matches!(collation.as_str(), "BINARY" | "NOCASE" | "RTRIM") {
                    return Err(Error::Unsupported(format!("SQLite collation {collation}")));
                }
                left = Expr::Collate(Box::new(left), collation);
                continue;
            }
            let negated =
                self.is("NOT") && self.tokens.get(self.at + 1).is_some_and(|t| ["BETWEEN", "IN", "LIKE", "GLOB"].iter().any(|s| t.is(s)));
            let offset = usize::from(negated);
            let special =
                ["BETWEEN", "IN", "LIKE", "GLOB", "IS"].into_iter().find(|s| self.tokens.get(self.at + offset).is_some_and(|t| t.is(s)));
            if min <= 4
                && let Some(special) = special
            {
                self.at += 1 + offset;
                left = match special {
                    "BETWEEN" => {
                        let low = self.expression(5)?;
                        self.expect("AND")?;
                        let high = self.expression(5)?;
                        Expr::Between(Box::new(left), Box::new(low), Box::new(high), negated)
                    }
                    "IN" => {
                        self.expect("(")?;
                        let mut list = Vec::new();
                        if !self.eat(")") {
                            loop {
                                list.push(self.expression(0)?);
                                if self.eat(")") {
                                    break;
                                }
                                self.expect(",")?;
                            }
                        }
                        Expr::In(Box::new(left), list, negated)
                    }
                    "IS" => {
                        let not = self.eat("NOT");
                        if self.eat("DISTINCT") {
                            self.expect("FROM")?;
                            Expr::Binary(if not { "IS" } else { "IS NOT" }.into(), Box::new(left), Box::new(self.expression(5)?))
                        } else {
                            Expr::Binary(if not { "IS NOT" } else { "IS" }.into(), Box::new(left), Box::new(self.expression(5)?))
                        }
                    }
                    _ => {
                        let pattern = self.expression(5)?;
                        let escape = if self.eat("ESCAPE") { Some(Box::new(self.expression(5)?)) } else { None };
                        if special == "GLOB" && escape.is_some() {
                            return Err(bad("GLOB does not accept ESCAPE"));
                        }
                        Expr::Pattern(Box::new(left), Box::new(pattern), escape, special == "GLOB", negated)
                    }
                };
                continue;
            }
            let operator = [
                ("OR", 1),
                ("AND", 2),
                ("=", 4),
                ("==", 4),
                ("!=", 4),
                ("<>", 4),
                ("<", 5),
                (">", 5),
                ("<=", 5),
                (">=", 5),
                ("&", 6),
                ("|", 6),
                ("<<", 6),
                (">>", 6),
                ("+", 7),
                ("-", 7),
                ("*", 8),
                ("/", 8),
                ("%", 8),
                ("||", 9),
            ]
            .into_iter()
            .find(|(name, bp)| *bp >= min && self.is(name));
            let Some((operator, bp)) = operator else { break };
            self.at += 1;
            let right = self.expression(bp + 1)?;
            left = Expr::Binary(operator.into(), Box::new(left), Box::new(right));
        }
        self.depth -= 1;
        Ok(left)
    }
    fn prefix(&mut self) -> Result<Expr> {
        if self.eat("NOT") {
            return Ok(Expr::Unary("NOT".into(), Box::new(self.expression(3)?)));
        }
        for operator in ["+", "-", "~"] {
            if self.eat(operator) {
                if operator == "-"
                    && matches!(self.tokens.get(self.at),Some(Token::Literal(Value::Real(n),true)) if *n==9_223_372_036_854_775_808.0)
                {
                    self.at += 1;
                    return Ok(Expr::Literal(Value::Int(i64::MIN)));
                }
                let value = self.expression(11)?;
                if operator == "-" && matches!(value, Expr::MinMagnitude) {
                    return Ok(Expr::Literal(Value::Int(i64::MIN)));
                }
                if operator == "-" && matches!(value, Expr::Literal(Value::Int(i64::MIN))) {
                    return Err(bad("hex literal too big"));
                }
                return Ok(Expr::Unary(operator.into(), Box::new(value)));
            }
        }
        if self.eat("(") {
            let expr = self.expression(0)?;
            self.expect(")")?;
            return Ok(expr);
        }
        if self.eat("CASE") {
            let base = if self.is("WHEN") { None } else { Some(Box::new(self.expression(0)?)) };
            let mut arms = Vec::new();
            while self.eat("WHEN") {
                let condition = self.expression(0)?;
                self.expect("THEN")?;
                let value = self.expression(0)?;
                arms.push((condition, value));
            }
            let otherwise = if self.eat("ELSE") { self.expression(0)? } else { Expr::Literal(Value::Null) };
            self.expect("END")?;
            if arms.is_empty() {
                return Err(bad("CASE requires WHEN"));
            }
            return Ok(Expr::Case(base, arms, Box::new(otherwise)));
        }
        if self.eat("CAST") {
            self.expect("(")?;
            let value = self.expression(0)?;
            self.expect("AS")?;
            let mut kind = self.name()?;
            while !self.is(")") {
                if self.eat("(") {
                    for i in 0..2 {
                        if !self.eat("+") {
                            self.eat("-");
                        }
                        if !matches!(self.tokens.get(self.at), Some(Token::Literal(Value::Int(_) | Value::Real(_), _))) {
                            return Err(bad("invalid CAST type precision"));
                        }
                        self.at += 1;
                        if i == 0 && self.eat(",") {
                            continue;
                        }
                        break;
                    }
                    self.expect(")")?;
                    continue;
                }
                kind.push(' ');
                kind.push_str(&self.name()?);
            }
            self.expect(")")?;
            return Ok(Expr::Cast(Box::new(value), kind.to_ascii_uppercase()));
        }
        let token = self.tokens.get(self.at).cloned().ok_or_else(|| bad("expression is missing"))?;
        self.at += 1;
        match token {
            Token::Literal(_, true) => Ok(Expr::MinMagnitude),
            Token::Literal(value, false) => Ok(Expr::Literal(value)),
            Token::Name(name, quoted) => {
                if quoted == 0 && name.eq_ignore_ascii_case("NULL") {
                    return Ok(Expr::Literal(Value::Null));
                }
                if self.eat("(") {
                    let name = name.to_ascii_lowercase();
                    functions::known(&name)?;
                    let mut args = Vec::new();
                    if !self.eat(")") {
                        loop {
                            args.push(self.expression(0)?);
                            if self.eat(")") {
                                break;
                            }
                            self.expect(",")?;
                        }
                    }
                    functions::validate(&name, &args)?;
                    return Ok(Expr::Call(name, args));
                }
                let mut parts = vec![name];
                while self.eat(".") {
                    parts.push(self.name()?);
                }
                Ok(Expr::Column(parts, quoted))
            }
            _ => Err(bad("expected scalar expression")),
        }
    }
}
