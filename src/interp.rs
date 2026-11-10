//! Tree-walking interpreter for awk programs.

use crate::ast::*;
use crate::value::{ArrayRef, Scalar, Value};
use regex::Regex;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::rc::Rc;

pub enum Flow {
    Normal,
    Break,
    Continue,
    Next,
    Exit(i32),
    Return(Option<Value>),
}

pub struct Interp {
    pub globals: HashMap<String, Value>,
    pub arrays: HashMap<String, ArrayRef>,
    pub funcs: HashMap<String, FuncDef>,
    pub rules: Vec<Rule>,
    // awk specials
    pub nr: f64,
    pub fnr: f64,
    pub nf: f64,
    pub fs: String,
    pub ofs: String,
    pub ors: String,
    pub filename: String,
    pub subsep: String,
    pub exit_code: Option<i32>,
    pub rng_active: HashMap<usize, bool>,
    pub input: Box<dyn BufRead>,
    pub stdout: Box<dyn Write>,
    pub stderr: Box<dyn Write>,
    /// one-shot mode: already consumed a line as $0 (for main loop)
    pub pending_line: Option<String>,
    pub match_start: usize,
    pub match_len: usize,
    rand_seeded: bool,
    // open handles for getline < file / | cmd
    getline_files: HashMap<String, BufReader<std::fs::File>>,
    getline_cmds: HashMap<String, BufReader<std::process::ChildStdout>>,
}

impl Interp {
    pub fn new(funcs: Vec<FuncDef>, rules: Vec<Rule>) -> Self {
        let mut f = HashMap::new();
        for fd in funcs {
            f.insert(fd.name.clone(), fd);
        }
        Interp {
            globals: HashMap::new(),
            arrays: HashMap::new(),
            funcs: f,
            rules,
            nr: 0.0,
            fnr: 0.0,
            nf: 0.0,
            fs: " ".into(),
            ofs: " ".into(),
            ors: "\n".into(),
            filename: String::new(),
            subsep: "\u{1c}".into(),
            exit_code: None,
            rng_active: HashMap::new(),
            input: Box::new(BufReader::new(std::io::stdin())),
            stdout: Box::new(std::io::stdout()),
            stderr: Box::new(std::io::stderr()),
            pending_line: None,
            match_start: 0,
            match_len: 0,
            rand_seeded: false,
            getline_files: HashMap::new(),
            getline_cmds: HashMap::new(),
        }
    }

    /// Run BEGIN, then main loop over input files (paths), then END.
    pub fn run(&mut self, files: &[String]) -> i32 {
        if let Err(e) = self.run_begin() {
            let _ = writeln!(self.stderr, "awk: {}", e);
            return 2;
        }
        if let Some(c) = self.exit_code {
            return c;
        }

        let paths: Vec<String> = if files.is_empty() {
            vec!["-".into()]
        } else {
            files.to_vec()
        };

        for path in &paths {
            self.filename = if path == "-" { String::new() } else { path.clone() };
            self.fnr = 0.0;
            let res = if path == "-" {
                self.run_input()
            } else {
                match std::fs::File::open(path) {
                    Ok(f) => self.run_stream(f),
                    Err(e) => {
                        let _ = writeln!(self.stderr, "awk: cannot open {}: {}", path, e);
                        self.exit_code = Some(2);
                        return 2;
                    }
                }
            };
            if let Err(e) = res {
                let _ = writeln!(self.stderr, "awk: {}", e);
                return 2;
            }
            if let Some(c) = self.exit_code {
                return c;
            }
        }

        if let Err(e) = self.run_end() {
            let _ = writeln!(self.stderr, "awk: {}", e);
            return 2;
        }
        self.exit_code.unwrap_or(0)
    }

    fn run_begin(&mut self) -> Result<(), String> {
        for rule in self.rules.clone() {
            if matches!(rule.pattern, Some(Pattern::Begin)) {
                let action = rule.action.clone().unwrap_or_else(default_print);
                if let Flow::Exit(c) = self.exec_stmt(&action)? {
                    self.exit_code = Some(c);
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn run_end(&mut self) -> Result<(), String> {
        for rule in self.rules.clone() {
            if matches!(rule.pattern, Some(Pattern::End)) {
                let action = rule.action.unwrap_or_else(default_print);
                if let Flow::Exit(c) = self.exec_stmt(&action)? {
                    self.exit_code = Some(c);
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Read main input from the shared stdin handle (same as getline).
    fn run_input(&mut self) -> Result<(), String> {
        loop {
            let mut line = String::new();
            let n = self.input.read_line(&mut line).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            self.nr += 1.0;
            self.fnr += 1.0;
            self.set_record(&line);
            let _ = self.exec_main_rules()?;
            if self.exit_code.is_some() {
                return Ok(());
            }
        }
        Ok(())
    }

    fn run_stream<R: Read>(&mut self, r: R) -> Result<(), String> {
        let mut reader = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            self.nr += 1.0;
            self.fnr += 1.0;
            self.set_record(&line);
            let _ = self.exec_main_rules()?;
            if self.exit_code.is_some() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Split $0 into fields per FS and set NF.
    fn set_record(&mut self, rec: &str) {
        // store $0
        self.set_field_str(0, rec);
        let fields = split_fields(rec, &self.fs);
        self.nf = fields.len() as f64;
        for (i, f) in fields.iter().enumerate() {
            self.set_field_str(i + 1, f);
        }
        // clear fields beyond NF
        // (fields stored in globals as $1..$N dynamically)
    }

    fn set_field_str(&mut self, i: usize, s: &str) {
        let key = format!("${}", i);
        self.globals.insert(key, Value::Scalar(Scalar::from_input(s)));
    }

    fn get_field(&self, i: usize) -> Value {
        let key = format!("${}", i);
        self.globals.get(&key).cloned().unwrap_or_else(Value::uninit)
    }

    fn exec_main_rules(&mut self) -> Result<Flow, String> {
        for (idx, rule) in self.rules.clone().iter().enumerate() {
            if let Some(p) = &rule.pattern {
                match p {
                    Pattern::Begin | Pattern::End => continue,
                    Pattern::Regex(re) => {
                        if !self.match_re(re, &self.get_field(0).to_str()) {
                            continue;
                        }
                    }
                    Pattern::Expr(e) => {
                        let v = self.eval(e)?;
                        if !self.truthy(&v) {
                            continue;
                        }
                    }
                    Pattern::Range(a, b) => {
                        let active = self.rng_active.get(&idx).copied().unwrap_or(false);
                        if !active {
                            let start = self.eval(a)?;
                            if !self.truthy(&start) {
                                continue;
                            }
                            self.rng_active.insert(idx, true);
                        }
                        let action = rule.action.clone().unwrap_or_else(default_print);
                        self.exec_stmt(&action)?;
                        let end = self.eval(b)?;
                        if self.truthy(&end) {
                            self.rng_active.insert(idx, false);
                        }
                        continue;
                    }
                }
            }
            let action = rule.action.clone().unwrap_or_else(default_print);
            match self.exec_stmt(&action)? {
                Flow::Next => return Ok(Flow::Next),
                Flow::Exit(c) => {
                    self.exit_code = Some(c);
                    return Ok(Flow::Exit(c));
                }
                Flow::Break | Flow::Continue | Flow::Return(_) => {
                    // invalid at top level; ignore or error
                }
                Flow::Normal => {}
            }
        }
        Ok(Flow::Normal)
    }

    fn truthy(&self, v: &Value) -> bool {
        if v.is_array() {
            return false;
        }
        let s = v.as_scalar();
        if s.strnum || (!s.uninit && crate::value::cmp_ok_num(s)) {
            s.num() != 0.0
        } else {
            !s.as_str().is_empty()
        }
    }

    fn match_re(&mut self, re_src: &str, s: &str) -> bool {
        let re = match compile(re_src) {
            Ok(r) => r,
            Err(_) => return false,
        };
        match re.find(s) {
            Some(m) => {
                self.match_start = m.start();
                self.match_len = m.end() - m.start();
                true
            }
            None => {
                self.match_start = 0;
                self.match_len = 0;
                false
            }
        }
    }

    // ----- statements -----

    fn exec_stmt(&mut self, st: &Stmt) -> Result<Flow, String> {
        match st {
            Stmt::Empty => Ok(Flow::Normal),
            Stmt::Block(list) => {
                for s in list {
                    match self.exec_stmt(s)? {
                        Flow::Normal => {}
                        other => return Ok(other),
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::Expr(e) => {
                self.eval(e)?;
                Ok(Flow::Normal)
            }
            Stmt::If(c, t, f) => {
                let cv = self.eval(c)?;
                if self.truthy(&cv) {
                    self.exec_stmt(t)
                } else if let Some(f) = f {
                    self.exec_stmt(f)
                } else {
                    Ok(Flow::Normal)
                }
            }
            Stmt::While(c, body) => {
                loop {
                    let cv = self.eval(c)?;
                    if !self.truthy(&cv) {
                        break;
                    }
                    match self.exec_stmt(body)? {
                        Flow::Break => break,
                        Flow::Continue => continue,
                        Flow::Next => return Ok(Flow::Next),
                        Flow::Exit(ec) => return Ok(Flow::Exit(ec)),
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Normal => {}
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::DoWhile(body, c) => {
                loop {
                    match self.exec_stmt(body)? {
                        Flow::Break => break,
                        Flow::Continue => {}
                        Flow::Next => return Ok(Flow::Next),
                        Flow::Exit(c) => return Ok(Flow::Exit(c)),
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Normal => {}
                    }
                    let cv = self.eval(c)?;
                    if !self.truthy(&cv) {
                        break;
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::For(init, cond, step, body) => {
                if let Some(init) = init {
                    self.exec_stmt(init)?;
                }
                loop {
                    if let Some(c) = cond {
                        let cv = self.eval(c)?;
                        if !self.truthy(&cv) {
                            break;
                        }
                    }
                    match self.exec_stmt(body)? {
                        Flow::Break => break,
                        Flow::Continue => {}
                        Flow::Next => return Ok(Flow::Next),
                        Flow::Exit(c) => return Ok(Flow::Exit(c)),
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Normal => {}
                    }
                    if let Some(step) = step {
                        self.exec_stmt(step)?;
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::ForIn(vars, arr, body) => {
                let arr = self.arrays.get(arr).cloned().unwrap_or_else(|| {
                    let a: ArrayRef = Rc::new(RefCell::new(HashMap::new()));
                    a
                });
                let keys: Vec<String> = arr.borrow().keys().cloned().collect();
                for k in keys {
                    for v in vars {
                        self.globals.insert(v.clone(), Value::str(k.clone()));
                    }
                    match self.exec_stmt(body)? {
                        Flow::Break => break,
                        Flow::Continue => continue,
                        Flow::Next => return Ok(Flow::Next),
                        Flow::Exit(c) => return Ok(Flow::Exit(c)),
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Normal => {}
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::Break => Ok(Flow::Break),
            Stmt::Continue => Ok(Flow::Continue),
            Stmt::Next => Ok(Flow::Next),
            Stmt::Exit(e) => {
                let c = if let Some(e) = e {
                    self.eval(e)?.to_num() as i32
                } else {
                    0
                };
                Ok(Flow::Exit(c))
            }
            Stmt::Return(e) => {
                let v = if let Some(e) = e {
                    Some(self.eval(e)?)
                } else {
                    None
                };
                Ok(Flow::Return(v))
            }
            Stmt::Delete(name, idx) => {
                if let Some(idx) = idx {
                    let k = self.eval(idx)?.to_str();
                    if let Some(a) = self.arrays.get(name) {
                        a.borrow_mut().remove(&k);
                    }
                } else if let Some(a) = self.arrays.get(name) {
                    a.borrow_mut().clear();
                } else {
                    // deleting non-array is no-op or create empty
                }
                Ok(Flow::Normal)
            }
            Stmt::Print(args, redir) => {
                let mut out = String::new();
                if args.is_empty() {
                    out.push_str(&self.get_field(0).to_str());
                } else {
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            out.push_str(&self.ofs);
                        }
                        let v = self.eval(a)?;
                        out.push_str(&v.to_str());
                    }
                }
                out.push_str(&self.ors);
                self.emit(out, redir)?;
                Ok(Flow::Normal)
            }
            Stmt::Printf(args, redir) => {
                let mut vals = Vec::new();
                for a in args {
                    vals.push(self.eval(a)?);
                }
                let out = do_printf(&vals, &self.ofs);
                self.emit(out, redir)?;
                Ok(Flow::Normal)
            }
            Stmt::Getline(_, _) => {
                // simplified: not fully supported in statement form yet
                Ok(Flow::Normal)
            }
        }
    }

    fn emit(&mut self, s: String, redir: &Option<Redir>) -> Result<(), String> {
        match redir {
            None => {
                self.stdout.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
                let _ = self.stdout.flush();
                Ok(())
            }
            Some(Redir::Trunc(e)) => {
                let path = self.eval(e)?.to_str();
                let mut f = std::fs::File::create(&path).map_err(|e| e.to_string())?;
                f.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
                Ok(())
            }
            Some(Redir::Append(e)) => {
                let path = self.eval(e)?.to_str();
                use std::fs::OpenOptions;
                let mut f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .map_err(|e| e.to_string())?;
                f.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
                Ok(())
            }
            Some(Redir::Pipe(e)) => {
                let cmd = self.eval(e)?.to_str();
                use std::process::{Command, Stdio};
                let mut child = Command::new("sh")
                    .arg("-c")
                    .arg(&cmd)
                    .stdin(Stdio::piped())
                    .spawn()
                    .map_err(|e| e.to_string())?;
                if let Some(stdin) = child.stdin.as_mut() {
                    stdin.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
                }
                let _ = child.wait();
                Ok(())
            }
        }
    }

    // ----- expressions -----

    fn eval(&mut self, e: &Expr) -> Result<Value, String> {
        match e {
            Expr::Num(n) => Ok(Value::num(*n)),
            Expr::Str(s) => Ok(Value::str(s.clone())),
            Expr::Regex(r) => Ok(Value::str(r.clone())),
            Expr::Var(name) => self.get_var(name),
            Expr::Field(idx) => {
                let i = self.eval(idx)?;
                if i.is_array() {
                    return Err("field reference on array".into());
                }
                let n = i.to_num();
                if n < 0.0 {
                    return Err("negative field".into());
                }
                Ok(self.get_field(n as usize))
            }
            Expr::Index(arr, idx) => {
                let arr_name = match &**arr {
                    Expr::Var(n) => n.clone(),
                    other => {
                        let _ = other;
                        return Err("subscript of non-array".into());
                    }
                };
                let k = self.subscript_key(idx)?;
                let a = self.ensure_array(&arr_name);
                let v = a.borrow().get(&k).cloned().unwrap_or_else(Value::uninit);
                Ok(v)
            }
            Expr::Assign(t, r) => {
                let v = self.eval(r)?;
                self.assign(t, v.clone())?;
                Ok(v)
            }
            Expr::PlusAssign(t, r) => {
                let cur = self.eval(t)?.to_num();
                let rhs = self.eval(r)?.to_num();
                let v = Value::num(cur + rhs);
                self.assign(t, v.clone())?;
                Ok(v)
            }
            Expr::MinusAssign(t, r) => {
                let cur = self.eval(t)?.to_num();
                let rhs = self.eval(r)?.to_num();
                let v = Value::num(cur - rhs);
                self.assign(t, v.clone())?;
                Ok(v)
            }
            Expr::MulAssign(t, r) => {
                let cur = self.eval(t)?.to_num();
                let rhs = self.eval(r)?.to_num();
                let v = Value::num(cur * rhs);
                self.assign(t, v.clone())?;
                Ok(v)
            }
            Expr::DivAssign(t, r) => {
                let cur = self.eval(t)?.to_num();
                let rhs = self.eval(r)?.to_num();
                let v = Value::num(cur / rhs);
                self.assign(t, v.clone())?;
                Ok(v)
            }
            Expr::ModAssign(t, r) => {
                let cur = self.eval(t)?.to_num();
                let rhs = self.eval(r)?.to_num();
                let v = Value::num(cur % rhs);
                self.assign(t, v.clone())?;
                Ok(v)
            }
            Expr::PowAssign(t, r) => {
                let cur = self.eval(t)?.to_num();
                let rhs = self.eval(r)?.to_num();
                let v = Value::num(cur.powf(rhs));
                self.assign(t, v.clone())?;
                Ok(v)
            }
            Expr::PostInc(t) => {
                let old = self.eval(t)?.to_num();
                let v = Value::num(old + 1.0);
                self.assign(t, v)?;
                Ok(Value::num(old))
            }
            Expr::PostDec(t) => {
                let old = self.eval(t)?.to_num();
                let v = Value::num(old - 1.0);
                self.assign(t, v)?;
                Ok(Value::num(old))
            }
            Expr::PreInc(t) => {
                let old = self.eval(t)?.to_num();
                let v = Value::num(old + 1.0);
                self.assign(t, v.clone())?;
                Ok(v)
            }
            Expr::PreDec(t) => {
                let old = self.eval(t)?.to_num();
                let v = Value::num(old - 1.0);
                self.assign(t, v.clone())?;
                Ok(v)
            }
            Expr::Neg(a) => Ok(Value::num(-self.eval(a)?.to_num())),
            Expr::Not(a) => {
                let v = self.eval(a)?;
                Ok(Value::num(if self.truthy(&v) { 0.0 } else { 1.0 }))
            }
            Expr::Add(a, b) => {
                let x = self.eval(a)?.to_num();
                let y = self.eval(b)?.to_num();
                Ok(Value::num(x + y))
            }
            Expr::Sub(a, b) => {
                let x = self.eval(a)?.to_num();
                let y = self.eval(b)?.to_num();
                Ok(Value::num(x - y))
            }
            Expr::Mul(a, b) => {
                let x = self.eval(a)?.to_num();
                let y = self.eval(b)?.to_num();
                Ok(Value::num(x * y))
            }
            Expr::Div(a, b) => {
                let x = self.eval(a)?.to_num();
                let y = self.eval(b)?.to_num();
                Ok(Value::num(x / y))
            }
            Expr::Mod(a, b) => {
                let x = self.eval(a)?.to_num();
                let y = self.eval(b)?.to_num();
                Ok(Value::num(x % y))
            }
            Expr::Pow(a, b) => {
                let x = self.eval(a)?.to_num();
                let y = self.eval(b)?.to_num();
                Ok(Value::num(x.powf(y)))
            }
            Expr::Concat(a, b) => {
                let x = self.eval(a)?;
                let y = self.eval(b)?;
                let s = format!("{}{}", x.to_str(), y.to_str());
                Ok(Value::str(s))
            }
            Expr::Lt(a, b) => self.cmp_op(a, b, |o| o == std::cmp::Ordering::Less),
            Expr::Le(a, b) => self.cmp_op(a, b, |o| o != std::cmp::Ordering::Greater),
            Expr::Gt(a, b) => self.cmp_op(a, b, |o| o == std::cmp::Ordering::Greater),
            Expr::Ge(a, b) => self.cmp_op(a, b, |o| o != std::cmp::Ordering::Less),
            Expr::Eq(a, b) => self.cmp_op(a, b, |o| o == std::cmp::Ordering::Equal),
            Expr::Ne(a, b) => self.cmp_op(a, b, |o| o != std::cmp::Ordering::Equal),
            Expr::Match(a, b) => {
                let s = self.eval(a)?.to_str();
                let re = self.regex_of(b)?;
                Ok(Value::num(if self.match_re(&re, &s) { 1.0 } else { 0.0 }))
            }
            Expr::NoMatch(a, b) => {
                let s = self.eval(a)?.to_str();
                let re = self.regex_of(b)?;
                Ok(Value::num(if self.match_re(&re, &s) { 0.0 } else { 1.0 }))
            }
            Expr::And(a, b) => {
                let x = self.eval(a)?;
                if !self.truthy(&x) {
                    return Ok(Value::num(0.0));
                }
                let y = self.eval(b)?;
                Ok(Value::num(if self.truthy(&y) { 1.0 } else { 0.0 }))
            }
            Expr::Or(a, b) => {
                let x = self.eval(a)?;
                if self.truthy(&x) {
                    return Ok(Value::num(1.0));
                }
                let y = self.eval(b)?;
                Ok(Value::num(if self.truthy(&y) { 1.0 } else { 0.0 }))
            }
            Expr::Ternary(c, a, b) => {
                let __v = self.eval(c)?;
                if self.truthy(&__v) {
                    self.eval(a)
                } else {
                    self.eval(b)
                }
            }
            Expr::InArray(idx, arr) => {
                let k = self.eval(idx)?.to_str();
                let a = self.ensure_array(arr);
                Ok(Value::num(if a.borrow().contains_key(&k) { 1.0 } else { 0.0 }))
            }
            Expr::Call(name, args) => self.call(name, args),
        }
    }

    fn cmp_op(&mut self, a: &Expr, b: &Expr, f: impl Fn(std::cmp::Ordering) -> bool) -> Result<Value, String> {
        let x = self.eval(a)?;
        let y = self.eval(b)?;
        if x.is_array() || y.is_array() {
            return Err("comparison with array".into());
        }
        let ord = crate::value::cmp_scalar(x.as_scalar(), y.as_scalar());
        Ok(Value::num(if f(ord) { 1.0 } else { 0.0 }))
    }

    fn regex_of(&mut self, e: &Expr) -> Result<String, String> {
        match e {
            Expr::Regex(r) | Expr::Str(r) => Ok(r.clone()),
            other => Ok(self.eval(other)?.to_str()),
        }
    }

    fn subscript_key(&mut self, e: &Expr) -> Result<String, String> {
        // concatenate with SUBSEP for multi-subscript (only single here)
        let v = self.eval(e)?;
        if v.is_array() {
            return Err("array subscript".into());
        }
        Ok(v.to_str())
    }

    fn ensure_array(&mut self, name: &str) -> ArrayRef {
        if let Some(a) = self.arrays.get(name) {
            if name == "ENVIRON" && a.borrow().is_empty() {
                for (k, v) in std::env::vars() {
                    a.borrow_mut().insert(k, Value::str(v));
                }
            }
            return a.clone();
        }
        self.globals.remove(name);
        let a: ArrayRef = Rc::new(RefCell::new(HashMap::new()));
        if name == "ENVIRON" {
            for (k, v) in std::env::vars() {
                a.borrow_mut().insert(k, Value::str(v));
            }
        }
        self.arrays.insert(name.to_string(), a.clone());
        a
    }

    fn get_var(&mut self, name: &str) -> Result<Value, String> {
        match name {
            "NR" => return Ok(Value::num(self.nr)),
            "NF" => return Ok(Value::num(self.nf)),
            "FNR" => return Ok(Value::num(self.fnr)),
            "FS" => return Ok(Value::str(self.fs.clone())),
            "OFS" => return Ok(Value::str(self.ofs.clone())),
            "ORS" => return Ok(Value::str(self.ors.clone())),
            "FILENAME" => return Ok(Value::str(self.filename.clone())),
            "SUBSEP" => return Ok(Value::str(self.subsep.clone())),
            "RSTART" => return Ok(Value::num(self.match_start as f64 + 1.0)),
            "RLENGTH" => return Ok(Value::num(self.match_len as f64)),
            "ENVIRON" => {
                let a = self.ensure_array("ENVIRON");
                if a.borrow().is_empty() {
                    for (k, v) in std::env::vars() {
                        a.borrow_mut().insert(k, Value::str(v));
                    }
                }
                return Ok(Value::Array(a));
            }
            _ => {}
        }
        if let Some(a) = self.arrays.get(name) {
            return Ok(Value::Array(a.clone()));
        }
        if let Some(v) = self.globals.get(name) {
            return Ok(v.clone());
        }
        Ok(Value::uninit())
    }

    fn assign(&mut self, target: &Expr, v: Value) -> Result<(), String> {
        match target {
            Expr::Var(name) => match name.as_str() {
                "NR" => self.nr = v.to_num(),
                "NF" => {
                    let n = v.to_num();
                    self.nf = n;
                    // truncating NF clears fields > n
                }
                "FNR" => self.fnr = v.to_num(),
                "FS" => self.fs = v.to_str(),
                "OFS" => self.ofs = v.to_str(),
                "ORS" => self.ors = v.to_str(),
                "FILENAME" => self.filename = v.to_str(),
                "SUBSEP" => self.subsep = v.to_str(),
                "RSTART" => self.match_start = (v.to_num() - 1.0) as usize,
                "RLENGTH" => self.match_len = v.to_num() as usize,
                _ => {
                    if v.is_array() {
                        self.arrays.insert(name.clone(), match v {
                            Value::Array(a) => a,
                            _ => unreachable!(),
                        });
                    } else {
                        self.arrays.remove(name);
                        self.globals.insert(name.clone(), v);
                    }
                }
            },
            Expr::Field(idx) => {
                let i = self.eval(idx)?.to_num() as usize;
                let s = v.to_str();
                if i == 0 {
                    self.set_record(&s);
                } else {
                    self.set_field_str(i, &s);
                    if (i as f64) > self.nf {
                        self.nf = i as f64;
                    }
                }
            }
            Expr::Index(arr, sub) => {
                let arr_name = match &**arr {
                    Expr::Var(n) => n.clone(),
                    _ => return Err("bad array assign".into()),
                };
                let k = self.subscript_key(sub)?;
                let a = self.ensure_array(&arr_name);
                if v.is_array() {
                    // nested not supported as value — replace element
                    a.borrow_mut().insert(k, Value::uninit());
                } else {
                    a.borrow_mut().insert(k, v);
                }
            }
            other => return Err(format!("cannot assign to {:?}", other)),
        }
        Ok(())
    }

    fn call(&mut self, name: &str, args: &[Expr]) -> Result<Value, String> {
        // user functions
        if let Some(fd) = self.funcs.get(name).cloned() {
            if args.len() != fd.params.len() {
                return Err(format!(
                    "function {}: expected {} args, got {}",
                    name,
                    fd.params.len(),
                    args.len()
                ));
            }
            let mut vals = Vec::new();
            for a in args {
                vals.push(self.eval(a)?);
            }
            // local scope: save globals for params
            let mut saved: Vec<(String, Option<Value>)> = Vec::new();
            for (p, v) in fd.params.iter().zip(vals) {
                saved.push((p.clone(), self.globals.get(p).cloned()));
                // arrays passed by reference: if Value::Array, put in arrays map too
                if v.is_array() {
                    if let Value::Array(a) = v {
                        self.arrays.insert(p.clone(), a);
                        self.globals.insert(p.clone(), Value::uninit());
                    }
                } else {
                    self.arrays.remove(p);
                    self.globals.insert(p.clone(), v);
                }
            }
            let result = match self.exec_stmt(&fd.body) {
                Ok(Flow::Return(v)) => v.unwrap_or_else(Value::uninit),
                Ok(Flow::Exit(c)) => {
                    self.exit_code = Some(c);
                    return Err("exit".into());
                }
                Ok(_) => Value::uninit(),
                Err(e) => {
                    self.restore_locals(&saved);
                    return Err(e);
                }
            };
            self.restore_locals(&saved);
            return Ok(result);
        }

        match name {
            "length" => {
                if args.is_empty() {
                    return Ok(Value::num(self.get_field(0).to_str().len() as f64));
                }
                let v = self.eval(&args[0])?;
                if v.is_array()
                    && let Value::Array(a) = v {
                        return Ok(Value::num(a.borrow().len() as f64));
                    }
                Ok(Value::num(v.to_str().len() as f64))
            }
            "substr" => {
                let s = self.eval(&args[0])?.to_str();
                let start = self.eval(&args.get(1).cloned().unwrap_or(Expr::Num(1.0)))?.to_num() as i64;
                let len = if args.len() >= 3 {
                    Some(self.eval(&args[2])?.to_num() as i64)
                } else {
                    None
                };
                let chars: Vec<char> = s.chars().collect();
                let n = chars.len() as i64;
                let from = (start - 1).max(0);
                let to = match len {
                    Some(l) => (from + l).min(n),
                    None => n,
                };
                if from >= n || to <= from {
                    return Ok(Value::str(""));
                }
                let out: String = chars[from as usize..to as usize].iter().collect();
                Ok(Value::str(out))
            }
            "index" => {
                let s = self.eval(&args[0])?.to_str();
                let t = self.eval(&args[1])?.to_str();
                match s.find(&t) {
                    Some(i) => {
                        // 1-based char index
                        Ok(Value::num(s[..i].chars().count() as f64 + 1.0))
                    }
                    None => Ok(Value::num(0.0)),
                }
            }
            "split" => {
                // split(s, arr, fs)
                let s = self.eval(&args[0])?.to_str();
                let arr_name = match &args[1] {
                    Expr::Var(n) => n.clone(),
                    _ => return Err("split: second arg must be array".into()),
                };
                let fs = if args.len() >= 3 {
                    self.eval(&args[2])?.to_str()
                } else {
                    self.fs.clone()
                };
                let a = self.ensure_array(&arr_name);
                a.borrow_mut().clear();
                let parts = split_fields(&s, &fs);
                for (i, p) in parts.iter().enumerate() {
                    a.borrow_mut().insert((i + 1).to_string(), Value::Scalar(Scalar::from_input(p.clone())));
                }
                Ok(Value::num(parts.len() as f64))
            }
            "sub" | "gsub" => {
                let re_src = self.regex_of(&args[0])?;
                let repl = self.eval(&args[1])?.to_str();
                let target = args
                    .get(2)
                    .cloned()
                    .unwrap_or_else(|| Expr::Field(Box::new(Expr::Num(0.0))));
                let s = self.eval(&target)?.to_str();
                let re = compile(&re_src).map_err(|e| e.to_string())?;
                let mut count = 0usize;
                let out = if name == "gsub" {
                    re.replace_all(&s, |caps: &regex::Captures| {
                        count += 1;
                        let whole = caps.get(0).map(|m| m.as_str()).unwrap_or("");
                        expand_repl(&repl, whole, &[])
                    })
                    .into_owned()
                } else {
                    re.replace(&s, |caps: &regex::Captures| {
                        count += 1;
                        let whole = caps.get(0).map(|m| m.as_str()).unwrap_or("");
                        expand_repl(&repl, whole, &[])
                    })
                    .into_owned()
                };
                if count > 0 {
                    self.assign(&target, Value::str(out))?;
                }
                Ok(Value::num(count as f64))
            }
            "sprintf" => {
                let vals: Result<Vec<_>, _> = args.iter().map(|a| self.eval(a)).collect();
                let vals = vals?;
                Ok(Value::str(do_printf(&vals, " ")))
            }
            "sin" | "cos" | "atan2" | "log" | "exp" | "sqrt" | "int" => {
                let x = self.eval(&args[0])?.to_num();
                let y = if args.len() > 1 { self.eval(&args[1])?.to_num() } else { 0.0 };
                let n = match name {
                    "sin" => x.sin(),
                    "cos" => x.cos(),
                    "atan2" => y.atan2(x),
                    "log" => x.ln(),
                    "exp" => x.exp(),
                    "sqrt" => x.sqrt(),
                    "int" => x.trunc(),
                    _ => unreachable!(),
                };
                Ok(Value::num(n))
            }
            "rand" => {
                if !self.rand_seeded {
                    // seeded by srand below
                }
                use std::time::{SystemTime, UNIX_EPOCH};
                // simple xorshift state in globals
                let mut state = self
                    .globals
                    .get("__rand_state")
                    .map(|v| v.to_num() as u64)
                    .unwrap_or_else(|| {
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.subsec_nanos() as u64 ^ 0x9e3779b9)
                            .unwrap_or(42)
                    });
                if state == 0 {
                    state = 42;
                }
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                self.globals.insert("__rand_state".to_string(), Value::num(state as f64));
                let r = (state >> 11) as f64 / (1u64 << 53) as f64;
                Ok(Value::num(r))
            }
            "srand" => {
                let seed = if args.is_empty() {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs() as f64)
                        .unwrap_or(0.0)
                } else {
                    self.eval(&args[0])?.to_num()
                };
                let old = self
                    .globals
                    .get("__rand_state")
                    .map(|v| v.to_num())
                    .unwrap_or(0.0);
                self.globals.insert("__rand_state".to_string(), Value::num(seed));
                self.rand_seeded = true;
                Ok(Value::num(old))
            }
            "tolower" => Ok(Value::str(self.eval(&args[0])?.to_str().to_lowercase())),
            "toupper" => Ok(Value::str(self.eval(&args[0])?.to_str().to_uppercase())),
            "match" => {
                let s = self.eval(&args[0])?.to_str();
                let re = self.regex_of(&args[1])?;
                if self.match_re(&re, &s) {
                    Ok(Value::num(self.match_start as f64 + 1.0))
                } else {
                    self.match_start = 0;
                    self.match_len = 0;
                    Ok(Value::num(0.0))
                }
            }
            "close" => Ok(Value::num(0.0)),
            "system" => {
                let cmd = self.eval(&args[0])?.to_str();
                use std::process::Command;
                let st = Command::new("sh").arg("-c").arg(&cmd).status();
                Ok(Value::num(match st {
                    Ok(s) => s.code().unwrap_or(0) as f64,
                    Err(_) => -1.0,
                }))
            }
            "getline" => self.builtin_getline(args),
            other => Err(format!("unknown function {}", other)),
        }
    }

    /// getline [var] [< file | | cmd] — returns 1 ok, 0 EOF, -1 error.
    fn builtin_getline(&mut self, args: &[Expr]) -> Result<Value, String> {
        let target = args[0].clone();
        let (kind, src) = if args.len() > 1 {
            match &args[1] {
                Expr::Call(op, a) if op == "<" => ("file", a[0].clone()),
                Expr::Call(op, a) if op == "|" => ("cmd", a[0].clone()),
                _ => ("stdin", Expr::Num(0.0)),
            }
        } else {
            ("stdin", Expr::Num(0.0))
        };
        let line = match kind {
            "stdin" => {
                let mut line = String::new();
                let n = self.input.read_line(&mut line).map_err(|e| e.to_string())?;
                if n == 0 {
                    None
                } else {
                    while line.ends_with('\n') || line.ends_with('\r') {
                        line.pop();
                    }
                    Some(line)
                }
            }
            "file" => {
                let path = self.eval(&src)?.to_str();
                if !self.getline_files.contains_key(&path) {
                    match std::fs::File::open(&path) {
                        Ok(f) => {
                            self.getline_files
                                .insert(path.clone(), BufReader::new(f));
                        }
                        Err(_) => return Ok(Value::num(-1.0)),
                    }
                }
                let mut line = String::new();
                let r = self.getline_files.get_mut(&path).unwrap();
                let n = r.read_line(&mut line).map_err(|e| e.to_string())?;
                if n == 0 {
                    self.getline_files.remove(&path);
                    None
                } else {
                    while line.ends_with('\n') || line.ends_with('\r') {
                        line.pop();
                    }
                    Some(line)
                }
            }
            "cmd" => {
                let cmd = self.eval(&src)?.to_str();
                if !self.getline_cmds.contains_key(&cmd) {
                    use std::process::{Command, Stdio};
                    let mut child = Command::new("sh")
                        .arg("-c")
                        .arg(&cmd)
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null())
                        .spawn()
                        .map_err(|e| e.to_string())?;
                    let stdout = child.stdout.take().ok_or_else(|| "no stdout".to_string())?;
                    // dropping child is fine: stdout pipe was moved out
                    drop(child);
                    self.getline_cmds.insert(cmd.clone(), BufReader::new(stdout));
                }
                let mut line = String::new();
                let r = self.getline_cmds.get_mut(&cmd).unwrap();
                let n = r.read_line(&mut line).map_err(|e| e.to_string())?;
                if n == 0 {
                    self.getline_cmds.remove(&cmd);
                    None
                } else {
                    while line.ends_with('\n') || line.ends_with('\r') {
                        line.pop();
                    }
                    Some(line)
                }
            }
            _ => None,
        };
        match line {
            Some(line) => {
                if let Expr::Var(name) = &target {
                    if name == "$0" {
                        self.set_record(&line);
                        if kind == "stdin" {
                            self.nr += 1.0;
                            self.fnr += 1.0;
                        }
                    } else {
                        self.assign(&target, Value::Scalar(Scalar::from_input(&line)))?;
                        if kind == "stdin" {
                            self.nr += 1.0;
                            self.fnr += 1.0;
                        }
                    }
                } else {
                    self.assign(&target, Value::Scalar(Scalar::from_input(&line)))?;
                }
                Ok(Value::num(1.0))
            }
            None => Ok(Value::num(0.0)),
        }
    }

    fn restore_locals(&mut self, saved: &[(String, Option<Value>)]) {
        for (k, old) in saved {
            match old {
                Some(v) => {
                    self.globals.insert(k.clone(), v.clone());
                }
                None => {
                    self.globals.remove(k);
                }
            }
            self.arrays.remove(k);
        }
    }
}

fn default_print() -> Stmt {
    Stmt::Print(vec![Expr::Field(Box::new(Expr::Num(0.0)))], None)
}

fn compile(src: &str) -> Result<Regex, regex::Error> {
    Regex::new(src)
}

fn expand_repl(repl: &str, whole: &str, _caps: &[&str]) -> String {
    let mut out = String::new();
    let b = repl.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            let n = b[i + 1];
            if n == b'&' {
                out.push_str(whole);
                i += 2;
                continue;
            }
            if n.is_ascii_digit() {
                // group n — we don't have full captures here; emit empty
                i += 2;
                continue;
            }
            if n == b'\\' {
                out.push('\\');
                i += 2;
                continue;
            }
            out.push('\\');
            out.push(n as char);
            i += 2;
            continue;
        }
        if b[i] == b'&' {
            out.push_str(whole);
            i += 1;
            continue;
        }
        // utf8
        let ch_len = utf8_len(b[i]);
        out.push_str(std::str::from_utf8(&b[i..i + ch_len]).unwrap_or("?"));
        i += ch_len;
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

/// Split record into fields per FS.
fn split_fields(rec: &str, fs: &str) -> Vec<String> {
    if fs == " " {
        // default: whitespace run, strip leading/trailing
        rec.split_whitespace().map(|s| s.to_string()).collect()
    } else if fs == "\t" {
        rec.split('\t').map(|s| s.to_string()).collect()
    } else if let Some(re) = fs.strip_prefix('/') {
        if re.ends_with('/') && !re.is_empty() {
            let re = &re[..re.len() - 1];
            if let Ok(rx) = Regex::new(re) {
                let parts: Vec<String> = rx.split(rec).map(|s| s.to_string()).collect();
                // awk keeps empty trailing fields only if FS is single non-space?
                // regex split: awk drops trailing empties for FS=" "
                return parts;
            }
        }
        // FS as single char or string without slashes
        if fs.is_empty() {
            return vec![rec.to_string()];
        }
        rec.split(fs).map(|s| s.to_string()).collect()
    } else if fs.len() == 1 {
        rec.split(fs).map(|s| s.to_string()).collect()
    } else {
        // multi-char FS is an ERE per POSIX
        match Regex::new(fs) {
            Ok(rx) => rx.split(rec).map(|s| s.to_string()).collect(),
            Err(_) => rec.split(fs).map(|s| s.to_string()).collect(),
        }
    }
}

/// printf family formatting.
fn do_printf(args: &[Value], _ofs: &str) -> String {
    if args.is_empty() {
        return String::new();
    }
    let fmt = args[0].to_str();
    let mut out = String::new();
    let fb = fmt.as_bytes();
    let mut fi = 0;
    let mut ai = 1;
    while fi < fb.len() {
        if fb[fi] == b'%' {
            if fi + 1 < fb.len() && fb[fi + 1] == b'%' {
                out.push('%');
                fi += 2;
                continue;
            }
            // parse %[flags][width][.prec][conv]
            let start = fi;
            fi += 1;
            while fi < fb.len() && matches!(fb[fi], b'-' | b'+' | b' ' | b'#' | b'0') {
                fi += 1;
            }
            while fi < fb.len() && fb[fi].is_ascii_digit() {
                fi += 1;
            }
            if fi < fb.len() && fb[fi] == b'.' {
                fi += 1;
                while fi < fb.len() && fb[fi].is_ascii_digit() {
                    fi += 1;
                }
            }
            // length modifiers
            while fi < fb.len() && matches!(fb[fi], b'h' | b'l' | b'L' | b'z' | b't' | b'j') {
                fi += 1;
            }
            if fi >= fb.len() {
                out.push_str(&fmt[start..]);
                break;
            }
            let conv = fb[fi];
            fi += 1;
            let spec = &fmt[start..fi];
            let arg = args.get(ai);
            if arg.is_some() {
                ai += 1;
            }
            let val = arg.cloned().unwrap_or_else(Value::uninit);
            let piece = format_conv(spec, conv, &val);
            out.push_str(&piece);
        } else {
            // copy one utf8 char
            let len = utf8_len(fb[fi]);
            out.push_str(std::str::from_utf8(&fb[fi..fi + len]).unwrap_or(""));
            fi += len;
        }
    }
    out
}

fn format_conv(spec: &str, conv: u8, val: &Value) -> String {
    let s = val.to_str();
    let n = val.to_num();
    match conv {
        b'd' | b'i' => {
            let mut width = 0usize;
            let mut prec: Option<usize> = None;
            let mut left = false;
            let mut zero = false;
            parse_spec(spec, &mut width, &mut prec, &mut left, &mut zero);
            let mut body = format!("{}", n as i64);
            if let Some(p) = prec {
                while body.len() < p {
                    body.insert(0, '0');
                }
            }
            pad(&body, width, left, zero)
        }
        b'u' => {
            let mut width = 0usize;
            let mut prec: Option<usize> = None;
            let mut left = false;
            let mut zero = false;
            parse_spec(spec, &mut width, &mut prec, &mut left, &mut zero);
            let mut body = format!("{}", n.max(0.0) as u64);
            if let Some(p) = prec {
                while body.len() < p {
                    body.insert(0, '0');
                }
            }
            pad(&body, width, left, zero)
        }
        b'f' | b'F' => {
            let mut width = 0usize;
            let mut prec: Option<usize> = None;
            let mut left = false;
            let mut zero = false;
            parse_spec(spec, &mut width, &mut prec, &mut left, &mut zero);
            let p = prec.unwrap_or(6);
            let body = format!("{:.*}", p, n);
            pad(&body, width, left, zero)
        }
        b'e' | b'E' => {
            let mut width = 0usize;
            let mut prec: Option<usize> = None;
            let mut left = false;
            let mut zero = false;
            parse_spec(spec, &mut width, &mut prec, &mut left, &mut zero);
            let p = prec.unwrap_or(6);
            let body = if conv == b'e' {
                format!("{:.*e}", p, n)
            } else {
                format!("{:.*E}", p, n)
            };
            pad(&body, width, left, zero)
        }
        b'g' | b'G' => {
            let mut width = 0usize;
            let mut prec: Option<usize> = None;
            let mut left = false;
            let mut zero = false;
            parse_spec(spec, &mut width, &mut prec, &mut left, &mut zero);
            let p = prec.unwrap_or(6);
            let body = format!("{:.*}", p, n);
            pad(&body, width, left, zero)
        }
        b's' => {
            let mut width = 0usize;
            let mut prec: Option<usize> = None;
            let mut left = false;
            let mut zero = false;
            parse_spec(spec, &mut width, &mut prec, &mut left, &mut zero);
            let mut body = s.clone();
            if let Some(p) = prec {
                let chars: Vec<char> = body.chars().collect();
                if chars.len() > p {
                    body = chars[..p].iter().collect();
                }
            }
            pad(&body, width, left, false)
        }
        b'c' => {
            let ch = if let Some(c) = s.chars().next() {
                c
            } else {
                char::from_u32(n as u32).unwrap_or('\0')
            };
            let mut width = 0usize;
            let mut left = false;
            let mut zero = false;
            parse_spec(spec, &mut width, &mut None, &mut left, &mut zero);
            pad(&ch.to_string(), width, left, zero)
        }
        b'x' | b'X' => {
            let mut width = 0usize;
            let mut prec: Option<usize> = None;
            let mut left = false;
            let mut zero = false;
            parse_spec(spec, &mut width, &mut prec, &mut left, &mut zero);
            let v = n as u64;
            let mut body = if conv == b'x' {
                format!("{:x}", v)
            } else {
                format!("{:X}", v)
            };
            if let Some(p) = prec {
                while body.len() < p {
                    body.insert(0, '0');
                }
            }
            pad(&body, width, left, zero)
        }
        b'o' => {
            let mut width = 0usize;
            let mut prec: Option<usize> = None;
            let mut left = false;
            let mut zero = false;
            parse_spec(spec, &mut width, &mut prec, &mut left, &mut zero);
            let mut body = format!("{:o}", n as u64);
            if let Some(p) = prec {
                while body.len() < p {
                    body.insert(0, '0');
                }
            }
            pad(&body, width, left, zero)
        }
        _ => s.to_string(),
    }
}

fn parse_spec(spec: &str, width: &mut usize, prec: &mut Option<usize>, left: &mut bool, zero: &mut bool) {
    let b = spec.as_bytes();
    let mut i = 1; // skip %
    if i < b.len() && b[i] == b'-' {
        *left = true;
        i += 1;
    }
    while i < b.len() && matches!(b[i], b'+' | b' ' | b'#') {
        i += 1;
    }
    if i < b.len() && b[i] == b'0' {
        *zero = true;
        i += 1;
    }
    let ws = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i > ws {
        *width = spec[ws..i].parse().unwrap_or(0);
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let ps = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        *prec = Some(if i > ps { spec[ps..i].parse().unwrap_or(0) } else { 0 });
    }
}

fn pad(body: &str, width: usize, left: bool, zero: bool) -> String {
    let len = body.chars().count();
    if len >= width {
        return body.to_string();
    }
    let fill = width - len;
    if left {
        format!("{}{}", body, " ".repeat(fill))
    } else if zero {
        format!("{}{}", "0".repeat(fill), body)
    } else {
        format!("{}{}", " ".repeat(fill), body)
    }
}
