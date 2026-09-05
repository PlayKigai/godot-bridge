use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
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
pub struct Number(NumberRepr);

#[derive(Clone, Debug, Eq, PartialEq)]
enum NumberRepr {
    Small { bytes: [u8; 24], len: u8 },
    Heap(String),
}

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
    fn from_bytes(value: &[u8]) -> Self {
        if value.len() <= 24 {
            let mut bytes = [0; 24];
            bytes[..value.len()].copy_from_slice(value);
            Self(NumberRepr::Small {
                bytes,
                len: value.len() as u8,
            })
        } else {
            Self(NumberRepr::Heap(
                String::from_utf8(value.to_vec()).expect("number syntax is ASCII"),
            ))
        }
    }

    fn from_integer(value: impl fmt::Display) -> Self {
        let mut bytes = [0; 24];
        let len;
        {
            let mut writer = NumberWriter {
                bytes: &mut bytes,
                len: 0,
            };
            write!(writer, "{value}").expect("integer fits in number buffer");
            len = writer.len;
        }
        Self(NumberRepr::Small {
            bytes,
            len: len as u8,
        })
    }

    fn as_bytes(&self) -> &[u8] {
        match &self.0 {
            NumberRepr::Small { bytes, len } => &bytes[..*len as usize],
            NumberRepr::Heap(value) => value.as_bytes(),
        }
    }

    fn as_i64(&self) -> Option<i64> {
        str::from_utf8(self.as_bytes()).ok()?.parse().ok()
    }

    fn as_u64(&self) -> Option<u64> {
        str::from_utf8(self.as_bytes()).ok()?.parse().ok()
    }
}

struct NumberWriter<'a> {
    bytes: &'a mut [u8; 24],
    len: usize,
}

impl fmt::Write for NumberWriter<'_> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let end = self.len.checked_add(value.len()).ok_or(fmt::Error)?;
        let destination = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        destination.copy_from_slice(value.as_bytes());
        self.len = end;
        Ok(())
    }
}

impl fmt::Display for Number {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(str::from_utf8(self.as_bytes()).map_err(|_| fmt::Error)?)
    }
}

impl Map {
    pub fn new() -> Self {
        Self(Vec::new())
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

impl From<&Value> for Value {
    fn from(value: &Value) -> Self {
        value.clone()
    }
}

macro_rules! integer_value {
    ($($type:ty),* $(,)?) => {
        $(
            impl From<$type> for Value {
                fn from(value: $type) -> Self {
                    Self::Number(Number::from_integer(value))
                }
            }
        )*
    };
}

integer_value!(i32, i64, u16, u32, u64);

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

#[derive(Clone, Copy)]
pub(crate) struct RawJson<'a>(&'a [u8]);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RequestKey {
    Number(i64),
    String(String),
    Lexical(String),
}

impl RawJson<'_> {
    pub(crate) fn as_i64(self) -> Option<i64> {
        self.is_number()
            .then(|| str::from_utf8(self.0).ok())??
            .parse()
            .ok()
    }

    fn is_number(self) -> bool {
        matches!(self.0.first(), Some(b'-' | b'0'..=b'9'))
    }

    fn lexical(self) -> String {
        String::from_utf8(self.0.to_vec()).expect("scanned JSON is UTF-8")
    }

    pub(crate) fn request_key(self) -> Option<RequestKey> {
        if self.0.first() == Some(&b'"') {
            if let Ok(Value::String(value)) = from_slice(self.0) {
                if value.len() > 256 {
                    crate::warn!("dropping request id longer than 256 bytes");
                    return None;
                }
                return Some(RequestKey::String(value));
            }
        }
        Some(
            self.as_i64()
                .map_or_else(|| RequestKey::Lexical(self.lexical()), RequestKey::Number),
        )
    }

    pub(crate) fn string_eq(self, expected: &str) -> bool {
        let Some(bytes) = self
            .0
            .strip_prefix(b"\"")
            .and_then(|bytes| bytes.strip_suffix(b"\""))
        else {
            return false;
        };
        let expected = expected.as_bytes();
        if !bytes.contains(&b'\\') {
            return bytes == expected;
        }
        matches!(from_slice(self.0), Ok(Value::String(value)) if value.as_bytes() == expected)
    }

    pub(crate) fn is_string(self) -> bool {
        self.0.first() == Some(&b'"')
    }
}

pub(crate) fn value_request_key(value: &Value) -> Option<RequestKey> {
    match value {
        Value::String(value) => {
            if value.len() > 256 {
                crate::warn!("dropping request id longer than 256 bytes");
                None
            } else {
                Some(RequestKey::String(value.clone()))
            }
        }
        Value::Number(number) => Some(number.as_i64().map_or_else(
            || RequestKey::Lexical(number.to_string()),
            RequestKey::Number,
        )),
        _ => Some(RequestKey::Lexical(to_string(value))),
    }
}

pub(crate) struct TopLevel<'a> {
    pub(crate) id: Option<RawJson<'a>>,
    pub(crate) method: Option<RawJson<'a>>,
    pub(crate) type_: Option<RawJson<'a>>,
    pub(crate) command: Option<RawJson<'a>>,
    pub(crate) event: Option<RawJson<'a>>,
    pub(crate) seq: Option<RawJson<'a>>,
    pub(crate) request_seq: Option<RawJson<'a>>,
}

pub(crate) fn scan_top_level(bytes: &[u8]) -> Result<TopLevel<'_>, Error> {
    let mut parser = Parser::new(bytes, false, false);
    parser.skip_space()?;
    if !parser.take(b'{') {
        return Err(Error::new(parser.index, "message is not an object"));
    }
    let mut fields = TopLevel {
        id: None,
        method: None,
        type_: None,
        command: None,
        event: None,
        seq: None,
        request_seq: None,
    };
    parser.skip_space()?;
    if parser.take(b'}') {
        parser.skip_space()?;
        return if parser.index == bytes.len() {
            Ok(fields)
        } else {
            Err(Error::new(parser.index, "trailing characters"))
        };
    }
    loop {
        let (key_start, key_end) = parser.parse_string_span()?;
        parser.skip_space()?;
        if !parser.take(b':') {
            return Err(Error::new(parser.index, "expected ':' after object key"));
        }
        parser.skip_space()?;
        let value_start = parser.index;
        parser.skip_value(1)?;
        let value = RawJson(&bytes[value_start..parser.index]);
        let key = RawJson(&bytes[key_start..key_end]);
        if key.string_eq("id") {
            fields.id = Some(value);
        } else if key.string_eq("method") {
            fields.method = Some(value);
        } else if key.string_eq("type") {
            fields.type_ = Some(value);
        } else if key.string_eq("command") {
            fields.command = Some(value);
        } else if key.string_eq("event") {
            fields.event = Some(value);
        } else if key.string_eq("seq") {
            fields.seq = Some(value);
        } else if key.string_eq("request_seq") {
            fields.request_seq = Some(value);
        }
        parser.skip_space()?;
        if parser.take(b'}') {
            parser.skip_space()?;
            if parser.index != bytes.len() {
                return Err(Error::new(parser.index, "trailing characters"));
            }
            return Ok(fields);
        }
        if !parser.take(b',') {
            return Err(Error::new(parser.index, "expected ',' or '}' in object"));
        }
        parser.skip_space()?;
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
    str::from_utf8(bytes).map_err(|error| Error::new(error.valid_up_to(), "input is not UTF-8"))?;
    Parser::new(bytes, false, true).parse()
}

pub fn from_str(text: &str) -> Result<Value, Error> {
    Parser::new(text.as_bytes(), false, true).parse()
}

pub fn from_str_relaxed(text: &str) -> Result<Value, Error> {
    Parser::new(text.as_bytes(), true, true).parse()
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
        Value::Number(number) => output.extend_from_slice(number.as_bytes()),
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

pub(crate) fn write_string(string: &str, output: &mut Vec<u8>) {
    output.push(b'"');
    let bytes = string.as_bytes();
    let mut run_start = 0;
    while run_start < bytes.len() {
        let index = run_start + find_special(&bytes[run_start..]);
        if index == bytes.len() {
            break;
        }
        let byte = bytes[index];
        output.extend_from_slice(&string.as_bytes()[run_start..index]);
        output.push(b'\\');
        match byte {
            b'"' => output.push(b'"'),
            b'\\' => output.push(b'\\'),
            0x08 => output.push(b'b'),
            0x0c => output.push(b'f'),
            b'\n' => output.push(b'n'),
            b'\r' => output.push(b'r'),
            b'\t' => output.push(b't'),
            byte => {
                let code = u32::from(byte);
                let hex = b"0123456789abcdef";
                output.push(b'u');
                output.push(hex[((code >> 12) & 0xf) as usize]);
                output.push(hex[((code >> 8) & 0xf) as usize]);
                output.push(hex[((code >> 4) & 0xf) as usize]);
                output.push(hex[(code & 0xf) as usize]);
            }
        }
        run_start = index + 1;
    }
    output.extend_from_slice(&string.as_bytes()[run_start..]);
    output.push(b'"');
}

fn find_special(bytes: &[u8]) -> usize {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    const QUOTES: u64 = 0x2222_2222_2222_2222;
    const BACKSLASHES: u64 = 0x5c5c_5c5c_5c5c_5c5c;
    const CONTROLS: u64 = 0x2020_2020_2020_2020;
    let mut index = 0;
    while let Some(chunk) = bytes.get(index..index + 8) {
        let word = u64::from_le_bytes(chunk.try_into().expect("eight-byte chunk"));
        let quote = word ^ QUOTES;
        let backslash = word ^ BACKSLASHES;
        let equal = (quote.wrapping_sub(ONES) & !quote & HIGHS)
            | (backslash.wrapping_sub(ONES) & !backslash & HIGHS);
        let control = word.wrapping_sub(CONTROLS) & !word & HIGHS;
        if equal | control != 0 {
            return index
                + bytes[index..index + 8]
                    .iter()
                    .position(|byte| *byte == b'"' || *byte == b'\\' || *byte < 0x20)
                    .expect("special byte exists in matching chunk");
        }
        index += 8;
    }
    index
        + bytes[index..]
            .iter()
            .position(|byte| *byte == b'"' || *byte == b'\\' || *byte < 0x20)
            .unwrap_or(bytes.len() - index)
}

struct Parser<'a> {
    input: &'a [u8],
    index: usize,
    relaxed: bool,
    validated: bool,
}

struct ObjectBuilder {
    object: Map,
    indices: Option<HashMap<String, usize>>,
}

impl ObjectBuilder {
    fn new() -> Self {
        Self {
            object: Map::new(),
            indices: None,
        }
    }

    fn insert(&mut self, key: String, value: Value) {
        if self.indices.is_none() && self.object.0.len() >= 16 {
            let mut indices = HashMap::with_capacity(self.object.0.len() + 1);
            for (index, (key, _)) in self.object.0.iter().enumerate() {
                indices.insert(key.clone(), index);
            }
            self.indices = Some(indices);
        }
        if let Some(indices) = &mut self.indices {
            if let Some(&index) = indices.get(&key) {
                self.object.0[index].1 = value;
            } else {
                let index = self.object.0.len();
                indices.insert(key.clone(), index);
                self.object.0.push((key, value));
            }
        } else {
            self.object.insert(key, value);
        }
    }

    fn finish(self) -> Map {
        self.object
    }
}

impl<'a> Parser<'a> {
    fn new(input: &'a [u8], relaxed: bool, validated: bool) -> Self {
        Self {
            input,
            index: 0,
            relaxed,
            validated,
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
            b'-' | b'0'..=b'9' => self.parse_number().map(Value::Number),
            _ => self.error("expected a value"),
        }
    }

    fn parse_string_span(&mut self) -> Result<(usize, usize), Error> {
        let start = self.index;
        if !self.take(b'"') {
            return self.error("object keys must be strings");
        }
        loop {
            let offset = find_special(&self.input[self.index..]);
            if offset == self.input.len() - self.index {
                return self.error("unterminated string");
            }
            self.index += offset;
            let byte = self.input[self.index];
            match byte {
                b'"' => {
                    self.index += 1;
                    if !self.validated
                        && str::from_utf8(&self.input[start + 1..self.index - 1]).is_err()
                    {
                        return self.error("string is not UTF-8");
                    }
                    return Ok((start, self.index));
                }
                b'\\' => {
                    self.index += 1;
                    self.parse_escape(None)?;
                }
                _ => return self.error("control character in string"),
            }
        }
    }

    fn skip_value(&mut self, depth: usize) -> Result<(), Error> {
        if depth >= MAX_DEPTH {
            return self.error("nesting exceeds 128 levels");
        }
        let Some(byte) = self.input.get(self.index).copied() else {
            return self.error("expected a value");
        };
        match byte {
            b'"' => {
                self.parse_string_span()?;
            }
            b'[' => {
                self.index += 1;
                self.skip_space()?;
                if self.take(b']') {
                    return Ok(());
                }
                loop {
                    self.skip_value(depth + 1)?;
                    self.skip_space()?;
                    if self.take(b']') {
                        return Ok(());
                    }
                    if !self.take(b',') {
                        return self.error("expected ',' or ']' in array");
                    }
                    self.skip_space()?;
                }
            }
            b'{' => {
                self.index += 1;
                self.skip_space()?;
                if self.take(b'}') {
                    return Ok(());
                }
                loop {
                    self.parse_string_span()?;
                    self.skip_space()?;
                    if !self.take(b':') {
                        return self.error("expected ':' after object key");
                    }
                    self.skip_space()?;
                    self.skip_value(depth + 1)?;
                    self.skip_space()?;
                    if self.take(b'}') {
                        return Ok(());
                    }
                    if !self.take(b',') {
                        return self.error("expected ',' or '}' in object");
                    }
                    self.skip_space()?;
                }
            }
            b'n' if self.take_keyword(b"null") => {}
            b't' if self.take_keyword(b"true") => {}
            b'f' if self.take_keyword(b"false") => {}
            b'-' | b'0'..=b'9' => {
                self.scan_number()?;
            }
            _ => return self.error("expected a value"),
        }
        Ok(())
    }

    fn take_keyword(&mut self, keyword: &[u8]) -> bool {
        if self.input.get(self.index..self.index + keyword.len()) == Some(keyword) {
            self.index += keyword.len();
            true
        } else {
            false
        }
    }

    fn scan_number(&mut self) -> Result<(usize, usize), Error> {
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
            let start = self.index;
            while self.input.get(self.index).is_some_and(u8::is_ascii_digit) {
                self.index += 1;
            }
            if self.index == start {
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
            let start = self.index;
            while self.input.get(self.index).is_some_and(u8::is_ascii_digit) {
                self.index += 1;
            }
            if self.index == start {
                return self.error("exponent has no digits");
            }
        }
        Ok((start, self.index))
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
        let mut object = ObjectBuilder::new();
        if self.take(b'}') {
            return Ok(Value::Object(object.finish()));
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
                return Ok(Value::Object(object.finish()));
            }
            if !self.take(b',') {
                return self.error("expected ',' or '}' in object");
            }
            self.skip_space()?;
            if self.take(b'}') {
                if self.relaxed {
                    return Ok(Value::Object(object.finish()));
                }
                return self.error("trailing comma is not allowed");
            }
        }
    }

    fn parse_number(&mut self) -> Result<Number, Error> {
        let (start, end) = self.scan_number()?;
        Ok(Number::from_bytes(&self.input[start..end]))
    }

    fn parse_string(&mut self) -> Result<String, Error> {
        self.index += 1;
        let mut run_start = self.index;
        let mut output = None;
        loop {
            let offset = find_special(&self.input[self.index..]);
            if offset == self.input.len() - self.index {
                return self.error("unterminated string");
            }
            self.index += offset;
            let byte = self.input[self.index];
            match byte {
                b'"' => {
                    let end = self.index;
                    if let Some(mut output) = output {
                        append_utf8(
                            &mut output,
                            &self.input[run_start..end],
                            self.index,
                            self.validated,
                        )?;
                        self.index += 1;
                        return Ok(output);
                    }
                    let value = if self.validated {
                        unsafe { str::from_utf8_unchecked(&self.input[run_start..end]) }.to_owned()
                    } else {
                        str::from_utf8(&self.input[run_start..end])
                            .map_err(|_| Error::new(self.index, "string is not UTF-8"))?
                            .to_owned()
                    };
                    self.index += 1;
                    return Ok(value);
                }
                b'\\' => {
                    let current = output.get_or_insert_with(String::new);
                    append_utf8(
                        current,
                        &self.input[run_start..self.index],
                        self.index,
                        self.validated,
                    )?;
                    self.index += 1;
                    self.parse_escape(Some(current))?;
                    run_start = self.index;
                }
                _ => return self.error("control character in string"),
            }
        }
    }

    fn parse_escape(&mut self, output: Option<&mut String>) -> Result<(), Error> {
        let Some(escape) = self.input.get(self.index).copied() else {
            return self.error("unterminated escape");
        };
        self.index += 1;
        let character = match escape {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{08}',
            b'f' => '\u{0c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
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
                char::from_u32(code)
                    .ok_or_else(|| Error::new(self.index, "invalid Unicode escape"))?
            }
            _ => return self.error("invalid escape sequence"),
        };
        if let Some(output) = output {
            output.push(character);
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
            if !self.relaxed {
                return Ok(());
            }
            if self.input.get(self.index..self.index + 2) == Some(b"//") {
                self.index += 2;
                while self
                    .input
                    .get(self.index)
                    .is_some_and(|byte| *byte != b'\n')
                {
                    self.index += 1;
                }
                continue;
            }
            if self.input.get(self.index..self.index + 2) == Some(b"/*") {
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
            return Ok(());
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

fn append_utf8(
    output: &mut String,
    bytes: &[u8],
    offset: usize,
    validated: bool,
) -> Result<(), Error> {
    if validated {
        output.push_str(unsafe { str::from_utf8_unchecked(bytes) });
    } else {
        let value = str::from_utf8(bytes).map_err(|_| Error::new(offset, "string is not UTF-8"))?;
        output.push_str(value);
    }
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
    fn duplicate_keys_are_replaced_and_removed_once() {
        let mut value = from_str(r#"{"key":1,"key":2}"#).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.iter().count(), 1);
        assert_eq!(value["key"], crate::json!(2));
        assert!(value.as_object_mut().unwrap().remove("key").is_some());
        assert!(!value.as_object().unwrap().contains_key("key"));
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

    #[test]
    fn top_level_scan_ignores_nested_keys() {
        let fields =
            scan_top_level(br#"{"params":{"method":"nested"},"method":"initialized","id":12}"#)
                .unwrap();
        assert!(fields.method.unwrap().string_eq("initialized"));
        assert_eq!(fields.id.unwrap().as_i64(), Some(12));
    }

    #[test]
    fn top_level_scan_matches_escaped_and_duplicate_keys() {
        let fields = scan_top_level(
            br#"{"met\u0068od":"ignored","method":"init\u0069alized","id":1,"id":2}"#,
        )
        .unwrap();
        assert!(fields.method.unwrap().string_eq("initialized"));
        assert_eq!(fields.id.unwrap().as_i64(), Some(2));
    }

    #[test]
    fn top_level_scan_rejects_invalid_nested_values() {
        assert!(scan_top_level(br#"{"params":{"x":}}"#).is_err());
        assert!(scan_top_level(br#"[1]"#).is_err());
    }

    #[test]
    fn request_keys_prefer_integer_ids() {
        assert_eq!(
            scan_top_level(br#"{"id":1}"#)
                .unwrap()
                .id
                .unwrap()
                .request_key()
                .unwrap(),
            RequestKey::Number(1)
        );
        assert_eq!(
            scan_top_level(br#"{"id":"1"}"#)
                .unwrap()
                .id
                .unwrap()
                .request_key()
                .unwrap(),
            RequestKey::String("1".to_owned())
        );
        assert_eq!(
            scan_top_level(br#"{"id":1.0}"#)
                .unwrap()
                .id
                .unwrap()
                .request_key()
                .unwrap(),
            RequestKey::Lexical("1.0".to_owned())
        );
        assert_eq!(
            scan_top_level(br#"{"id":18446744073709551615}"#)
                .unwrap()
                .id
                .unwrap()
                .request_key()
                .unwrap(),
            RequestKey::Lexical("18446744073709551615".to_owned())
        );
    }

    #[test]
    fn request_keys_decode_string_escapes() {
        let wire = scan_top_level(br#"{"id":"a\/b"}"#)
            .unwrap()
            .id
            .unwrap()
            .request_key()
            .unwrap();
        let parsed = value_request_key(&from_str(r#""a/b""#).unwrap()).unwrap();
        assert_eq!(wire, parsed);
    }

    #[test]
    fn request_keys_keep_string_and_number_lexemes_distinct() {
        let number = scan_top_level(br#"{"id":1.0}"#)
            .unwrap()
            .id
            .unwrap()
            .request_key()
            .unwrap();
        let string = scan_top_level(br#"{"id":"1.0"}"#)
            .unwrap()
            .id
            .unwrap()
            .request_key()
            .unwrap();
        assert_ne!(number, string);
        assert_eq!(
            value_request_key(&from_str(r#"1.0"#).unwrap()).unwrap(),
            number
        );
        assert_eq!(
            value_request_key(&from_str(r#""1.0""#).unwrap()).unwrap(),
            string
        );
    }

    #[test]
    fn request_keys_reject_oversized_string_ids() {
        let value = Value::String("x".repeat(257));
        assert!(value_request_key(&value).is_none());
        let wire = format!(r#"{{"id":"{}"}}"#, "x".repeat(257));
        assert!(scan_top_level(wire.as_bytes())
            .unwrap()
            .id
            .unwrap()
            .request_key()
            .is_none());
    }
}
