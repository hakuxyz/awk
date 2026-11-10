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
}

impl Scalar {
    pub fn empty() -> Self {
        Scalar { s: String::new(), n: 0.0, strnum: false, uninit: true }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: impl Into<String>) -> Self {
        let s = s.into();
        let n = parse_num(&s);
        Scalar { s, n, strnum: false, uninit: false }
    }

    pub fn from_num(n: f64) -> Self {
        Scalar { s: num_to_str(n), n, strnum: false, uninit: false }
    }

    pub fn from_input(s: impl Into<String>) -> Self {
        let s = s.into();
        let n = parse_num(&s);
        let looks = !s.is_empty() && s.trim() == s && num_strict(&s);
        Scalar { s, n, strnum: looks, uninit: false }
    }

    /// Numeric value for arithmetic (uninit -> 0).
    pub fn num(&self) -> f64 {
        if self.uninit {
            0.0
        } else {
            self.n
        }
    }

    /// String value for concatenation/print (uninit -> "").
    pub fn as_str(&self) -> &str {
        if self.uninit {
            ""
        } else {
            &self.s
        }
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
    }

    pub fn set_str(&mut self, s: impl Into<String>) {
        self.s = s.into();
        self.n = parse_num(&self.s);
        self.strnum = false;
        self.uninit = false;
    }

    pub fn set_input(&mut self, s: impl Into<String>) {
        let s = s.into();
        self.n = parse_num(&s);
        self.strnum = !s.is_empty() && s.trim() == s && num_strict(&s);
        self.s = s;
        self.uninit = false;
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

/// Parse awk numeric string: leading whitespace, optional sign, digits, optional frac/exp.
fn parse_num(s: &str) -> f64 {
    let t = s.trim_start();
    if t.is_empty() {
        return 0.0;
    }
    match t.parse::<f64>() {
        Ok(n) if num_strict_prefix_ok(t, n) => n,
        Ok(_) => 0.0,
        Err(_) => 0.0,
    }
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

fn num_strict_prefix_ok(_t: &str, _n: f64) -> bool {
    true
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

/// Compare two scalars per awk rules (numeric if both strnum/num).
pub fn cmp_scalar(a: &Scalar, b: &Scalar) -> std::cmp::Ordering {
    let both_num = (!a.uninit && a.strnum) || (!a.uninit && !a.is_str())
        && ((!b.uninit && b.strnum) || (!b.uninit && !b.is_str()));
    // simplify: if both look numeric as values
    let a_num = !a.uninit && (a.strnum || !a.is_str() && num_strict(a.as_str()));
    let b_num = !b.uninit && (b.strnum || !b.is_str() && num_strict(b.as_str()));
    let _ = both_num;
    if a_num && b_num {
        a.num().partial_cmp(&b.num()).unwrap_or(std::cmp::Ordering::Equal)
    } else {
        a.as_str().cmp(b.as_str())
    }
}

/// True if scalar should be treated as number in boolean context.
pub fn cmp_ok_num(s: &Scalar) -> bool {
    !s.uninit && (s.strnum || num_strict(s.as_str()))
}
