//! Runtime values: dual string/number scalars and associative arrays.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Awk scalar: string and/or number (strnum for input fields).
#[derive(Debug, Clone)]
pub struct Scalar {
    pub s: String,
    pub n: f64,
    /// true if value came from input and looks numeric (strnum)
    pub strnum: bool,
    /// true if never assigned (uninitialized)
    pub uninit: bool,
    /// true if the value is a number, string part derived via CONVFMT
    pub numval: bool,
}

impl Scalar {
    pub fn empty() -> Self {
        Scalar {
            s: String::new(),
            n: 0.0,
            strnum: false,
            uninit: true,
            numval: false,
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: impl Into<String>) -> Self {
        let s = s.into();
        let n = parse_num(&s);
        Scalar {
            s,
            n,
            strnum: false,
            uninit: false,
            numval: false,
        }
    }

    pub fn from_num(n: f64) -> Self {
        Scalar {
            s: num_to_str(n),
            n,
            strnum: false,
            uninit: false,
            numval: true,
        }
    }

    pub fn from_input(s: impl Into<String>) -> Self {
        let s = s.into();
        let n = parse_num(&s);
        // input values keep leading/trailing blanks and still count as strnum
        let looks = num_strict(s.trim());
        Scalar {
            s,
            n,
            strnum: looks,
            uninit: false,
            numval: false,
        }
    }

    /// Numeric value for arithmetic (uninit -> 0).
    pub fn num(&self) -> f64 {
        if self.uninit { 0.0 } else { self.n }
    }

    /// String value for concatenation/print (uninit -> "").
    pub fn as_str(&self) -> &str {
        if self.uninit { "" } else { &self.s }
    }

    /// Comparison class per POSIX awk.
    pub fn is_str(&self) -> bool {
        self.uninit || !self.strnum && !self.looks_numeric()
    }

    fn looks_numeric(&self) -> bool {
        num_strict(&self.s)
    }

    pub fn set_num(&mut self, n: f64) {
        self.n = n;
        self.s = num_to_str(n);
        self.strnum = false;
        self.uninit = false;
        self.numval = true;
    }

    pub fn set_str(&mut self, s: impl Into<String>) {
        self.s = s.into();
        self.n = parse_num(&self.s);
        self.strnum = false;
        self.uninit = false;
        self.numval = false;
    }

    pub fn set_input(&mut self, s: impl Into<String>) {
        let s = s.into();
        self.n = parse_num(&s);
        self.strnum = !s.is_empty() && s.trim() == s && num_strict(&s);
        self.s = s;
        self.uninit = false;
        self.numval = false;
    }
}

/// Array shared by reference.
pub type ArrayRef = Rc<RefCell<HashMap<String, Value>>>;

#[derive(Debug, Clone)]
pub enum Value {
    Scalar(Scalar),
    Array(ArrayRef),
}

impl Value {
    pub fn uninit() -> Self {
        Value::Scalar(Scalar::empty())
    }

    pub fn num(n: f64) -> Self {
        Value::Scalar(Scalar::from_num(n))
    }

    pub fn str(s: impl Into<String>) -> Self {
        Value::Scalar(Scalar::from_str(s))
    }

    pub fn as_scalar(&self) -> &Scalar {
        match self {
            Value::Scalar(s) => s,
            Value::Array(_) => panic!("used array as scalar"),
        }
    }

    pub fn as_scalar_mut(&mut self) -> &mut Scalar {
        match self {
            Value::Scalar(s) => s,
            Value::Array(_) => panic!("used array as scalar"),
        }
    }

    pub fn is_array(&self) -> bool {
        matches!(self, Value::Array(_))
    }

    pub fn to_num(&self) -> f64 {
        self.as_scalar().num()
    }

    pub fn to_str(&self) -> String {
        self.as_scalar().as_str().to_string()
    }
}

/// Parse awk numeric string: longest valid numeric prefix (POSIX).
pub(crate) fn parse_num(s: &str) -> f64 {
    let t = s.trim_start();
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let mut digits = false;
    while i < b.len() && b[i].is_ascii_digit() {
        digits = true;
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            digits = true;
            i += 1;
        }
    }
    if !digits {
        return 0.0;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let mut ed = false;
        while j < b.len() && b[j].is_ascii_digit() {
            ed = true;
            j += 1;
        }
        if ed {
            i = j;
        }
    }
    t[..i].parse::<f64>().unwrap_or(0.0)
}

/// True if entire string is a valid awk number (for strnum detection).
fn num_strict(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let b = s.as_bytes();
    let mut i = 0;
    if b[i] == b'+' || b[i] == b'-' {
        i += 1;
    }
    let mut digits = false;
    while i < b.len() && b[i].is_ascii_digit() {
        digits = true;
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            digits = true;
            i += 1;
        }
    }
    if !digits {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let mut ed = false;
        while i < b.len() && b[i].is_ascii_digit() {
            ed = true;
            i += 1;
        }
        if !ed {
            return false;
        }
    }
    i == b.len()
}

/// Format number the way awk does (16 significant digits-ish).
pub fn num_to_str(n: f64) -> String {
    if n == n.trunc() && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else if n == 0.0 {
        "0".into()
    } else {
        // use shortest roundtrip
        let s = format!("{}", n);
        s
    }
}

/// True when the scalar participates in numeric comparison (number or strnum).
pub fn is_numlike(s: &Scalar) -> bool {
    !s.uninit && (s.numval || s.strnum)
}

/// Compare two scalars per awk rules: uninit equals 0 and "", numbers
/// compare numerically, everything else compares as string.
pub fn cmp_scalar(a: &Scalar, b: &Scalar) -> std::cmp::Ordering {
    match (a.uninit, b.uninit) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => {
            if is_numlike(b) {
                0.0_f64
                    .partial_cmp(&b.num())
                    .unwrap_or(std::cmp::Ordering::Equal)
            } else {
                "".cmp(b.as_str())
            }
        }
        (false, true) => {
            if is_numlike(a) {
                a.num()
                    .partial_cmp(&0.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
            } else {
                a.as_str().cmp("")
            }
        }
        (false, false) => {
            if is_numlike(a) && is_numlike(b) {
                a.num()
                    .partial_cmp(&b.num())
                    .unwrap_or(std::cmp::Ordering::Equal)
            } else {
                a.as_str().cmp(b.as_str())
            }
        }
    }
}

/// True if scalar should be treated as number in boolean context.
pub fn cmp_ok_num(s: &Scalar) -> bool {
    is_numlike(s)
}

/// Process backslash escapes in a -v value or a `var=value` operand.
pub fn unescape(s: &str) -> String {
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            let l = utf8_len(b[i]);
            out.push_str(std::str::from_utf8(&b[i..i + l]).unwrap_or(""));
            i += l;
            continue;
        }
        i += 1;
        if i >= b.len() {
            out.push('\\');
            break;
        }
        let c = b[i];
        i += 1;
        match c {
            b'\\' => out.push('\\'),
            b'a' => out.push('\u{7}'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'v' => out.push('\u{b}'),
            b'/' => out.push('/'),
            b'x' => {
                let mut v = 0u32;
                let mut n = 0;
                while n < 2 && i < b.len() {
                    if let Some(d) = (b[i] as char).to_digit(16) {
                        v = v * 16 + d;
                        i += 1;
                        n += 1;
                    } else {
                        break;
                    }
                }
                if n == 0 {
                    out.push('x');
                } else {
                    out.push(char::from_u32(v).unwrap_or('\0'));
                }
            }
            b'0'..=b'7' => {
                let mut v = (c - b'0') as u32;
                let mut n = 1;
                while n < 3 && i < b.len() && (b'0'..=b'7').contains(&b[i]) {
                    v = v * 8 + (b[i] - b'0') as u32;
                    i += 1;
                    n += 1;
                }
                out.push(char::from_u32(v).unwrap_or('\0'));
            }
            // unknown escapes keep the letter, like gawk
            other => {
                let l = utf8_len(other);
                if l == 1 {
                    out.push(other as char);
                } else {
                    i += l - 1;
                }
            }
        }
    }
    out
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}
