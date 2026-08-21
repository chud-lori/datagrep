use std::sync::Arc;

use datagrep_api::{Document, Value};

use super::date::parse_iso8601_utc_micros;
use super::error::MongoError;

#[derive(Debug, Clone, PartialEq)]
pub enum ParsedMongo {
    Chain(MongoStatement),
    RawCommand(Value),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MongoStatement {
    pub collection: String,
    pub method: String,
    pub args: Vec<Value>,
    pub modifiers: Vec<(String, Vec<Value>)>,
}

pub fn parse(src: &str) -> Result<ParsedMongo, MongoError> {
    let mut p = Parser {
        src,
        bytes: src.as_bytes(),
        pos: 0,
    };
    p.skip_trivia();
    let result = if p.peek() == Some(b'{') {
        let v = p.parse_value()?;
        ParsedMongo::RawCommand(v)
    } else {
        ParsedMongo::Chain(p.parse_chain()?)
    };
    p.skip_trivia();
    if p.pos != p.bytes.len() {
        return Err(MongoError::TrailingInput {
            at: p.pos,
            found: p.remaining_preview(),
        });
    }
    Ok(result)
}

struct Parser<'s> {
    src: &'s str,
    bytes: &'s [u8],
    pos: usize,
}

fn is_js_operator_byte(b: u8) -> bool {
    matches!(
        b,
        b'+' | b'-'
            | b'*'
            | b'/'
            | b'%'
            | b'<'
            | b'>'
            | b'='
            | b'&'
            | b'|'
            | b'?'
            | b'!'
            | b'~'
            | b'^'
    )
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b >= 0x80
}

fn is_ident_continue(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

impl<'s> Parser<'s> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn remaining_preview(&self) -> String {
        let end = (self.pos + 20).min(self.bytes.len());
        self.src[self.pos..end].to_string()
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b) if b.is_ascii_whitespace() => self.pos += 1,
                Some(b'/') if self.peek_at(1) == Some(b'/') => {
                    self.pos += 2;
                    while self.peek().is_some_and(|b| b != b'\n') {
                        self.pos += 1;
                    }
                }
                Some(b'#') => {
                    self.pos += 1;
                    while self.peek().is_some_and(|b| b != b'\n') {
                        self.pos += 1;
                    }
                }
                Some(b'/') if self.peek_at(1) == Some(b'*') => {
                    self.pos += 2;
                    while self.pos < self.bytes.len()
                        && !(self.peek() == Some(b'*') && self.peek_at(1) == Some(b'/'))
                    {
                        self.pos += 1;
                    }
                    self.pos = (self.pos + 2).min(self.bytes.len());
                }
                _ => break,
            }
        }
    }

    fn expect_byte(&mut self, b: u8, expected: &'static str) -> Result<(), MongoError> {
        self.skip_trivia();
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err_here(expected))
        }
    }

    fn err_here(&self, expected: &'static str) -> MongoError {
        match self.peek() {
            Some(_) => MongoError::Unexpected {
                at: self.pos,
                expected,
                found: self.remaining_preview(),
            },
            None => MongoError::UnexpectedEof {
                at: self.pos,
                expected,
            },
        }
    }

    fn parse_ident(&mut self) -> Result<&'s str, MongoError> {
        self.skip_trivia();
        let start = self.pos;
        if !self.peek().is_some_and(is_ident_start) {
            return Err(self.err_here("an identifier"));
        }
        self.pos += 1;
        while self.peek().is_some_and(is_ident_continue) {
            self.pos += 1;
        }
        Ok(&self.src[start..self.pos])
    }

    fn parse_chain(&mut self) -> Result<MongoStatement, MongoError> {
        let head = self.parse_ident()?;
        if head != "db" {
            return Err(MongoError::UnsupportedJs);
        }
        self.expect_byte(b'.', "'.' after db")?;
        let collection = self.parse_collection_ref()?;
        self.expect_byte(b'.', "'.' after collection name")?;
        let (method, args) = self.parse_method_call()?;
        let mut modifiers = Vec::new();
        loop {
            self.skip_trivia();
            if self.peek() == Some(b'.') {
                self.pos += 1;
                let (m, a) = self.parse_method_call()?;
                modifiers.push((m.to_string(), a));
            } else {
                break;
            }
        }
        Ok(MongoStatement {
            collection,
            method: method.to_string(),
            args,
            modifiers,
        })
    }

    fn parse_collection_ref(&mut self) -> Result<String, MongoError> {
        let ident = self.parse_ident()?;
        if ident == "getCollection" {
            self.expect_byte(b'(', "'(' after getCollection")?;
            self.skip_trivia();
            let name = self.parse_string()?;
            self.expect_byte(b')', "')' to close getCollection(...)")?;
            Ok(name)
        } else {
            Ok(ident.to_string())
        }
    }

    fn parse_method_call(&mut self) -> Result<(&'s str, Vec<Value>), MongoError> {
        let name = self.parse_ident()?;
        self.expect_byte(b'(', "'(' to start an argument list")?;
        let args = self.parse_value_list(b')')?;
        self.expect_byte(b')', "')' to close an argument list")?;
        Ok((name, args))
    }

    fn parse_value_list(&mut self, end: u8) -> Result<Vec<Value>, MongoError> {
        let mut out = Vec::new();
        self.skip_trivia();
        if self.peek() == Some(end) {
            return Ok(out);
        }
        loop {
            out.push(self.parse_value()?);
            self.skip_trivia();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_trivia();
                    if self.peek() == Some(end) {
                        break; // trailing comma
                    }
                }
                Some(b) if b == end => break,
                Some(b) if is_js_operator_byte(b) => return Err(MongoError::UnsupportedJs),
                _ => return Err(self.err_here("',' or the end of the argument list")),
            }
        }
        Ok(out)
    }

    fn parse_value(&mut self) -> Result<Value, MongoError> {
        self.skip_trivia();
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') | Some(b'\'') => Ok(Value::Str(Arc::from(self.parse_string()?.as_str()))),
            Some(b) if b.is_ascii_digit() => self.parse_number(),
            Some(b'-') | Some(b'+') if self.peek_at(1).is_some_and(|b| b.is_ascii_digit()) => {
                self.parse_number()
            }
            Some(b) if is_ident_start(b) => self.parse_ident_value(),
            Some(_) => Err(MongoError::UnsupportedJs),
            None => Err(MongoError::UnexpectedEof {
                at: self.pos,
                expected: "a value",
            }),
        }
    }

    fn parse_ident_value(&mut self) -> Result<Value, MongoError> {
        let start = self.pos;
        let ident = self.parse_ident()?;
        match ident {
            "true" => return Ok(Value::Bool(true)),
            "false" => return Ok(Value::Bool(false)),
            "null" => return Ok(Value::Null),
            "ObjectId" => return self.parse_object_id(),
            "ISODate" => return self.parse_iso_date(),
            "NumberLong" => return self.parse_number_long(),
            "NumberDecimal" => return self.parse_number_decimal(),
            "NumberInt" => return self.parse_number_int(),
            _ => {}
        }
        self.pos = start;
        Err(MongoError::UnsupportedJs)
    }

    fn parse_ctor_string_arg(&mut self) -> Result<(String, usize), MongoError> {
        self.expect_byte(b'(', "'(' to start a constructor argument")?;
        self.skip_trivia();
        let at = self.pos;
        let s = self.parse_string()?;
        self.skip_trivia();
        self.expect_byte(b')', "')' to close a constructor call")?;
        Ok((s, at))
    }

    fn parse_object_id(&mut self) -> Result<Value, MongoError> {
        let (hex, at) = self.parse_ctor_string_arg()?;
        if hex.len() != 24 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(MongoError::InvalidLiteral {
                kind: "ObjectId",
                value: hex,
                at,
                reason: "expected exactly 24 hex characters",
            });
        }
        Ok(Value::Str(Arc::from(hex.to_ascii_lowercase().as_str())))
    }

    fn parse_iso_date(&mut self) -> Result<Value, MongoError> {
        let (s, at) = self.parse_ctor_string_arg()?;
        let micros = parse_iso8601_utc_micros(&s).ok_or_else(|| MongoError::InvalidLiteral {
            kind: "ISODate",
            value: s.clone(),
            at,
            reason: "expected an ISO-8601 UTC timestamp, e.g. 2024-01-15T10:30:00Z",
        })?;
        Ok(Value::Timestamp {
            micros,
            tz: datagrep_api::TzSpec::Utc,
        })
    }

    fn parse_number_long(&mut self) -> Result<Value, MongoError> {
        self.expect_byte(b'(', "'(' to start a constructor argument")?;
        self.skip_trivia();
        let (text, at) = self.parse_ctor_inner_number_or_string()?;
        self.skip_trivia();
        self.expect_byte(b')', "')' to close a constructor call")?;
        let n: i64 = text
            .trim()
            .parse()
            .map_err(|_| MongoError::InvalidLiteral {
                kind: "NumberLong",
                value: text,
                at,
                reason: "expected an integer",
            })?;
        Ok(Value::I64(n))
    }

    fn parse_number_int(&mut self) -> Result<Value, MongoError> {
        self.expect_byte(b'(', "'(' to start a constructor argument")?;
        self.skip_trivia();
        let (text, at) = self.parse_ctor_inner_number_or_string()?;
        self.skip_trivia();
        self.expect_byte(b')', "')' to close a constructor call")?;
        let n: i32 = text
            .trim()
            .parse()
            .map_err(|_| MongoError::InvalidLiteral {
                kind: "NumberInt",
                value: text,
                at,
                reason: "expected a 32-bit integer",
            })?;
        Ok(Value::I64(n as i64))
    }

    fn parse_number_decimal(&mut self) -> Result<Value, MongoError> {
        self.expect_byte(b'(', "'(' to start a constructor argument")?;
        self.skip_trivia();
        let (text, at) = self.parse_ctor_inner_number_or_string()?;
        self.skip_trivia();
        self.expect_byte(b')', "')' to close a constructor call")?;
        if text.trim().is_empty() || !looks_like_number(text.trim()) {
            return Err(MongoError::InvalidLiteral {
                kind: "NumberDecimal",
                value: text,
                at,
                reason: "expected a decimal number",
            });
        }
        Ok(Value::Decimal(Arc::from(text.trim())))
    }

    fn parse_ctor_inner_number_or_string(&mut self) -> Result<(String, usize), MongoError> {
        let at = self.pos;
        match self.peek() {
            Some(b'"') | Some(b'\'') => Ok((self.parse_string()?, at)),
            Some(b) if b.is_ascii_digit() || b == b'-' || b == b'+' => {
                let start = self.pos;
                self.pos += 1;
                while self.peek().is_some_and(|b| {
                    b.is_ascii_digit()
                        || b == b'.'
                        || b == b'e'
                        || b == b'E'
                        || b == b'+'
                        || b == b'-'
                }) {
                    self.pos += 1;
                }
                Ok((self.src[start..self.pos].to_string(), at))
            }
            _ => Err(self.err_here("a number or a quoted string")),
        }
    }

    fn parse_number(&mut self) -> Result<Value, MongoError> {
        let start = self.pos;
        if matches!(self.peek(), Some(b'-') | Some(b'+')) {
            self.pos += 1;
        }
        let mut is_float = false;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') && self.peek_at(1).is_some_and(|b| b.is_ascii_digit()) {
            is_float = true;
            self.pos += 1;
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            let mut lookahead = self.pos + 1;
            if matches!(self.bytes.get(lookahead), Some(b'+') | Some(b'-')) {
                lookahead += 1;
            }
            if self.bytes.get(lookahead).is_some_and(u8::is_ascii_digit) {
                is_float = true;
                self.pos = lookahead;
                while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
        }
        let text = &self.src[start..self.pos];
        if is_float {
            let f: f64 = text.parse().map_err(|_| MongoError::InvalidLiteral {
                kind: "number",
                value: text.to_string(),
                at: start,
                reason: "not a valid number",
            })?;
            Ok(Value::F64(f))
        } else {
            match text.parse::<i64>() {
                Ok(n) => Ok(Value::I64(n)),
                Err(_) => {
                    let f: f64 = text.parse().map_err(|_| MongoError::InvalidLiteral {
                        kind: "number",
                        value: text.to_string(),
                        at: start,
                        reason: "not a valid number",
                    })?;
                    Ok(Value::F64(f))
                }
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, MongoError> {
        self.skip_trivia();
        let quote = match self.peek() {
            Some(b @ (b'"' | b'\'')) => b,
            _ => return Err(self.err_here("a quoted string")),
        };
        let start = self.pos;
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => {
                    return Err(MongoError::UnexpectedEof {
                        at: start,
                        expected: "a closing quote",
                    })
                }
                Some(b) if b == quote => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some(b'n') => {
                            out.push('\n');
                            self.pos += 1;
                        }
                        Some(b'r') => {
                            out.push('\r');
                            self.pos += 1;
                        }
                        Some(b't') => {
                            out.push('\t');
                            self.pos += 1;
                        }
                        Some(b'b') => {
                            out.push('\u{8}');
                            self.pos += 1;
                        }
                        Some(b'f') => {
                            out.push('\u{c}');
                            self.pos += 1;
                        }
                        Some(b'\\') => {
                            out.push('\\');
                            self.pos += 1;
                        }
                        Some(b'"') => {
                            out.push('"');
                            self.pos += 1;
                        }
                        Some(b'\'') => {
                            out.push('\'');
                            self.pos += 1;
                        }
                        Some(b'/') => {
                            out.push('/');
                            self.pos += 1;
                        }
                        Some(b'u') => {
                            self.pos += 1;
                            let hex = self
                                .src
                                .get(self.pos..self.pos + 4)
                                .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
                                .ok_or_else(|| MongoError::InvalidLiteral {
                                    kind: "string",
                                    value: "\\u".to_string(),
                                    at: self.pos,
                                    reason: "expected 4 hex digits after \\u",
                                })?;
                            let code = u32::from_str_radix(hex, 16).unwrap_or(0xFFFD);
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                            self.pos += 4;
                        }
                        Some(other) => {
                            out.push(other as char);
                            self.pos += 1;
                        }
                        None => {
                            return Err(MongoError::UnexpectedEof {
                                at: self.pos,
                                expected: "an escape sequence",
                            })
                        }
                    }
                }
                Some(_) => {
                    let ch_len = utf8_len(self.bytes[self.pos]);
                    out.push_str(&self.src[self.pos..self.pos + ch_len]);
                    self.pos += ch_len;
                }
            }
        }
    }

    fn parse_object(&mut self) -> Result<Value, MongoError> {
        self.expect_byte(b'{', "'{'")?;
        let mut fields = Vec::new();
        self.skip_trivia();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Document(Arc::new(Document::from_fields(fields))));
        }
        loop {
            self.skip_trivia();
            let key = self.parse_object_key()?;
            self.skip_trivia();
            self.expect_byte(b':', "':' after an object key")?;
            let value = self.parse_value()?;
            fields.push((Arc::from(key.as_str()), value));
            self.skip_trivia();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_trivia();
                    if self.peek() == Some(b'}') {
                        break; // trailing comma
                    }
                }
                Some(b'}') => break,
                Some(b) if is_js_operator_byte(b) => return Err(MongoError::UnsupportedJs),
                _ => return Err(self.err_here("',' or '}'")),
            }
        }
        self.expect_byte(b'}', "'}' to close an object")?;
        Ok(Value::Document(Arc::new(Document::from_fields(fields))))
    }

    fn parse_object_key(&mut self) -> Result<String, MongoError> {
        match self.peek() {
            Some(b'"') | Some(b'\'') => self.parse_string(),
            Some(b) if is_ident_start(b) || b.is_ascii_digit() => {
                let start = self.pos;
                while self
                    .peek()
                    .is_some_and(|b| is_ident_continue(b) || b == b'-')
                {
                    self.pos += 1;
                }
                if self.pos == start {
                    Err(self.err_here("an object key"))
                } else {
                    Ok(self.src[start..self.pos].to_string())
                }
            }
            _ => Err(self.err_here("an object key")),
        }
    }

    fn parse_array(&mut self) -> Result<Value, MongoError> {
        self.expect_byte(b'[', "'['")?;
        let items = self.parse_value_list(b']')?;
        self.expect_byte(b']', "']' to close an array")?;
        Ok(Value::Array(Arc::from(items)))
    }
}

fn looks_like_number(s: &str) -> bool {
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-')
}

fn utf8_len(b: u8) -> usize {
    if b & 0x80 == 0 {
        1
    } else if b & 0xE0 == 0xC0 {
        2
    } else if b & 0xF0 == 0xE0 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datagrep_api::TzSpec;

    fn chain(src: &str) -> MongoStatement {
        match parse(src).unwrap() {
            ParsedMongo::Chain(c) => c,
            ParsedMongo::RawCommand(_) => panic!("expected a chain"),
        }
    }

    #[test]
    fn simple_find() {
        let c = chain(r#"db.users.find({name: "amy"})"#);
        assert_eq!(c.collection, "users");
        assert_eq!(c.method, "find");
        assert_eq!(c.args.len(), 1);
    }

    #[test]
    fn get_collection_for_special_names() {
        let c = chain(r#"db.getCollection("weird name").find({})"#);
        assert_eq!(c.collection, "weird name");
    }

    #[test]
    fn chained_modifiers() {
        let c = chain(r#"db.users.find({a:1}).limit(5).sort({b:-1})"#);
        assert_eq!(c.method, "find");
        assert_eq!(c.modifiers.len(), 2);
        assert_eq!(c.modifiers[0].0, "limit");
        assert_eq!(c.modifiers[0].1, vec![Value::I64(5)]);
        assert_eq!(c.modifiers[1].0, "sort");
    }

    #[test]
    fn json5_unquoted_keys_single_quotes_trailing_commas() {
        let c = chain("db.users.insertOne({name: 'amy', age: 30,})");
        let Value::Document(doc) = &c.args[0] else {
            panic!()
        };
        assert_eq!(doc.get("name"), Some(&Value::Str(Arc::from("amy"))));
        assert_eq!(doc.get("age"), Some(&Value::I64(30)));
    }

    #[test]
    fn nested_documents_and_arrays() {
        let c = chain(r#"db.users.find({address: {city: "sg", tags: ["a", "b"]}})"#);
        let Value::Document(doc) = &c.args[0] else {
            panic!()
        };
        let Some(Value::Document(addr)) = doc.get("address") else {
            panic!()
        };
        assert_eq!(addr.get("city"), Some(&Value::Str(Arc::from("sg"))));
        let Some(Value::Array(tags)) = addr.get("tags") else {
            panic!()
        };
        assert_eq!(tags.len(), 2);
    }

    #[test]
    fn object_id_constructor() {
        let c = chain(r#"db.users.find({_id: ObjectId("507f1f77bcf86cd799439011")})"#);
        let Value::Document(doc) = &c.args[0] else {
            panic!()
        };
        assert_eq!(
            doc.get("_id"),
            Some(&Value::Str(Arc::from("507f1f77bcf86cd799439011")))
        );
    }

    #[test]
    fn object_id_rejects_bad_hex() {
        let err = parse(r#"db.users.find({_id: ObjectId("not-hex")})"#).unwrap_err();
        assert!(matches!(
            err,
            MongoError::InvalidLiteral {
                kind: "ObjectId",
                ..
            }
        ));
    }

    #[test]
    fn iso_date_constructor() {
        let c = chain(r#"db.events.find({at: ISODate("2024-01-15T10:30:00Z")})"#);
        let Value::Document(doc) = &c.args[0] else {
            panic!()
        };
        match doc.get("at") {
            Some(Value::Timestamp { tz, .. }) => assert_eq!(*tz, TzSpec::Utc),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn number_long_and_decimal_constructors() {
        let c = chain(
            r#"db.a.insertOne({big: NumberLong("9007199254740993"), price: NumberDecimal("19.99")})"#,
        );
        let Value::Document(doc) = &c.args[0] else {
            panic!()
        };
        assert_eq!(doc.get("big"), Some(&Value::I64(9_007_199_254_740_993)));
        assert_eq!(doc.get("price"), Some(&Value::Decimal(Arc::from("19.99"))));
    }

    #[test]
    fn number_long_accepts_bare_number_form() {
        let c = chain("db.a.insertOne({n: NumberLong(42)})");
        let Value::Document(doc) = &c.args[0] else {
            panic!()
        };
        assert_eq!(doc.get("n"), Some(&Value::I64(42)));
    }

    #[test]
    fn raw_command_document() {
        match parse(r#"{ find: "users", filter: { active: true } }"#).unwrap() {
            ParsedMongo::RawCommand(Value::Document(doc)) => {
                assert_eq!(doc.get("find"), Some(&Value::Str(Arc::from("users"))));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn rejects_for_loop_with_exact_message() {
        let err = parse("for (let i = 0; i < 10; i++) { db.a.insertOne({i: i}) }").unwrap_err();
        assert_eq!(
            err.to_string(),
            "datagrep supports query expressions, not arbitrary JavaScript — use a raw command document for anything else"
        );
    }

    #[test]
    fn rejects_variables_and_arithmetic() {
        assert!(matches!(
            parse("db.a.insertOne({x: someVariable})"),
            Err(MongoError::UnsupportedJs)
        ));
        assert!(matches!(
            parse("db.a.insertOne({x: 1 + 1})"),
            Err(MongoError::UnsupportedJs)
        ));
        assert!(matches!(
            parse("let x = 1;"),
            Err(MongoError::UnsupportedJs)
        ));
        assert!(matches!(
            parse("db.a.find().limit(1 + 1)"),
            Err(MongoError::UnsupportedJs)
        ));
    }

    #[test]
    fn rejects_new_expression() {
        assert!(matches!(
            parse("db.a.insertOne({d: new Date()})"),
            Err(MongoError::UnsupportedJs)
        ));
    }
}
