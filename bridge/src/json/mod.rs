use std::borrow::Cow;
use std::fmt;
use std::ops::{Index, IndexMut};
use std::str;

const MAX_DEPTH: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Value>),
    Object(Map),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Number(String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Map(Vec<(String, Value)>);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    offset: usize,
    message: String,
}

impl Error {
    fn new(offset: usize, message: impl Into<String>) -> Self {
        Self {
            offset,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "JSON error at byte {}: {}",
            self.offset, self.message
        )
    }
}

impl std::error::Error for Error {}

impl Number {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    fn from_f64(value: f64) -> Self {
        assert!(value.is_finite(), "JSON numbers must be finite");
        Self(value.to_string())
    }

    fn as_i64(&self) -> Option<i64> {
        self.0.parse().ok()
    }

    fn as_u64(&self) -> Option<u64> {
        self.0.parse().ok()
    }

    fn as_f64(&self) -> Option<f64> {
        self.0.parse().ok()
    }
}

impl fmt::Display for Number {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Map {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0
            .iter()
            .find_map(|(name, value)| (name == key).then_some(value))
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.0
            .iter_mut()
            .find_map(|(name, value)| (name == key).then_some(value))
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn insert(&mut self, key: String, value: Value) -> Option<Value> {
        if let Some((_, current)) = self.0.iter_mut().find(|(name, _)| name == &key) {
            return Some(std::mem::replace(current, value));
        }
        self.0.push((key, value));
        None
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let index = self.0.iter().position(|(name, _)| name == key)?;
        Some(self.0.remove(index).1)
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.iter().map(|(key, _)| key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter().map(|(key, value)| (key, value))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&String, &mut Value)> {
        self.0.iter_mut().map(|(key, value)| (&*key, value))
    }

    pub fn extend<I>(&mut self, entries: I)
    where
        I: IntoIterator<Item = (String, Value)>,
    {
        for (key, value) in entries {
            self.insert(key, value);
        }
    }
}

impl Default for Map {
    fn default() -> Self {
        Self::new()
    }
}

impl IntoIterator for Map {
    type Item = (String, Value);
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a Map {
    type Item = (&'a String, &'a Value);
    type IntoIter = std::iter::Map<
        std::slice::Iter<'a, (String, Value)>,
        fn(&(String, Value)) -> (&String, &Value),
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter().map(|(key, value)| (key, value))
    }
}

impl FromIterator<(String, Value)> for Map {
    fn from_iter<T: IntoIterator<Item = (String, Value)>>(entries: T) -> Self {
        let mut map = Self::new();
        map.extend(entries);
        map
    }
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Self::Object(_))
    }

    pub fn is_array(&self) -> bool {
        matches!(self, Self::Array(_))
    }

    pub fn is_number(&self) -> bool {
        matches!(self, Self::Number(_))
    }

    pub fn as_object(&self) -> Option<&Map> {
        match self {
            Self::Object(object) => Some(object),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut Map> {
        match self {
            Self::Object(object) => Some(object),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Self::Array(array) => Some(array),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        match self {
            Self::Array(array) => Some(array),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(string) => Some(string),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(number) => number.as_i64(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(number) => number.as_u64(),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(number) => number.as_f64(),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object()?.get(key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.as_object_mut()?.get_mut(key)
    }
}

impl Index<&str> for Value {
    type Output = Value;

    fn index(&self, key: &str) -> &Self::Output {
        static NULL: Value = Value::Null;
        self.get(key).unwrap_or(&NULL)
    }
}

impl IndexMut<&str> for Value {
    fn index_mut(&mut self, key: &str) -> &mut Self::Output {
        if !self.is_object() {
            *self = Self::Object(Map::new());
        }
        let object = self.as_object_mut().expect("value was made an object");
        if object.get(key).is_none() {
            object.insert(key.to_owned(), Value::Null);
        }
        object.get_mut(key).expect("value was inserted")
    }
}

impl Index<usize> for Value {
    type Output = Value;

    fn index(&self, index: usize) -> &Self::Output {
        static NULL: Value = Value::Null;
        match self {
            Self::Array(array) => array.get(index).unwrap_or(&NULL),
            _ => &NULL,
        }
    }
}

impl IndexMut<usize> for Value {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        match self {
            Self::Array(array) => array
                .get_mut(index)
                .expect("array index is outside the array"),
            _ => panic!("cannot index a non-array with a number"),
        }
    }
}

impl From<Number> for Value {
    fn from(value: Number) -> Self {
        Self::Number(value)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<&String> for Value {
    fn from(value: &String) -> Self {
        Self::String(value.clone())
    }
}

impl<'a> From<Cow<'a, str>> for Value {
    fn from(value: Cow<'a, str>) -> Self {
        Self::String(value.into_owned())
    }
}

impl From<&Value> for Value {
    fn from(value: &Value) -> Self {
        value.clone()
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Number(Number::from_f64(value))
    }
}

impl From<f32> for Value {
    fn from(value: f32) -> Self {
        Self::Number(Number::from_f64(f64::from(value)))
    }
}

macro_rules! integer_value {
    ($($type:ty),* $(,)?) => {
        $(
            impl From<$type> for Value {
                fn from(value: $type) -> Self {
                    Self::Number(Number::new(value.to_string()))
                }
            }
        )*
    };
}

integer_value!(i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize);

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(values: Vec<T>) -> Self {
        Self::Array(values.into_iter().map(Into::into).collect())
    }
}

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = to_vec(self);
        let string = str::from_utf8(&bytes).map_err(|_| fmt::Error)?;
        formatter.write_str(string)
    }
}

impl PartialEq<&str> for Value {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == Some(*other)
    }
}

macro_rules! value_number_eq {
    ($($type:ty),* $(,)?) => {
        $(
            impl PartialEq<$type> for Value {
                fn eq(&self, other: &$type) -> bool {
                    self.as_i64() == i64::try_from(*other).ok()
                        || self.as_u64() == u64::try_from(*other).ok()
                }
            }
        )*
    };
}

value_number_eq!(i32, i64, u32, u64, usize);

pub fn from_slice(bytes: &[u8]) -> Result<Value, Error> {
    let text = str::from_utf8(bytes)
        .map_err(|error| Error::new(error.valid_up_to(), "input is not UTF-8"))?;
    from_str(text)
}

pub fn from_str(text: &str) -> Result<Value, Error> {
    Parser::new(text.as_bytes(), false).parse()
}

pub fn from_slice_relaxed(bytes: &[u8]) -> Result<Value, Error> {
    let text = str::from_utf8(bytes)
        .map_err(|error| Error::new(error.valid_up_to(), "input is not UTF-8"))?;
    from_str_relaxed(text)
}

pub fn from_str_relaxed(text: &str) -> Result<Value, Error> {
    Parser::new(text.as_bytes(), true).parse()
}

pub fn to_vec(value: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_value(value, &mut bytes);
    bytes
}

pub fn to_string(value: &Value) -> String {
    String::from_utf8(to_vec(value)).expect("JSON writer emits UTF-8")
}

fn write_value(value: &Value, output: &mut Vec<u8>) {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(true) => output.extend_from_slice(b"true"),
        Value::Bool(false) => output.extend_from_slice(b"false"),
        Value::Number(number) => output.extend_from_slice(number.0.as_bytes()),
        Value::String(string) => write_string(string, output),
        Value::Array(array) => {
            output.push(b'[');
            for (index, value) in array.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_value(value, output);
            }
            output.push(b']');
        }
        Value::Object(object) => {
            output.push(b'{');
            for (index, (key, value)) in object.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_string(key, output);
                output.push(b':');
                write_value(value, output);
            }
            output.push(b'}');
        }
    }
}

fn write_string(string: &str, output: &mut Vec<u8>) {
    output.push(b'"');
    let mut run_start = 0;
    for (index, character) in string.char_indices() {
        let escape = match character {
            '"' => Some(b'"'),
            '\\' => Some(b'\\'),
            '\u{08}' => Some(b'b'),
            '\u{0c}' => Some(b'f'),
            '\n' => Some(b'n'),
            '\r' => Some(b'r'),
            '\t' => Some(b't'),
            character if character <= '\u{1f}' => Some(0),
            _ => None,
        };
        let Some(escape) = escape else {
            continue;
        };
        output.extend_from_slice(&string.as_bytes()[run_start..index]);
        output.push(b'\\');
        if escape == 0 {
            let code = character as u32;
            let hex = b"0123456789abcdef";
            output.push(b'u');
            output.push(hex[((code >> 12) & 0xf) as usize]);
            output.push(hex[((code >> 8) & 0xf) as usize]);
            output.push(hex[((code >> 4) & 0xf) as usize]);
            output.push(hex[(code & 0xf) as usize]);
        } else {
            output.push(escape);
        }
        run_start = index + character.len_utf8();
    }
    output.extend_from_slice(&string.as_bytes()[run_start..]);
    output.push(b'"');
}

struct Parser<'a> {
    input: &'a [u8],
    index: usize,
    relaxed: bool,
}

impl<'a> Parser<'a> {
    fn new(input: &'a [u8], relaxed: bool) -> Self {
        Self {
            input,
            index: 0,
            relaxed,
        }
    }

    fn parse(mut self) -> Result<Value, Error> {
        self.skip_space()?;
        let value = self.parse_value(0)?;
        self.skip_space()?;
        if self.index != self.input.len() {
            return self.error("trailing characters");
        }
        Ok(value)
    }

    fn parse_value(&mut self, depth: usize) -> Result<Value, Error> {
        self.skip_space()?;
        let Some(byte) = self.input.get(self.index).copied() else {
            return self.error("expected a value");
        };
        match byte {
            b'n' => self.keyword(b"null", Value::Null),
            b't' => self.keyword(b"true", Value::Bool(true)),
            b'f' => self.keyword(b"false", Value::Bool(false)),
            b'"' => self.parse_string().map(Value::String),
            b'[' => self.parse_array(depth),
            b'{' => self.parse_object(depth),
            b'-' | b'0'..=b'9' => self
                .parse_number()
                .map(|number| Value::Number(Number::new(number))),
            _ => self.error("expected a value"),
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<Value, Error> {
        if depth >= MAX_DEPTH {
            return self.error("nesting exceeds 128 levels");
        }
        self.index += 1;
        self.skip_space()?;
        let mut values = Vec::new();
        if self.take(b']') {
            return Ok(Value::Array(values));
        }
        loop {
            values.push(self.parse_value(depth + 1)?);
            self.skip_space()?;
            if self.take(b']') {
                return Ok(Value::Array(values));
            }
            if !self.take(b',') {
                return self.error("expected ',' or ']' in array");
            }
            self.skip_space()?;
            if self.take(b']') {
                if self.relaxed {
                    return Ok(Value::Array(values));
                }
                return self.error("trailing comma is not allowed");
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<Value, Error> {
        if depth >= MAX_DEPTH {
            return self.error("nesting exceeds 128 levels");
        }
        self.index += 1;
        self.skip_space()?;
        let mut object = Map::new();
        if self.take(b'}') {
            return Ok(Value::Object(object));
        }
        loop {
            if self.input.get(self.index) != Some(&b'"') {
                return self.error("object keys must be strings");
            }
            let key = self.parse_string()?;
            self.skip_space()?;
            if !self.take(b':') {
                return self.error("expected ':' after object key");
            }
            let value = self.parse_value(depth + 1)?;
            object.insert(key, value);
            self.skip_space()?;
            if self.take(b'}') {
                return Ok(Value::Object(object));
            }
            if !self.take(b',') {
                return self.error("expected ',' or '}' in object");
            }
            self.skip_space()?;
            if self.take(b'}') {
                if self.relaxed {
                    return Ok(Value::Object(object));
                }
                return self.error("trailing comma is not allowed");
            }
        }
    }

    fn parse_number(&mut self) -> Result<String, Error> {
        let start = self.index;
        self.take(b'-');
        match self.input.get(self.index).copied() {
            Some(b'0') => {
                self.index += 1;
                if self.input.get(self.index).is_some_and(u8::is_ascii_digit) {
                    return self.error("leading zero in number");
                }
            }
            Some(b'1'..=b'9') => {
                self.index += 1;
                while self.input.get(self.index).is_some_and(u8::is_ascii_digit) {
                    self.index += 1;
                }
            }
            _ => return self.error("invalid number"),
        }
        if self.take(b'.') {
            let fraction = self.index;
            while self.input.get(self.index).is_some_and(u8::is_ascii_digit) {
                self.index += 1;
            }
            if self.index == fraction {
                return self.error("fraction has no digits");
            }
        }
        if self
            .input
            .get(self.index)
            .is_some_and(|byte| *byte == b'e' || *byte == b'E')
        {
            self.index += 1;
            if self
                .input
                .get(self.index)
                .is_some_and(|byte| *byte == b'+' || *byte == b'-')
            {
                self.index += 1;
            }
            let exponent = self.index;
            while self.input.get(self.index).is_some_and(u8::is_ascii_digit) {
                self.index += 1;
            }
            if self.index == exponent {
                return self.error("exponent has no digits");
            }
        }
        Ok(str::from_utf8(&self.input[start..self.index])
            .expect("number syntax is ASCII")
            .to_owned())
    }

    fn parse_string(&mut self) -> Result<String, Error> {
        self.index += 1;
        let mut run_start = self.index;
        let mut output = None;
        loop {
            let Some(byte) = self.input.get(self.index).copied() else {
                return self.error("unterminated string");
            };
            match byte {
                b'"' => {
                    let end = self.index;
                    if let Some(mut output) = output {
                        append_utf8(&mut output, &self.input[run_start..end], self.index)?;
                        self.index += 1;
                        return Ok(output);
                    }
                    let value = str::from_utf8(&self.input[run_start..end])
                        .map_err(|_| Error::new(self.index, "string is not UTF-8"))?
                        .to_owned();
                    self.index += 1;
                    return Ok(value);
                }
                b'\\' => {
                    let current = output.get_or_insert_with(String::new);
                    append_utf8(current, &self.input[run_start..self.index], self.index)?;
                    self.index += 1;
                    self.parse_escape(current)?;
                    run_start = self.index;
                }
                byte if byte < 0x20 => return self.error("control character in string"),
                _ => self.index += 1,
            }
        }
    }

    fn parse_escape(&mut self, output: &mut String) -> Result<(), Error> {
        let Some(escape) = self.input.get(self.index).copied() else {
            return self.error("unterminated escape");
        };
        self.index += 1;
        match escape {
            b'"' => output.push('"'),
            b'\\' => output.push('\\'),
            b'/' => output.push('/'),
            b'b' => output.push('\u{08}'),
            b'f' => output.push('\u{0c}'),
            b'n' => output.push('\n'),
            b'r' => output.push('\r'),
            b't' => output.push('\t'),
            b'u' => {
                let high = self.parse_hex()?;
                let code = match high {
                    0xd800..=0xdbff => {
                        if self.input.get(self.index..self.index + 2) != Some(b"\\u") {
                            return self.error("high surrogate is not followed by a low surrogate");
                        }
                        self.index += 2;
                        let low = self.parse_hex()?;
                        if !(0xdc00..=0xdfff).contains(&low) {
                            return self.error("high surrogate is not followed by a low surrogate");
                        }
                        0x10000 + (u32::from(high - 0xd800) << 10) + u32::from(low - 0xdc00)
                    }
                    0xdc00..=0xdfff => return self.error("unexpected low surrogate"),
                    value => u32::from(value),
                };
                let character = char::from_u32(code)
                    .ok_or_else(|| Error::new(self.index, "invalid Unicode escape"))?;
                output.push(character);
            }
            _ => return self.error("invalid escape sequence"),
        }
        Ok(())
    }

    fn parse_hex(&mut self) -> Result<u16, Error> {
        let start = self.index;
        let Some(bytes) = self.input.get(start..start + 4) else {
            return self.error("Unicode escape has fewer than four digits");
        };
        let mut value = 0u16;
        for byte in bytes {
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return self.error("invalid Unicode escape"),
            };
            value = value * 16 + u16::from(digit);
        }
        self.index += 4;
        Ok(value)
    }

    fn keyword(&mut self, keyword: &[u8], value: Value) -> Result<Value, Error> {
        if self.input.get(self.index..self.index + keyword.len()) == Some(keyword) {
            self.index += keyword.len();
            Ok(value)
        } else {
            self.error("invalid literal")
        }
    }

    fn skip_space(&mut self) -> Result<(), Error> {
        loop {
            while self
                .input
                .get(self.index)
                .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
            {
                self.index += 1;
            }
            if !self.relaxed || self.input.get(self.index..self.index + 2) != Some(b"//") {
                if !self.relaxed || self.input.get(self.index..self.index + 2) != Some(b"/*") {
                    return Ok(());
                }
                self.index += 2;
                let Some(rest) = self.input.get(self.index..) else {
                    return self.error("unterminated comment");
                };
                let Some(end) = rest.windows(2).position(|pair| pair == b"*/") else {
                    return self.error("unterminated comment");
                };
                self.index += end + 2;
                continue;
            }
            self.index += 2;
            while self
                .input
                .get(self.index)
                .is_some_and(|byte| *byte != b'\n')
            {
                self.index += 1;
            }
        }
    }

    fn take(&mut self, byte: u8) -> bool {
        if self.input.get(self.index) == Some(&byte) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn error<T>(&self, message: impl Into<String>) -> Result<T, Error> {
        Err(Error::new(self.index, message))
    }
}

fn append_utf8(output: &mut String, bytes: &[u8], offset: usize) -> Result<(), Error> {
    let value = str::from_utf8(bytes).map_err(|_| Error::new(offset, "string is not UTF-8"))?;
    output.push_str(value);
    Ok(())
}

#[macro_export]
macro_rules! json {
    (null) => {
        $crate::json::Value::Null
    };
    ([$($values:tt),* $(,)?]) => {
        $crate::json::Value::Array(vec![$($crate::json!($values)),*])
    };
    ({$($key:literal : $value:tt),* $(,)?}) => {
        $crate::json::Value::Object(
            [$(($key.to_owned(), $crate::json!($value))),*]
                .into_iter()
                .collect(),
        )
    };
    ($value:expr) => {
        $crate::json::Value::from($value)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_and_invalid_documents_match_serde_json() {
        let valid = [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"workspaceFolders":[{"uri":"file:///tmp/project","name":"fixture"}]}}"#,
            r#"{"seq":1,"type":"request","command":"initialize","arguments":{"adapterID":"godot","linesStartAt1":true}}"#,
            r#"{"a":{"b":{"c":[1,2,3]}},"empty":{},"values":[]}"#,
            r#"{"escaped":"\"\\\/\b\f\n\r\t","unicode":"\u0061\ud83d\ude00"}"#,
            r#"{"big":18446744073709551615,"negative":-9223372036854775808,"exponent":-1.25e+20}"#,
            " [ true, false, null, -0, 0.5, 6E-2 ] ",
            r#"{"duplicate":1,"duplicate":2}"#,
        ];
        let invalid = [
            r#"{"#,
            r#"[1,]"#,
            r#"{"a":1,}"#,
            r#"{"a":01}"#,
            r#"{"a":.5}"#,
            r#"{"a":1e}"#,
            r#"{"a":"\uD800"}"#,
            r#"{"a":"\uDC00"}"#,
            r#"{"a": "line
break"}"#,
            r#"{"a": true} trailing"#,
        ];
        for document in valid {
            assert!(
                serde_json::from_str::<serde_json::Value>(document).is_ok(),
                "{document}"
            );
            let value = from_str(document).unwrap_or_else(|error| panic!("{document}: {error}"));
            let actual = serde_json::from_slice::<serde_json::Value>(&to_vec(&value)).unwrap();
            let expected = serde_json::from_str::<serde_json::Value>(document).unwrap();
            assert_eq!(actual, expected, "{document}");
        }
        for document in invalid {
            assert!(
                serde_json::from_str::<serde_json::Value>(document).is_err(),
                "{document}"
            );
            assert!(from_str(document).is_err(), "{document}");
        }
    }

    #[test]
    fn relaxed_mode_accepts_comments_and_trailing_commas_only() {
        let value = from_str_relaxed(
            r#"{
                // line
                "a": [1, /* block */ 2,],
            }"#,
        )
        .unwrap();
        assert_eq!(value["a"][1], crate::json!(2));
        assert!(from_str_relaxed("{unquoted: 1}").is_err());
        assert!(from_str_relaxed("{'single': 1}").is_err());
    }

    #[test]
    fn numbers_keep_their_lexical_form() {
        let value = from_str(r#"{"id":-12.3400e+05}"#).unwrap();
        assert_eq!(value["id"].to_string(), "-12.3400e+05");
    }

    #[test]
    fn indexing_creates_objects() {
        let mut value = Value::Null;
        value["params"]["id"] = 4.into();
        assert_eq!(value.to_string(), r#"{"params":{"id":4}}"#);
    }

    #[test]
    fn depth_is_bounded() {
        let accepted = format!("{}0{}", "[".repeat(128), "]".repeat(128));
        let rejected = format!("{}0{}", "[".repeat(129), "]".repeat(129));
        assert!(from_str(&accepted).is_ok());
        assert!(from_str(&rejected).is_err());
    }
}
