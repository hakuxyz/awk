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
    pub convfmt: String,
    pub ofmt: String,
    pub ignorecase: bool,
    pub argind: f64,
    pub argc: f64,
    pub exit_code: Option<i32>,
    pub rng_active: HashMap<usize, bool>,
    pub input: Box<dyn BufRead>,
    pub stdout: Box<dyn Write>,
    pub stderr: Box<dyn Write>,
    // line buffered when stdout is a terminal, block buffered otherwise
    stdout_tty: bool,
    /// one-shot mode: already consumed a line as $0 (for main loop)
    pub pending_line: Option<String>,
    pub match_start: usize,
    pub match_len: usize,
    // set by nextfile: drop the current input file after this record
    nextfile: bool,
    // compiled patterns are reused across records instead of recompiled
    re_cache: RefCell<HashMap<String, Regex>>,
    // ARGV/ARGC were seeded from the command line
    args_seeded: bool,
    rand_seeded: bool,
    rand_seed: f64,
    // open handles for getline < file / | cmd
    getline_files: HashMap<String, BufReader<std::fs::File>>,
    getline_cmds: HashMap<
        String,
        (
            BufReader<std::process::ChildStdout>,
            Option<std::process::Child>,
        ),
    >,
    // cached handles for print > file / | cmd
    print_files: HashMap<String, std::fs::File>,
    print_pipes: HashMap<String, std::process::Child>,
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
            convfmt: "%.6g".into(),
            ofmt: "%.6g".into(),
            ignorecase: false,
            argind: 0.0,
            argc: 1.0,
            args_seeded: false,
            exit_code: None,
            rng_active: HashMap::new(),
            input: Box::new(BufReader::new(std::io::stdin())),
            stdout: Box::new(std::io::BufWriter::with_capacity(
                64 * 1024,
                std::io::stdout(),
            )),
            stderr: Box::new(std::io::stderr()),
            stdout_tty: stdout_is_tty(),
            pending_line: None,
            match_start: 0,
            match_len: 0,
            nextfile: false,
            re_cache: RefCell::new(HashMap::new()),
            rand_seeded: false,
            rand_seed: 1.0,
            getline_files: HashMap::new(),
            getline_cmds: HashMap::new(),
            print_files: HashMap::new(),
            print_pipes: HashMap::new(),
        }
    }

    /// Set a variable from the command line; routes special variables
    /// (CONVFMT, OFMT, IGNORECASE, FS, ...) through the normal assign path.
    pub fn set_var(&mut self, name: &str, v: Value) {
        let _ = self.assign(&Expr::Var(name.to_string()), v);
    }

    /// Run BEGIN, then main loop over input files (paths), then END.
    pub fn run(&mut self, files: &[String]) -> i32 {
        if !self.args_seeded {
            let argv0 = std::env::args().next().unwrap_or_else(|| "awk".into());
            self.set_args(&argv0, files);
        }
        let code = self.run_inner();
        let _ = self.flush_out();
        self.close_all_outputs();
        code
    }

    /// Seed ARGV and ARGC from the command line. ARGV[0] is the basename of
    /// the program name, as gawk reports it.
    pub fn set_args(&mut self, argv0: &str, operands: &[String]) {
        self.args_seeded = true;
        self.argc = (operands.len() + 1) as f64;
        let base = argv0.rsplit('/').next().unwrap_or(argv0);
        let a = self.ensure_array("ARGV");
        let mut a = a.borrow_mut();
        a.insert("0".into(), Value::str(base.to_string()));
        for (i, o) in operands.iter().enumerate() {
            a.insert((i + 1).to_string(), Value::str(o.clone()));
        }
    }

    /// Flush buffered stdout; a closed pipe becomes the BROKEN_PIPE sentinel.
    fn flush_out(&mut self) -> Result<(), String> {
        self.stdout.flush().map_err(|e| {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                BROKEN_PIPE.to_string()
            } else {
                e.to_string()
            }
        })
    }

    /// Report a runtime error; a broken stdout ends the run quietly with 0.
    fn fail(&mut self, e: &str) -> i32 {
        if e == BROKEN_PIPE {
            return 0;
        }
        let _ = self.flush_out();
        let _ = writeln!(self.stderr, "awk: {}", e);
        2
    }

    fn run_inner(&mut self) -> i32 {
        if let Err(e) = self.run_begin() {
            return self.fail(&e);
        }
        if let Some(c) = self.exit_code {
            return c;
        }

        // a program made of BEGIN rules only never touches the input
        let begin_only = self
            .rules
            .iter()
            .all(|r| matches!(r.pattern, Some(Pattern::Begin)));
        if !begin_only && let Some(c) = self.run_input_phase() {
            return c;
        }

        if let Err(e) = self.run_end() {
            return self.fail(&e);
        }
        self.exit_code.unwrap_or(0)
    }

    /// Walk ARGV[1..ARGC): `name=value` operands assign variables, the rest
    /// are files. Operands apply in order while input is read, so BEGIN sees
    /// them unset. With no file operand the standard input is read.
    fn run_input_phase(&mut self) -> Option<i32> {
        let mut read_file = false;
        let mut i = 1i64;
        while (i as f64) < self.argc {
            let key = i.to_string();
            // ARGV is read live: scripts edit it from BEGIN
            let entry = self
                .arrays
                .get("ARGV")
                .and_then(|a| a.borrow().get(&key).cloned());
            i += 1;
            let Some(v) = entry else {
                continue; // index past ARGV or deleted
            };
            let s = self.conv_str(&v);
            if s.is_empty() {
                continue; // empty entries are skipped
            }
            if let Some((name, val)) = operand_assign(&s) {
                let val = Value::Scalar(Scalar::from_input(crate::value::unescape(&val)));
                self.set_var(&name, val);
                continue;
            }
            read_file = true;
            self.argind = (i - 1) as f64;
            self.filename = s.clone();
            self.fnr = 0.0;
            let res = if s == "-" {
                self.run_input()
            } else {
                match std::fs::File::open(&s) {
                    Ok(f) => self.run_stream(f),
                    Err(e) => {
                        let _ = self.flush_out();
                        let _ = writeln!(self.stderr, "awk: cannot open {}: {}", s, e);
                        self.exit_code = Some(2);
                        return Some(2);
                    }
                }
            };
            if let Err(e) = res {
                return Some(self.fail(&e));
            }
            if let Some(c) = self.exit_code {
                return Some(c);
            }
        }
        if !read_file {
            // no file operand: POSIX says read the standard input
            self.argind = 0.0;
            self.filename = "-".into();
            self.fnr = 0.0;
            if let Err(e) = self.run_input() {
                return Some(self.fail(&e));
            }
            if let Some(c) = self.exit_code {
                return Some(c);
            }
        }
        None
    }

    /// Flush and close every file/pipe opened by print redirections.
    fn close_all_outputs(&mut self) {
        for (_, f) in self.print_files.drain() {
            drop(f);
        }
        for (_, mut child) in self.print_pipes.drain() {
            drop(child.stdin.take());
            let _ = child.wait();
        }
        for (_, (reader, child)) in self.getline_cmds.drain() {
            drop(reader);
            if let Some(mut c) = child {
                let _ = c.wait();
            }
        }
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
            if self.nextfile {
                self.nextfile = false;
                break;
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
            if self.nextfile {
                self.nextfile = false;
                break;
            }
        }
        Ok(())
    }

    /// Split $0 into fields per FS and set NF.
    fn set_record(&mut self, rec: &str) {
        // store $0
        self.set_field_str(0, rec);
        let fields = split_fields(rec, &self.fs, self.ignorecase);
        let old_nf = self.nf as usize;
        self.nf = fields.len() as f64;
        for (i, f) in fields.iter().enumerate() {
            self.set_field_str(i + 1, f);
        }
        // drop fields that the previous record left behind
        for i in fields.len() + 1..=old_nf.max(fields.len()) {
            self.globals.remove(&format!("${}", i));
        }
    }

    /// Join the current fields with OFS into $0 (after a field assignment).
    fn rebuild_record(&mut self) {
        let nf = self.nf as usize;
        let mut out = String::new();
        for i in 1..=nf {
            if i > 1 {
                out.push_str(&self.ofs);
            }
            let f = self.get_field(i);
            out.push_str(&self.conv_str(&f));
        }
        self.globals
            .insert("$0".to_string(), Value::Scalar(Scalar::from_input(out)));
    }

    fn set_field_str(&mut self, i: usize, s: &str) {
        let key = format!("${}", i);
        self.globals
            .insert(key, Value::Scalar(Scalar::from_input(s)));
    }

    fn get_field(&self, i: usize) -> Value {
        let key = format!("${}", i);
        self.globals
            .get(&key)
            .cloned()
            .unwrap_or_else(Value::uninit)
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
        if crate::value::is_numlike(s) {
            s.num() != 0.0
        } else {
            !s.as_str().is_empty()
        }
    }

    /// Compile a regex, honoring IGNORECASE. Patterns are cached because
    /// the same source is matched against every record.
    fn compile_re(&self, src: &str) -> Result<Regex, String> {
        let pat = if self.ignorecase {
            format!("(?i){}", src)
        } else {
            src.to_string()
        };
        if let Some(rx) = self.re_cache.borrow().get(&pat) {
            return Ok(rx.clone());
        }
        let rx = Regex::new(&pat).map_err(|e| e.to_string())?;
        let mut cache = self.re_cache.borrow_mut();
        // bound the cache: dynamic patterns must not grow it forever
        if cache.len() >= 512 {
            cache.clear();
        }
        cache.insert(pat, rx.clone());
        Ok(rx)
    }

    fn match_re(&mut self, re_src: &str, s: &str) -> bool {
        let re = match self.compile_re(re_src) {
            Ok(r) => r,
            Err(_) => return false,
        };
        match re.find(s) {
            Some(m) => {
                // gawk counts characters, not bytes
                self.match_start = s[..m.start()].chars().count();
                self.match_len = s[m.start()..m.end()].chars().count();
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
                let mut keys: Vec<String> = arr.borrow().keys().cloned().collect();
                keys.sort();
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
            Stmt::Nextfile => {
                // abandon the rest of the current input file
                self.nextfile = true;
                Ok(Flow::Next)
            }
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
                if self.globals.contains_key(name) {
                    return Err(format!("attempt to delete scalar variable {}", name));
                }
                if let Some(idx) = idx {
                    let k = self.subscript_key(idx)?;
                    let a = self.ensure_array(name);
                    a.borrow_mut().remove(&k);
                } else if let Some(a) = self.arrays.get(name) {
                    a.borrow_mut().clear();
                } else {
                    self.ensure_array(name);
                }
                Ok(Flow::Normal)
            }
            Stmt::Print(args, redir) => {
                let mut out = String::new();
                if args.is_empty() {
                    let v = self.get_field(0);
                    out.push_str(&self.out_str(&v));
                } else {
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            out.push_str(&self.ofs);
                        }
                        let v = self.eval(a)?;
                        out.push_str(&self.out_str(&v));
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
                let out = do_printf(&vals, &self.convfmt)?;
                self.emit(out, redir)?;
                Ok(Flow::Normal)
            }
            Stmt::Getline(_, _) => {
                // getline statements are parsed as expressions
                Ok(Flow::Normal)
            }
        }
    }

    fn emit(&mut self, s: String, redir: &Option<Redir>) -> Result<(), String> {
        match redir {
            None => {
                let res = self.stdout.write_all(s.as_bytes()).and_then(|()| {
                    if self.stdout_tty {
                        self.stdout.flush()
                    } else {
                        Ok(())
                    }
                });
                if let Err(e) = res {
                    if e.kind() == std::io::ErrorKind::BrokenPipe {
                        return Err(BROKEN_PIPE.to_string());
                    }
                    return Err(e.to_string());
                }
                Ok(())
            }
            Some(Redir::Trunc(e)) => {
                let ev = self.eval(e)?;
                let path = self.conv_str(&ev);
                if !self.print_files.contains_key(&path) {
                    let f = std::fs::File::create(&path).map_err(|e| e.to_string())?;
                    self.print_files.insert(path.clone(), f);
                }
                let f = self.print_files.get_mut(&path).unwrap();
                f.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
                Ok(())
            }
            Some(Redir::Append(e)) => {
                let ev = self.eval(e)?;
                let path = self.conv_str(&ev);
                if !self.print_files.contains_key(&path) {
                    use std::fs::OpenOptions;
                    let f = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .map_err(|e| e.to_string())?;
                    self.print_files.insert(path.clone(), f);
                }
                let f = self.print_files.get_mut(&path).unwrap();
                f.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
                Ok(())
            }
            Some(Redir::Pipe(e)) => {
                let ev = self.eval(e)?;
                let cmd = self.conv_str(&ev);
                use std::process::{Command, Stdio};
                if !self.print_pipes.contains_key(&cmd) {
                    let child = Command::new("sh")
                        .arg("-c")
                        .arg(&cmd)
                        .stdin(Stdio::piped())
                        .spawn()
                        .map_err(|e| e.to_string())?;
                    self.print_pipes.insert(cmd.clone(), child);
                }
                let child = self.print_pipes.get_mut(&cmd).unwrap();
                if let Some(stdin) = child.stdin.as_mut() {
                    stdin.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
                }
                Ok(())
            }
        }
    }

    // ----- expressions -----

    fn eval(&mut self, e: &Expr) -> Result<Value, String> {
        match e {
            Expr::Num(n) => Ok(Value::num(*n)),
            Expr::Str(s) => Ok(Value::str(s.clone())),
            Expr::Regex(r) => {
                // a regexp constant used as a value is `$0 ~ /re/`
                let rec = self.get_field(0).to_str();
                Ok(Value::num(if self.match_re(r, &rec) { 1.0 } else { 0.0 }))
            }
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
            // multi subscript: parts joined with SUBSEP
            Expr::Sublist(list) => {
                let mut out = String::new();
                for (i, e) in list.iter().enumerate() {
                    if i > 0 {
                        out.push_str(&self.subsep);
                    }
                    let v = self.eval(e)?;
                    out.push_str(&self.conv_str(&v));
                }
                Ok(Value::str(out))
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
                // any reference to an element creates it, like gawk does
                let mut b = a.borrow_mut();
                if !b.contains_key(&k) {
                    b.insert(k.clone(), Value::uninit());
                }
                let v = b.get(&k).cloned().unwrap_or_else(Value::uninit);
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
                if rhs == 0.0 {
                    return Err("division by zero attempted".into());
                }
                let v = Value::num(cur / rhs);
                self.assign(t, v.clone())?;
                Ok(v)
            }
            Expr::ModAssign(t, r) => {
                let cur = self.eval(t)?.to_num();
                let rhs = self.eval(r)?.to_num();
                if rhs == 0.0 {
                    return Err("division by zero attempted in %".into());
                }
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
                if y == 0.0 {
                    return Err("division by zero attempted".into());
                }
                Ok(Value::num(x / y))
            }
            Expr::Mod(a, b) => {
                let x = self.eval(a)?.to_num();
                let y = self.eval(b)?.to_num();
                if y == 0.0 {
                    return Err("division by zero attempted in %".into());
                }
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
                let s = format!("{}{}", self.conv_str(&x), self.conv_str(&y));
                Ok(Value::str(s))
            }
            Expr::Lt(a, b) => self.cmp_op(a, b, |o| o == std::cmp::Ordering::Less),
            Expr::Le(a, b) => self.cmp_op(a, b, |o| o != std::cmp::Ordering::Greater),
            Expr::Gt(a, b) => self.cmp_op(a, b, |o| o == std::cmp::Ordering::Greater),
            Expr::Ge(a, b) => self.cmp_op(a, b, |o| o != std::cmp::Ordering::Less),
            Expr::Eq(a, b) => self.cmp_op(a, b, |o| o == std::cmp::Ordering::Equal),
            Expr::Ne(a, b) => self.cmp_op(a, b, |o| o != std::cmp::Ordering::Equal),
            Expr::Match(a, b) => {
                let x = self.eval(a)?;
                let s = self.conv_str(&x);
                let re = self.regex_of(b)?;
                Ok(Value::num(if self.match_re(&re, &s) { 1.0 } else { 0.0 }))
            }
            Expr::NoMatch(a, b) => {
                let x = self.eval(a)?;
                let s = self.conv_str(&x);
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
                let iv = self.eval(idx)?;
                let k = self.conv_str(&iv);
                let a = self.ensure_array(arr);
                Ok(Value::num(if a.borrow().contains_key(&k) {
                    1.0
                } else {
                    0.0
                }))
            }
            Expr::Call(name, args) => self.call(name, args),
        }
    }

    /// Format a number with an awk printf format string.
    fn fmt_num(&self, n: f64, fmt: &str) -> String {
        do_printf(&[Value::str(fmt), Value::num(n)], &self.convfmt).unwrap_or_else(|_| int_str(n))
    }

    /// String form in string contexts: integers stay integers, other
    /// numbers convert with CONVFMT.
    fn conv_str(&self, v: &Value) -> String {
        sstr(v, &self.convfmt)
    }

    /// String form for print: integers as integers, other numbers with OFMT.
    fn out_str(&self, v: &Value) -> String {
        if v.is_array() {
            return String::new();
        }
        let s = v.as_scalar();
        if s.uninit {
            return String::new();
        }
        if s.numval {
            let n = s.n;
            if !n.is_finite() {
                return inf_str(n);
            }
            if n == n.trunc() && n.is_finite() {
                if n == 0.0 {
                    "0".to_string()
                } else {
                    format!("{:.0}", n)
                }
            } else {
                self.fmt_num(n, &self.ofmt)
            }
        } else {
            s.s.clone()
        }
    }

    fn cmp_op(
        &mut self,
        a: &Expr,
        b: &Expr,
        f: impl Fn(std::cmp::Ordering) -> bool,
    ) -> Result<Value, String> {
        let mut x = self.eval(a)?;
        let mut y = self.eval(b)?;
        if x.is_array() || y.is_array() {
            return Err("comparison with array".into());
        }
        // number against string: the number converts with CONVFMT first
        if !x.as_scalar().uninit && !y.as_scalar().uninit {
            let xn = crate::value::is_numlike(x.as_scalar());
            let yn = crate::value::is_numlike(y.as_scalar());
            if xn && !yn {
                let s = self.conv_str(&x);
                x = Value::str(s);
            } else if yn && !xn {
                let s = self.conv_str(&y);
                y = Value::str(s);
            }
        }
        let ord = crate::value::cmp_scalar(x.as_scalar(), y.as_scalar());
        Ok(Value::num(if f(ord) { 1.0 } else { 0.0 }))
    }

    fn regex_of(&mut self, e: &Expr) -> Result<String, String> {
        match e {
            Expr::Regex(r) | Expr::Str(r) => Ok(r.clone()),
            other => {
                let v = self.eval(other)?;
                Ok(self.conv_str(&v))
            }
        }
    }

    fn subscript_key(&mut self, e: &Expr) -> Result<String, String> {
        let v = self.eval(e)?;
        if v.is_array() {
            return Err("array subscript".into());
        }
        Ok(self.conv_str(&v))
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
        if name == "length" {
            // POSIX allows length without parentheses
            return self.call("length", &[]);
        }
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
            "CONVFMT" => return Ok(Value::str(self.convfmt.clone())),
            "OFMT" => return Ok(Value::str(self.ofmt.clone())),
            "IGNORECASE" => {
                if let Some(v) = self.globals.get("IGNORECASE") {
                    return Ok(v.clone());
                }
                return Ok(Value::num(0.0));
            }
            "ARGIND" => return Ok(Value::num(self.argind)),
            "ARGC" => return Ok(Value::num(self.argc)),
            "ARGV" => return Ok(Value::Array(self.ensure_array("ARGV"))),
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
                    let old = self.nf as usize;
                    let n = if v.to_num() < 0.0 {
                        0
                    } else {
                        v.to_num() as usize
                    };
                    self.nf = n as f64;
                    for i in n + 1..=old.max(n) {
                        self.globals.remove(&format!("${}", i));
                    }
                    self.rebuild_record();
                }
                "FNR" => self.fnr = v.to_num(),
                "FS" => self.fs = self.conv_str(&v),
                "OFS" => self.ofs = self.conv_str(&v),
                "ORS" => self.ors = self.conv_str(&v),
                "FILENAME" => self.filename = self.conv_str(&v),
                "SUBSEP" => self.subsep = self.conv_str(&v),
                "RSTART" => self.match_start = (v.to_num() - 1.0) as usize,
                "RLENGTH" => self.match_len = v.to_num() as usize,
                "CONVFMT" => self.convfmt = self.conv_str(&v),
                "OFMT" => self.ofmt = self.conv_str(&v),
                "IGNORECASE" => {
                    let s = self.conv_str(&v);
                    let t = s.trim();
                    self.ignorecase =
                        !t.is_empty() && t.parse::<f64>().map(|n| n != 0.0).unwrap_or(true);
                    self.globals.insert(name.clone(), v);
                }
                "ARGIND" => self.argind = v.to_num(),
                "ARGC" => self.argc = v.to_num(),
                _ => {
                    if v.is_array() {
                        self.arrays.insert(
                            name.clone(),
                            match v {
                                Value::Array(a) => a,
                                _ => unreachable!(),
                            },
                        );
                    } else {
                        self.arrays.remove(name);
                        self.globals.insert(name.clone(), v);
                    }
                }
            },
            Expr::Field(idx) => {
                let i = self.eval(idx)?.to_num() as usize;
                if i == 0 {
                    let s = self.conv_str(&v);
                    self.set_record(&s);
                } else {
                    if v.is_array() {
                        return Err("array in field assign".into());
                    }
                    self.globals.insert(format!("${}", i), v);
                    if (i as f64) > self.nf {
                        self.nf = i as f64;
                    }
                    self.rebuild_record();
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
                    // nested not supported as value - replace element
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
            // extra arguments are ignored, missing ones stay uninitialized
            let mut vals = Vec::new();
            for a in args {
                vals.push(self.eval(a)?);
            }
            // local scope: save globals for params
            let mut saved: Vec<(String, Option<Value>)> = Vec::new();
            for (i, p) in fd.params.iter().enumerate() {
                let v = vals.get(i).cloned().unwrap_or_else(Value::uninit);
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
                    let v = self.get_field(0);
                    return Ok(Value::num(self.conv_str(&v).chars().count() as f64));
                }
                let v = self.eval(&args[0])?;
                if let Value::Array(a) = v {
                    return Ok(Value::num(a.borrow().len() as f64));
                }
                Ok(Value::num(self.conv_str(&v).chars().count() as f64))
            }
            "substr" => {
                if args.len() < 2 || args.len() > 3 {
                    return Err(format!(
                        "substr: expected 2 or 3 arguments, got {}",
                        args.len()
                    ));
                }
                let v = self.eval(&args[0])?;
                let s = self.conv_str(&v);
                let start = self.eval(&args[1])?.to_num() as i64;
                let len = if args.len() == 3 {
                    Some(self.eval(&args[2])?.to_num() as i64)
                } else {
                    None
                };
                let chars: Vec<char> = s.chars().collect();
                let n = chars.len() as i64;
                // a start below 1 counts from the beginning
                let from = (start - 1).max(0);
                let to = match len {
                    Some(l) if l >= 0 => (from + l).min(n),
                    Some(_) => from,
                    None => n,
                };
                if from >= n || to <= from {
                    return Ok(Value::str(""));
                }
                let out: String = chars[from as usize..to as usize].iter().collect();
                Ok(Value::str(out))
            }
            "index" => {
                let v = self.eval(&args[0])?;
                let t = self.eval(&args[1])?;
                let s = self.conv_str(&v);
                let t = self.conv_str(&t);
                if t.is_empty() {
                    return Ok(Value::num(1.0));
                }
                match s.find(&t) {
                    Some(i) => Ok(Value::num(s[..i].chars().count() as f64 + 1.0)),
                    None => Ok(Value::num(0.0)),
                }
            }
            "split" => {
                if args.len() < 2 || args.len() > 3 {
                    return Err(format!(
                        "split: expected 2 or 3 arguments, got {}",
                        args.len()
                    ));
                }
                let v = self.eval(&args[0])?;
                let s = self.conv_str(&v);
                let arr_name = match &args[1] {
                    Expr::Var(n) => n.clone(),
                    _ => return Err("split: second arg must be array".into()),
                };
                let fs = if args.len() >= 3 {
                    // a regexp constant keeps its pattern, not its match result
                    self.regex_of(&args[2])?
                } else {
                    self.fs.clone()
                };
                let a = self.ensure_array(&arr_name);
                a.borrow_mut().clear();
                let parts = split_fields(&s, &fs, self.ignorecase);
                for (i, p) in parts.iter().enumerate() {
                    a.borrow_mut().insert(
                        (i + 1).to_string(),
                        Value::Scalar(Scalar::from_input(p.clone())),
                    );
                }
                Ok(Value::num(parts.len() as f64))
            }
            "sub" | "gsub" => {
                let re_src = self.regex_of(&args[0])?;
                let r = self.eval(&args[1])?;
                let repl = self.conv_str(&r);
                let target = args
                    .get(2)
                    .cloned()
                    .unwrap_or_else(|| Expr::Field(Box::new(Expr::Num(0.0))));
                let tv = self.eval(&target)?;
                let s = self.conv_str(&tv);
                let re = self.compile_re(&re_src)?;
                let mut count = 0usize;
                let out = if name == "gsub" {
                    re.replace_all(&s, |caps: &regex::Captures| {
                        count += 1;
                        expand_repl(&repl, caps, false)
                    })
                    .into_owned()
                } else {
                    re.replace(&s, |caps: &regex::Captures| {
                        count += 1;
                        expand_repl(&repl, caps, false)
                    })
                    .into_owned()
                };
                // gawk silently ignores a non-assignable third argument
                if count > 0 && is_lvalue(&target) {
                    self.assign(&target, Value::str(out))?;
                }
                Ok(Value::num(count as f64))
            }
            "gensub" => self.builtin_gensub(args),
            "sprintf" => {
                let vals: Result<Vec<_>, _> = args.iter().map(|a| self.eval(a)).collect();
                let vals = vals?;
                Ok(Value::str(do_printf(&vals, &self.convfmt)?))
            }
            "sin" | "cos" | "atan2" | "log" | "exp" | "sqrt" | "int" => {
                let x = self.eval(&args[0])?.to_num();
                let y = if args.len() > 1 {
                    self.eval(&args[1])?.to_num()
                } else {
                    0.0
                };
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
                use std::time::{SystemTime, UNIX_EPOCH};
                // xorshift64 state kept in a global
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
                self.globals
                    .insert("__rand_state".to_string(), Value::num(state as f64));
                let r = (state >> 11) as f64 / (1u64 << 53) as f64;
                Ok(Value::num(r))
            }
            "srand" => {
                let old = self.rand_seed;
                let seed = if args.is_empty() {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs() as f64)
                        .unwrap_or(0.0)
                } else {
                    self.eval(&args[0])?.to_num()
                };
                self.rand_seed = seed;
                self.globals
                    .insert("__rand_state".to_string(), Value::num(seed));
                self.rand_seeded = true;
                Ok(Value::num(old))
            }
            "tolower" => {
                let v = self.eval(&args[0])?;
                Ok(Value::str(self.conv_str(&v).to_lowercase()))
            }
            "toupper" => {
                let v = self.eval(&args[0])?;
                Ok(Value::str(self.conv_str(&v).to_uppercase()))
            }
            "match" => {
                let v = self.eval(&args[0])?;
                let s = self.conv_str(&v);
                let re = self.regex_of(&args[1])?;
                if self.match_re(&re, &s) {
                    Ok(Value::num(self.match_start as f64 + 1.0))
                } else {
                    self.match_start = 0;
                    self.match_len = 0;
                    Ok(Value::num(0.0))
                }
            }
            "close" => {
                if args.len() != 1 {
                    return Err(format!("close: expected 1 argument, got {}", args.len()));
                }
                let v = self.eval(&args[0])?;
                let key = self.conv_str(&v);
                if let Some(mut child) = self.print_pipes.remove(&key) {
                    drop(child.stdin.take());
                    let code = child.wait().map(|s| s.code().unwrap_or(0)).unwrap_or(-1);
                    return Ok(Value::num(code as f64));
                }
                if let Some((reader, child)) = self.getline_cmds.remove(&key) {
                    drop(reader);
                    let code = match child {
                        Some(mut c) => c.wait().map(|s| s.code().unwrap_or(0)).unwrap_or(-1),
                        None => 0,
                    };
                    return Ok(Value::num(code as f64));
                }
                if self.print_files.remove(&key).is_some()
                    || self.getline_files.remove(&key).is_some()
                {
                    return Ok(Value::num(0.0));
                }
                Ok(Value::num(-1.0))
            }
            "system" => {
                let v = self.eval(&args[0])?;
                let cmd = self.conv_str(&v);
                let _ = self.flush_out();
                use std::process::Command;
                let st = Command::new("sh").arg("-c").arg(&cmd).status();
                Ok(Value::num(match st {
                    Ok(s) => s.code().unwrap_or(0) as f64,
                    Err(_) => -1.0,
                }))
            }
            "asort" | "asorti" => self.builtin_asort(name, args),
            "and" | "or" | "xor" | "lshift" | "rshift" | "compl" => {
                if name == "compl" {
                    if args.len() != 1 {
                        return Err(format!("compl: expected 1 argument, got {}", args.len()));
                    }
                    let x = self.eval(&args[0])?.to_num();
                    if x < 0.0 {
                        return Err("compl: negative value not allowed".into());
                    }
                    let a = x as u64;
                    let t = (a + 1).trailing_zeros();
                    let k = (53 + t).min(63);
                    let r = ((1u64 << k) - 1u64).wrapping_sub(a) as i64;
                    return Ok(Value::num(r as f64));
                }
                if args.len() != 2 {
                    return Err(format!(
                        "{}: expected 2 arguments, got {}",
                        name,
                        args.len()
                    ));
                }
                let x = self.eval(&args[0])?.to_num();
                let y = self.eval(&args[1])?.to_num();
                if (name == "rshift" || name == "lshift") && (x < 0.0 || y < 0.0) {
                    return Err(format!("{}: negative value not allowed", name));
                }
                let a = x as i64;
                let b = y as i64;
                let r = match name {
                    "and" => a & b,
                    "or" => a | b,
                    "xor" => a ^ b,
                    "lshift" => {
                        if b >= 64 {
                            0
                        } else {
                            a << b
                        }
                    }
                    "rshift" => {
                        if b >= 64 {
                            0
                        } else {
                            a >> b
                        }
                    }
                    _ => unreachable!(),
                };
                Ok(Value::num(r as f64))
            }
            "strtonum" => {
                if args.len() != 1 {
                    return Err(format!("strtonum: expected 1 argument, got {}", args.len()));
                }
                let v = self.eval(&args[0])?;
                let s = self.conv_str(&v);
                Ok(Value::num(parse_strtonum(&s)))
            }
            "typeof" => {
                if args.len() != 1 {
                    return Err(format!("typeof: expected 1 argument, got {}", args.len()));
                }
                // a subscript that already holds a slot reports "unassigned"
                let existed = match &args[0] {
                    Expr::Index(arr, sub) => match &**arr {
                        Expr::Var(n) if self.arrays.contains_key(n) => {
                            let k = self.subscript_key(sub).unwrap_or_default();
                            self.arrays
                                .get(n)
                                .is_some_and(|a| a.borrow().contains_key(&k))
                        }
                        _ => false,
                    },
                    _ => false,
                };
                let v = self.eval(&args[0])?;
                let t = match &v {
                    Value::Array(_) => "array",
                    Value::Scalar(sc) if sc.uninit => match &args[0] {
                        Expr::Field(_) => "unassigned",
                        Expr::Index(..) if existed => "unassigned",
                        _ => "untyped",
                    },
                    Value::Scalar(sc) if sc.numval => "number",
                    Value::Scalar(sc) if sc.strnum => "strnum",
                    Value::Scalar(_) => "string",
                };
                Ok(Value::str(t))
            }
            "isarray" => {
                if args.len() != 1 {
                    return Err(format!("isarray: expected 1 argument, got {}", args.len()));
                }
                let v = self.eval(&args[0])?;
                Ok(Value::num(if v.is_array() { 1.0 } else { 0.0 }))
            }
            "systime" => {
                use std::time::{SystemTime, UNIX_EPOCH};
                let t = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as f64)
                    .unwrap_or(0.0);
                Ok(Value::num(t))
            }
            "strftime" => {
                if args.is_empty() || args.len() > 3 {
                    return Err(format!(
                        "strftime: expected 1 to 3 arguments, got {}",
                        args.len()
                    ));
                }
                let f = self.eval(&args[0])?;
                let fmt = self.conv_str(&f);
                let ts = if args.len() > 1 {
                    self.eval(&args[1])?.to_num() as i64
                } else {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0)
                };
                let utc = args.len() > 2 && self.eval(&args[2])?.to_num() != 0.0;
                Ok(Value::str(strftime(&fmt, ts, utc)))
            }
            "mktime" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(format!(
                        "mktime: expected 1 or 2 arguments, got {}",
                        args.len()
                    ));
                }
                let v = self.eval(&args[0])?;
                let spec = self.conv_str(&v);
                let utc = args.len() > 1 && self.eval(&args[1])?.to_num() != 0.0;
                Ok(Value::num(mktime(&spec, utc)))
            }
            "getline" => self.builtin_getline(args),
            other => Err(format!("unknown function {}", other)),
        }
    }

    /// getline [var] [< file | | cmd] - returns 1 ok, 0 EOF, -1 error.
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
                let fv = self.eval(&src)?;
                let path = self.conv_str(&fv);
                if !self.getline_files.contains_key(&path) {
                    match std::fs::File::open(&path) {
                        Ok(f) => {
                            self.getline_files.insert(path.clone(), BufReader::new(f));
                        }
                        Err(_) => return Ok(Value::num(-1.0)),
                    }
                }
                let mut line = String::new();
                let r = self.getline_files.get_mut(&path).unwrap();
                let n = r.read_line(&mut line).map_err(|e| e.to_string())?;
                if n == 0 {
                    // keep the handle: reads stay at 0 until close() reopens it
                    None
                } else {
                    while line.ends_with('\n') || line.ends_with('\r') {
                        line.pop();
                    }
                    Some(line)
                }
            }
            "cmd" => {
                let cv = self.eval(&src)?;
                let cmd = self.conv_str(&cv);
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
                    self.getline_cmds
                        .insert(cmd.clone(), (BufReader::new(stdout), Some(child)));
                }
                let mut line = String::new();
                let entry = self.getline_cmds.get_mut(&cmd).unwrap();
                let n = entry.0.read_line(&mut line).map_err(|e| e.to_string())?;
                if n == 0 {
                    // reap the child once, but keep the handle parked at EOF
                    if let Some(mut child) = entry.1.take() {
                        let _ = child.wait();
                    }
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

    /// gensub(re, repl, how, [target]) returns the substituted string.
    fn builtin_gensub(&mut self, args: &[Expr]) -> Result<Value, String> {
        if args.len() < 3 || args.len() > 4 {
            return Err(format!(
                "gensub: expected 3 or 4 arguments, got {}",
                args.len()
            ));
        }
        let re_src = self.regex_of(&args[0])?;
        let rv = self.eval(&args[1])?;
        let repl = self.conv_str(&rv);
        let hv = self.eval(&args[2])?;
        let how = self.conv_str(&hv);
        let target = args
            .get(3)
            .cloned()
            .unwrap_or_else(|| Expr::Field(Box::new(Expr::Num(0.0))));
        let tv = self.eval(&target)?;
        let s = self.conv_str(&tv);
        let re = self.compile_re(&re_src)?;
        let out = if how == "g" {
            re.replace_all(&s, |caps: &regex::Captures| expand_repl(&repl, caps, true))
                .into_owned()
        } else {
            // anything else is the n-th match, invalid or < 1 means the first
            let n: usize = how
                .parse::<f64>()
                .map(|v| if v < 1.0 { 1 } else { v as usize })
                .unwrap_or(1);
            let mut out = s.clone();
            let mut seen = 0usize;
            for caps in re.captures_iter(&s) {
                seen += 1;
                if seen == n {
                    if let Some(m) = caps.get(0) {
                        let rep = expand_repl(&repl, &caps, true);
                        out.replace_range(m.start()..m.end(), &rep);
                    }
                    break;
                }
            }
            out
        };
        Ok(Value::str(out))
    }

    /// asort(arr[, dest]) sorts values, asorti sorts indices (as strings).
    fn builtin_asort(&mut self, name: &str, args: &[Expr]) -> Result<Value, String> {
        if args.is_empty() || args.len() > 2 {
            return Err(format!(
                "{}: expected 1 or 2 arguments, got {}",
                name,
                args.len()
            ));
        }
        let src_name = match &args[0] {
            Expr::Var(n) => n.clone(),
            _ => return Err(format!("{}: first argument must be an array", name)),
        };
        let dst_name = if args.len() == 2 {
            match &args[1] {
                Expr::Var(n) => n.clone(),
                _ => return Err(format!("{}: second argument must be an array", name)),
            }
        } else {
            src_name.clone()
        };
        let src = self.ensure_array(&src_name);
        let mut entries: Vec<(String, Value)> = src
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let n = entries.len();
        if name == "asort" {
            entries.sort_by(|a, b| crate::value::cmp_scalar(a.1.as_scalar(), b.1.as_scalar()));
        } else {
            // asorti: the index strings become the values of the result
            entries.sort_by(|a, b| a.0.cmp(&b.0));
        }
        let dst = self.ensure_array(&dst_name);
        dst.borrow_mut().clear();
        for (i, (k, v)) in entries.into_iter().enumerate() {
            let out = if name == "asort" { v } else { Value::str(k) };
            dst.borrow_mut().insert((i + 1).to_string(), out);
        }
        Ok(Value::num(n as f64))
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

/// Sentinel error: stdout went away (broken pipe). Reported as a silent
/// successful exit, the way every Unix filter behaves when piped into `head`.
const BROKEN_PIPE: &str = "\u{1}broken-pipe\u{1}";

/// True when stdout is a terminal, so output must be flushed per record.
fn stdout_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

/// Expand a replacement string: & is the match, \& a literal ampersand.
/// With groups enabled \1..\9 are capture references, otherwise they stay
/// literal (sub/gsub keep them, gensub resolves them).
fn expand_repl(repl: &str, caps: &regex::Captures, groups: bool) -> String {
    let whole = caps.get(0).map(|m| m.as_str()).unwrap_or("");
    let mut out = String::new();
    let b = repl.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            let n = b[i + 1];
            if n == b'&' {
                out.push('&');
                i += 2;
                continue;
            }
            if n == b'\\' {
                out.push('\\');
                i += 2;
                continue;
            }
            if groups && n.is_ascii_digit() {
                if let Some(m) = caps.get((n - b'0') as usize) {
                    out.push_str(m.as_str());
                }
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
        let ch_len = utf8_len(b[i]);
        out.push_str(std::str::from_utf8(&b[i..i + ch_len]).unwrap_or("?"));
        i += ch_len;
    }
    out
}

/// Targets that gsub/sub may write back to.
fn is_lvalue(e: &Expr) -> bool {
    matches!(e, Expr::Var(_) | Expr::Field(_) | Expr::Index(..))
}

/// Split a `name=value` operand. Only a plain identifier makes it an
/// assignment; `a[1]=5`, `1x=5` and `=x=7` are file names.
fn operand_assign(s: &str) -> Option<(String, String)> {
    let b = s.as_bytes();
    if b.is_empty() || !(b[0].is_ascii_alphabetic() || b[0] == b'_') {
        return None;
    }
    let mut i = 1;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
        i += 1;
    }
    if i >= b.len() || b[i] != b'=' {
        return None;
    }
    Some((s[..i].to_string(), s[i + 1..].to_string()))
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

// Compiled field separators, keyed by pattern text. FS is applied to every
// record, so recompiling it each time dominates runtime scripts.
thread_local! {
    static SPLIT_RE_CACHE: RefCell<HashMap<String, Option<Regex>>> =
        RefCell::new(HashMap::new());
}

/// Split a record into fields per FS. An empty record yields no fields,
/// FS="" splits into single characters and FS=" " strips whitespace runs.
fn split_fields(rec: &str, fs: &str, ignorecase: bool) -> Vec<String> {
    if rec.is_empty() {
        return Vec::new();
    }
    if fs.is_empty() {
        return rec.chars().map(|c| c.to_string()).collect();
    }
    if fs == " " {
        return rec.split_whitespace().map(|s| s.to_string()).collect();
    }
    if fs.len() >= 2 && fs.starts_with('/') && fs.ends_with('/') {
        let src = &fs[1..fs.len() - 1];
        return split_ere(rec, src, ignorecase);
    }
    if fs.chars().count() == 1 {
        return rec.split(fs).map(|s| s.to_string()).collect();
    }
    // multi-character FS is an ERE
    split_ere(rec, fs, ignorecase)
}

/// Split with an ERE separator, reusing a compiled pattern from the cache.
fn split_ere(rec: &str, src: &str, ignorecase: bool) -> Vec<String> {
    let pat = if ignorecase {
        format!("(?i){}", src)
    } else {
        src.to_string()
    };
    SPLIT_RE_CACHE.with(|c| {
        if let Some(Some(rx)) = c.borrow().get(&pat) {
            return rx.split(rec).map(|s| s.to_string()).collect();
        }
        let built = Regex::new(&pat).ok();
        let mut cache = c.borrow_mut();
        if cache.len() >= 512 {
            cache.clear();
        }
        cache.insert(pat, built.clone());
        match built {
            Some(rx) => rx.split(rec).map(|s| s.to_string()).collect(),
            None => vec![rec.to_string()],
        }
    })
}

// Guard against CONVFMT values that would convert a number again.
thread_local! {
    static FMT_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Decimal form of an integral value (awk prints integers full length).
fn int_str(n: f64) -> String {
    if !n.is_finite() {
        return inf_str(n);
    }
    if n == 0.0 {
        "0".to_string()
    } else {
        format!("{:.0}", n)
    }
}

/// gawk prints non-finite values with an explicit sign: +inf, -inf, -nan.
fn inf_str(n: f64) -> String {
    let sign = if n.is_sign_negative() { "-" } else { "+" };
    format!("{}{}", sign, if n.is_nan() { "nan" } else { "inf" })
}

/// String form used by %s and by number-to-string conversion: integers stay
/// integers, other numbers convert with CONVFMT.
fn sstr(v: &Value, convfmt: &str) -> String {
    match v {
        Value::Scalar(s) => {
            if s.uninit {
                String::new()
            } else if s.numval {
                let n = s.n;
                if !n.is_finite() {
                    inf_str(n)
                } else if n == n.trunc() && n.is_finite() {
                    int_str(n)
                } else {
                    FMT_DEPTH.with(|d| {
                        let depth = d.get();
                        if depth > 2 {
                            return fmt_g(n, 6, false, false);
                        }
                        d.set(depth + 1);
                        let r = do_printf(&[Value::str(convfmt), Value::num(n)], convfmt)
                            .unwrap_or_else(|_| fmt_g(n, 6, false, false));
                        d.set(depth);
                        r
                    })
                }
            } else {
                s.s.clone()
            }
        }
        Value::Array(_) => String::new(),
    }
}

/// printf family formatting. Returns Err when the format string needs more
/// arguments than were supplied (gawk also treats that as fatal).
fn do_printf(args: &[Value], convfmt: &str) -> Result<String, String> {
    if args.is_empty() {
        return Ok(String::new());
    }
    let fmt = sstr(&args[0], convfmt);
    let fb = fmt.as_bytes();
    let mut out = String::new();
    let mut fi = 0;
    let mut ai = 1;
    while fi < fb.len() {
        if fb[fi] != b'%' {
            let l = utf8_len(fb[fi]);
            out.push_str(std::str::from_utf8(&fb[fi..fi + l]).unwrap_or(""));
            fi += l;
            continue;
        }
        let start = fi;
        fi += 1;
        if fi < fb.len() && fb[fi] == b'%' {
            out.push('%');
            fi += 1;
            continue;
        }
        let (mut left, mut plus, mut space, mut alt, mut zero) =
            (false, false, false, false, false);
        while fi < fb.len() {
            match fb[fi] {
                b'-' => left = true,
                b'+' => plus = true,
                b' ' => space = true,
                b'#' => alt = true,
                b'0' => zero = true,
                _ => break,
            }
            fi += 1;
        }
        // field width, optionally from an argument
        let mut width: i64 = 0;
        if fi < fb.len() && fb[fi] == b'*' {
            fi += 1;
            if ai >= args.len() {
                return Err("not enough arguments for format string".into());
            }
            width = args[ai].to_num() as i64;
            ai += 1;
            if width < 0 {
                left = true;
                width = -width;
            }
        } else {
            let ws = fi;
            while fi < fb.len() && fb[fi].is_ascii_digit() {
                fi += 1;
            }
            if fi > ws {
                width = fmt[ws..fi].parse().unwrap_or(0);
            }
        }
        // precision, optionally from an argument
        let mut prec: Option<usize> = None;
        if fi < fb.len() && fb[fi] == b'.' {
            fi += 1;
            if fi < fb.len() && fb[fi] == b'*' {
                fi += 1;
                if ai >= args.len() {
                    return Err("not enough arguments for format string".into());
                }
                let p = args[ai].to_num() as i64;
                ai += 1;
                if p >= 0 {
                    prec = Some(p as usize);
                }
            } else {
                let ps = fi;
                while fi < fb.len() && fb[fi].is_ascii_digit() {
                    fi += 1;
                }
                prec = Some(if fi > ps {
                    fmt[ps..fi].parse().unwrap_or(0)
                } else {
                    0
                });
            }
        }
        // length modifiers are accepted and ignored
        while fi < fb.len() && matches!(fb[fi], b'h' | b'l' | b'L' | b'z' | b't' | b'j') {
            fi += 1;
        }
        if fi >= fb.len() {
            out.push_str(&fmt[start..]);
            break;
        }
        let conv = fb[fi];
        fi += 1;
        if !matches!(
            conv,
            b'd' | b'i'
                | b'u'
                | b'x'
                | b'X'
                | b'o'
                | b'f'
                | b'F'
                | b'e'
                | b'E'
                | b'g'
                | b'G'
                | b'a'
                | b'A'
                | b'c'
                | b's'
        ) {
            // unknown conversion: copy verbatim, no argument is consumed
            out.push_str(&fmt[start..fi]);
            continue;
        }
        if ai >= args.len() {
            return Err("not enough arguments for format string".into());
        }
        let val = &args[ai];
        ai += 1;
        let piece = format_conv(
            conv,
            val,
            width as usize,
            left,
            zero,
            plus,
            space,
            alt,
            prec,
            convfmt,
        );
        out.push_str(&piece);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn format_conv(
    conv: u8,
    val: &Value,
    width: usize,
    left: bool,
    zero: bool,
    plus: bool,
    space: bool,
    alt: bool,
    prec: Option<usize>,
    convfmt: &str,
) -> String {
    if matches!(
        conv,
        b'd' | b'i'
            | b'u'
            | b'x'
            | b'X'
            | b'o'
            | b'f'
            | b'F'
            | b'e'
            | b'E'
            | b'g'
            | b'G'
            | b'a'
            | b'A'
    ) {
        let n = val.to_num();
        if !n.is_finite() {
            return pad_num("", &inf_str(n), width, left, zero);
        }
    }
    match conv {
        b'd' | b'i' => {
            let v = val.to_num() as i64;
            let zero = zero && prec.is_none();
            let body = match prec {
                Some(p) => pad_prec(&v.unsigned_abs().to_string(), p),
                None => v.unsigned_abs().to_string(),
            };
            let sign = if v < 0 {
                "-"
            } else if plus {
                "+"
            } else if space {
                " "
            } else {
                ""
            };
            pad_num(sign, &body, width, left, zero)
        }
        b'u' => {
            let v = val.to_num() as i64 as u64;
            let zero = zero && prec.is_none();
            let body = match prec {
                Some(p) => pad_prec(&v.to_string(), p),
                None => v.to_string(),
            };
            pad_num("", &body, width, left, zero)
        }
        b'x' | b'X' | b'o' => {
            let v = val.to_num() as i64 as u64;
            let zero = zero && prec.is_none();
            let mut body = if conv == b'x' {
                format!("{:x}", v)
            } else if conv == b'X' {
                format!("{:X}", v)
            } else {
                format!("{:o}", v)
            };
            if let Some(p) = prec {
                body = pad_prec(&body, p);
            }
            if alt && v != 0 && !body.starts_with('0') {
                if conv == b'o' {
                    body.insert(0, '0');
                } else if conv == b'x' {
                    body.insert_str(0, "0x");
                } else {
                    body.insert_str(0, "0X");
                }
            }
            pad_num("", &body, width, left, zero)
        }
        b'f' | b'F' => {
            let n = val.to_num();
            let p = prec.unwrap_or(6);
            let mut body = format!("{:.*}", p, n.abs());
            if alt && !body.contains('.') {
                body.push('.');
            }
            pad_num(&float_sign(n, plus, space), &body, width, left, zero)
        }
        b'e' | b'E' => {
            let n = val.to_num();
            let p = prec.unwrap_or(6);
            let mut body = fmt_e(n.abs(), p, conv == b'E');
            if alt {
                body = force_dot_e(body, conv == b'E');
            }
            pad_num(&float_sign(n, plus, space), &body, width, left, zero)
        }
        b'g' | b'G' => {
            let n = val.to_num();
            let p = prec.unwrap_or(6);
            let body = fmt_g(n.abs(), p, conv == b'G', alt);
            pad_num(&float_sign(n, plus, space), &body, width, left, zero)
        }
        b'a' | b'A' => {
            let body = fmt_a(val.to_num(), prec, conv == b'A');
            pad_num("", &body, width, left, zero)
        }
        b'c' => {
            let ch = match val {
                Value::Scalar(s) if s.numval => char::from_u32(s.n as u32).unwrap_or('\0'),
                _ => sstr(val, convfmt).chars().next().unwrap_or('\0'),
            };
            let body = ch.to_string();
            pad_num("", &body, width, left, zero)
        }
        b's' => {
            let mut body = sstr(val, convfmt);
            if let Some(p) = prec {
                let chars: Vec<char> = body.chars().collect();
                if chars.len() > p {
                    body = chars[..p].iter().collect();
                }
            }
            pad_num("", &body, width, left, false)
        }
        _ => String::new(),
    }
}

/// Sign prefix for the floating conversions (handles negative zero).
fn float_sign(n: f64, plus: bool, space: bool) -> String {
    if n.is_sign_negative() {
        "-".to_string()
    } else if plus {
        "+".to_string()
    } else if space {
        " ".to_string()
    } else {
        String::new()
    }
}

/// %e with a sign and at least two exponent digits: 1.234500e+03.
fn fmt_e(n: f64, p: usize, upper: bool) -> String {
    let s = if upper {
        format!("{:.*E}", p, n)
    } else {
        format!("{:.*e}", p, n)
    };
    let sep = if upper { 'E' } else { 'e' };
    match s.find(sep) {
        Some(epos) => {
            let (mant, exps) = s.split_at(epos);
            let exp: i32 = exps[1..].parse().unwrap_or(0);
            format!(
                "{}{}{}{:02}",
                mant,
                sep,
                if exp < 0 { "-" } else { "+" },
                exp.unsigned_abs()
            )
        }
        None => s,
    }
}

/// Remove trailing zeros from the mantissa of an %e style number.
fn strip_e_zeros(s: String, upper: bool) -> String {
    let sep = if upper { 'E' } else { 'e' };
    let Some(epos) = s.find(sep) else { return s };
    let mut mant = s[..epos].to_string();
    if mant.contains('.') {
        while mant.ends_with('0') {
            mant.pop();
        }
        if mant.ends_with('.') {
            mant.pop();
        }
    }
    format!("{}{}", mant, &s[epos..])
}

/// %#: always keep a decimal point in the mantissa.
fn force_dot_e(s: String, upper: bool) -> String {
    let sep = if upper { 'E' } else { 'e' };
    let Some(epos) = s.find(sep) else { return s };
    let mut mant = s[..epos].to_string();
    if !mant.contains('.') {
        mant.push('.');
    }
    format!("{}{}", mant, &s[epos..])
}

/// %g: significant digits, style e when the exponent is out of range.
fn fmt_g(n: f64, p: usize, upper: bool, alt: bool) -> String {
    if !n.is_finite() {
        return if upper {
            format!("{:E}", n)
        } else {
            format!("{:e}", n)
        };
    }
    let p = if p == 0 { 1 } else { p };
    let es = format!("{:e}", n);
    let exp: i32 = es
        .split('e')
        .nth(1)
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);
    if exp < -4 || exp >= p as i32 {
        let s = fmt_e(n, p - 1, upper);
        if alt {
            force_dot_e(s, upper)
        } else {
            strip_e_zeros(s, upper)
        }
    } else {
        let dp = (p as i32 - 1 - exp) as usize;
        let mut body = format!("{:.*}", dp, n);
        if !alt {
            if body.contains('.') {
                while body.ends_with('0') {
                    body.pop();
                }
                if body.ends_with('.') {
                    body.pop();
                }
            }
        } else if !body.contains('.') {
            body.push('.');
        }
        body
    }
}

/// %a: IEEE hex float such as 0x1.999999999999ap-4.
fn fmt_a(n: f64, prec: Option<usize>, upper: bool) -> String {
    if n.is_nan() || n.is_infinite() {
        return if upper {
            format!("{:E}", n)
        } else {
            format!("{:e}", n)
        };
    }
    let neg = n.is_sign_negative();
    let a = n.abs();
    let (mut e, digits) = if a == 0.0 {
        (0i32, String::new())
    } else {
        // m * 2^e with m in [1, 2)
        let mut e = 0i32;
        let mut m = a;
        while m >= 2.0 {
            m /= 2.0;
            e += 1;
        }
        while m < 1.0 {
            m *= 2.0;
            e -= 1;
        }
        let mut frac = m - 1.0;
        let mut digits = String::new();
        // 13 hex digits cover the 52 bit mantissa exactly
        let limit = prec.unwrap_or(13).max(1);
        let mut count = 0;
        while frac > 0.0 && count < limit + 1 {
            frac *= 16.0;
            let d = frac.floor();
            digits.push(std::char::from_digit(d as u32, 16).unwrap_or('0'));
            frac -= d;
            count += 1;
        }
        (e, digits)
    };
    let limit = prec.unwrap_or(13).max(1);
    let mut v: Vec<char> = digits.chars().take(limit).collect();
    if digits.chars().nth(limit).is_some_and(|c| c >= '5') {
        // round half up, carrying into the integer part
        let mut j = v.len();
        let mut rolled = true;
        while j > 0 {
            j -= 1;
            let d = v[j].to_digit(16).unwrap_or(0) + 1;
            if d <= 15 {
                v[j] = std::char::from_digit(d, 16).unwrap_or('0');
                rolled = false;
                break;
            }
            v[j] = '0';
        }
        if rolled {
            v = vec!['0'; limit];
            e += 1;
        }
    }
    if prec.is_some() {
        while v.len() < limit {
            v.push('0');
        }
    }
    let mut digits: String = v.into_iter().collect();
    if prec.is_none() {
        while digits.ends_with('0') {
            digits.pop();
        }
    }
    if upper {
        digits = digits.to_uppercase();
    }
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    out.push_str(if upper { "0X" } else { "0x" });
    if a == 0.0 {
        out.push('0');
    } else {
        out.push('1');
        if !digits.is_empty() {
            out.push('.');
            out.push_str(&digits);
        }
    }
    out.push(if upper { 'P' } else { 'p' });
    out.push(if e < 0 { '-' } else { '+' });
    out.push_str(&e.unsigned_abs().to_string());
    out
}

/// Zero-pad a digit string to the printf precision.
fn pad_prec(body: &str, p: usize) -> String {
    let len = body.chars().count();
    if len >= p {
        body.to_string()
    } else {
        format!("{:0>width$}", body, width = p)
    }
}

/// Pad a finished conversion body with its sign.
fn pad_num(sign: &str, body: &str, width: usize, left: bool, zero: bool) -> String {
    let total = sign.chars().count() + body.chars().count();
    if total >= width {
        return format!("{}{}", sign, body);
    }
    let fill = width - total;
    if left {
        format!("{}{}{}", sign, body, " ".repeat(fill))
    } else if zero {
        format!("{}{}{}", sign, "0".repeat(fill), body)
    } else {
        format!("{}{}{}", " ".repeat(fill), sign, body)
    }
}

/// strtonum: decimal, 0x hex and leading-zero octal prefixes.
fn parse_strtonum(s: &str) -> f64 {
    let t = s.trim_start();
    let b = t.as_bytes();
    let mut i = 0;
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let rest = &t[i..];
    let rb = rest.as_bytes();
    let scale = |v: i128| -> f64 {
        let v = v as f64;
        if neg { -v } else { v }
    };
    // 0x1f style hex
    if rb.len() > 2 && rb[0] == b'0' && matches!(rb[1], b'x' | b'X') && rb[2].is_ascii_hexdigit() {
        let mut j = 2;
        let mut v: i128 = 0;
        while j < rb.len() && rb[j].is_ascii_hexdigit() {
            let c = rb[j];
            let d = if c.is_ascii_digit() {
                (c - b'0') as i128
            } else {
                ((c | 0x20) - b'a' + 10) as i128
            };
            v = v * 16 + d;
            j += 1;
        }
        return scale(v);
    }
    // leading zero: octal when every digit of the run is an octal digit
    if rb.len() >= 2 && rb[0] == b'0' && rb[1].is_ascii_digit() {
        let mut j = 1;
        while j < rb.len() && rb[j].is_ascii_digit() {
            j += 1;
        }
        let run = &rest[..j];
        if run.bytes().all(|c| (b'0'..=b'7').contains(&c)) {
            let mut v: i128 = 0;
            for c in run.bytes() {
                v = v * 8 + (c - b'0') as i128;
            }
            return scale(v);
        }
        // contains 8 or 9: plain decimal
        let v = crate::value::parse_num(rest);
        return if neg { -v } else { v };
    }
    let v = crate::value::parse_num(rest);
    if neg { -v } else { v }
}

/// Names used by strftime, chosen from LC_ALL / LC_TIME / LANG.
#[derive(Clone, Copy, PartialEq)]
enum TimeLocale {
    De,
    C,
}

fn time_locale() -> TimeLocale {
    for key in ["LC_ALL", "LC_TIME", "LANG"] {
        if let Ok(v) = std::env::var(key) {
            if v.is_empty() {
                continue;
            }
            return if v.to_ascii_lowercase().starts_with("de") {
                TimeLocale::De
            } else {
                TimeLocale::C
            };
        }
    }
    TimeLocale::C
}

const EN_WD_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const EN_WD_FULL: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const EN_MO_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const EN_MO_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DE_WD_ABBR: [&str; 7] = ["So", "Mo", "Di", "Mi", "Do", "Fr", "Sa"];
const DE_WD_FULL: [&str; 7] = [
    "Sonntag",
    "Montag",
    "Dienstag",
    "Mittwoch",
    "Donnerstag",
    "Freitag",
    "Samstag",
];
const DE_MO_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mär", "Apr", "Mai", "Jun", "Jul", "Aug", "Sep", "Okt", "Nov", "Dez",
];
const DE_MO_FULL: [&str; 12] = [
    "Januar",
    "Februar",
    "März",
    "April",
    "Mai",
    "Juni",
    "Juli",
    "August",
    "September",
    "Oktober",
    "November",
    "Dezember",
];

/// Broken down calendar fields shared by both time zones.
struct Parts {
    y: i32,
    mo: u32,
    d: u32,
    h: u32,
    mi: u32,
    s: u32,
    wd: u32, // 0 = Sunday
    yday: i32,
    unix: i64,
    tz: String,
    off: String,
    off_secs: i32,
}

fn parts_of<Tz: chrono::TimeZone>(dt: &chrono::DateTime<Tz>) -> Parts
where
    Tz::Offset: chrono::Offset + std::fmt::Display,
{
    use chrono::{Datelike, Offset, Timelike};
    Parts {
        y: dt.year(),
        mo: dt.month(),
        d: dt.day(),
        h: dt.hour(),
        mi: dt.minute(),
        s: dt.second(),
        wd: dt.weekday().num_days_from_sunday(),
        yday: dt.ordinal() as i32,
        unix: dt.timestamp(),
        tz: dt.format("%Z").to_string(),
        off: dt.format("%z").to_string(),
        off_secs: dt.offset().fix().local_minus_utc(),
    }
}

/// Zone abbreviation for a local timestamp. Unix reads libc so it matches
/// what gawk gets from localtime (CET/CEST, EST/EDT, ...).
#[cfg(unix)]
fn local_tzname(p: &Parts) -> String {
    use std::ffi::CStr;
    unsafe extern "C" {
        fn tzset();
        static timezone: i64;
        static daylight: i32;
        static mut tzname: [*const std::os::raw::c_char; 2];
    }
    unsafe {
        tzset();
        // timezone is always the standard (non-DST) offset, west of UTC
        let std_east = -(timezone as i32);
        let dst = daylight != 0 && p.off_secs != std_east;
        let q = tzname[if dst { 1 } else { 0 }];
        if q.is_null() {
            return String::new();
        }
        CStr::from_ptr(q).to_string_lossy().into_owned()
    }
}

/// No tzname outside Unix: chrono's abbreviation, else the numeric offset.
#[cfg(not(unix))]
fn local_tzname(p: &Parts) -> String {
    if p.tz.is_empty() {
        if p.off_secs == 0 {
            "UTC".to_string()
        } else {
            p.off.clone()
        }
    } else {
        p.tz.clone()
    }
}

fn hour12(h: u32) -> u32 {
    let v = h % 12;
    if v == 0 { 12 } else { v }
}

/// %U / %W: week of year, first week starts on Sunday / Monday.
fn week_of_year(p: &Parts, monday_first: bool) -> u32 {
    let wd = if monday_first { (p.wd + 6) % 7 } else { p.wd };
    let w = (p.yday as i64 + 6 - wd as i64) / 7;
    w.max(0) as u32
}

fn iso_week(p: &Parts) -> (i32, u32) {
    use chrono::Datelike;
    chrono::NaiveDate::from_ymd_opt(p.y, p.mo, p.d)
        .map(|d| {
            let w = d.iso_week();
            (w.year(), w.week())
        })
        .unwrap_or((p.y, 1))
}

/// strftime with the specifier set of gawk, honoring LC_TIME for names.
fn strftime(fmt: &str, ts: i64, utc: bool) -> String {
    use chrono::{Local, TimeZone, Utc};
    let parts = if utc {
        Utc.timestamp_opt(ts, 0).single().map(|dt| parts_of(&dt))
    } else {
        Local.timestamp_opt(ts, 0).single().map(|dt| parts_of(&dt))
    };
    let Some(mut p) = parts else {
        return String::new();
    };
    p.tz = if utc {
        "GMT".to_string()
    } else {
        local_tzname(&p)
    };
    let loc = time_locale();
    let wd_abbr = match loc {
        TimeLocale::De => &DE_WD_ABBR[..],
        TimeLocale::C => &EN_WD_ABBR[..],
    };
    let wd_full = match loc {
        TimeLocale::De => &DE_WD_FULL[..],
        TimeLocale::C => &EN_WD_FULL[..],
    };
    let mo_abbr = match loc {
        TimeLocale::De => &DE_MO_ABBR[..],
        TimeLocale::C => &EN_MO_ABBR[..],
    };
    let mo_full = match loc {
        TimeLocale::De => &DE_MO_FULL[..],
        TimeLocale::C => &EN_MO_FULL[..],
    };
    let ampm = match loc {
        TimeLocale::De => "",
        TimeLocale::C => {
            if p.h < 12 {
                "AM"
            } else {
                "PM"
            }
        }
    };
    let ampm_l = match loc {
        TimeLocale::De => "",
        TimeLocale::C => {
            if p.h < 12 {
                "am"
            } else {
                "pm"
            }
        }
    };
    let (iso_y, iso_w) = iso_week(&p);
    let mut out = String::new();
    let b = fmt.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'%' {
            let l = utf8_len(b[i]);
            out.push_str(std::str::from_utf8(&b[i..i + l]).unwrap_or(""));
            i += l;
            continue;
        }
        if i + 1 >= b.len() {
            out.push('%');
            break;
        }
        let c = b[i + 1];
        i += 2;
        match c {
            b'%' => out.push('%'),
            b'n' => out.push('\n'),
            b't' => out.push('\t'),
            b'A' => out.push_str(wd_full[p.wd as usize]),
            b'a' => out.push_str(wd_abbr[p.wd as usize]),
            b'B' => out.push_str(mo_full[p.mo as usize - 1]),
            b'b' | b'h' => out.push_str(mo_abbr[p.mo as usize - 1]),
            b'C' => out.push_str(&format!("{:02}", p.y.div_euclid(100))),
            b'd' => out.push_str(&format!("{:02}", p.d)),
            b'e' => out.push_str(&format!("{:2}", p.d)),
            b'D' => out.push_str(&format!(
                "{:02}/{:02}/{:02}",
                p.mo,
                p.d,
                p.y.rem_euclid(100)
            )),
            b'F' => out.push_str(&format!("{:04}-{:02}-{:02}", p.y, p.mo, p.d)),
            b'G' => out.push_str(&format!("{}", iso_y)),
            b'g' => out.push_str(&format!("{:02}", iso_y.rem_euclid(100))),
            b'H' => out.push_str(&format!("{:02}", p.h)),
            b'I' => out.push_str(&format!("{:02}", hour12(p.h))),
            b'j' => out.push_str(&format!("{:03}", p.yday)),
            b'k' => out.push_str(&format!("{:2}", p.h)),
            b'l' => out.push_str(&format!("{:2}", hour12(p.h))),
            b'm' => out.push_str(&format!("{:02}", p.mo)),
            b'M' => out.push_str(&format!("{:02}", p.mi)),
            b'p' => out.push_str(ampm),
            b'P' => out.push_str(ampm_l),
            b'S' => out.push_str(&format!("{:02}", p.s)),
            b'T' => out.push_str(&format!("{:02}:{:02}:{:02}", p.h, p.mi, p.s)),
            b'u' => out.push_str(&format!("{}", if p.wd == 0 { 7 } else { p.wd })),
            b'U' => out.push_str(&format!("{:02}", week_of_year(&p, false))),
            b'V' => out.push_str(&format!("{:02}", iso_w)),
            b'W' => out.push_str(&format!("{:02}", week_of_year(&p, true))),
            b'w' => out.push_str(&format!("{}", p.wd)),
            b'X' => out.push_str(&format!("{:02}:{:02}:{:02}", p.h, p.mi, p.s)),
            b'x' => match loc {
                TimeLocale::De => out.push_str(&format!("{:02}.{:02}.{:04}", p.d, p.mo, p.y)),
                TimeLocale::C => out.push_str(&format!(
                    "{:02}/{:02}/{:02}",
                    p.mo,
                    p.d,
                    p.y.rem_euclid(100)
                )),
            },
            b'Y' => out.push_str(&format!("{}", p.y)),
            b'y' => out.push_str(&format!("{:02}", p.y.rem_euclid(100))),
            b'Z' => out.push_str(&p.tz),
            b'z' => out.push_str(&p.off),
            b's' => out.push_str(&p.unix.to_string()),
            b'c' => match loc {
                TimeLocale::De => out.push_str(&format!(
                    "{} {:02} {} {:04} {:02}:{:02}:{:02} {}",
                    wd_abbr[p.wd as usize],
                    p.d,
                    mo_abbr[p.mo as usize - 1],
                    p.y,
                    p.h,
                    p.mi,
                    p.s,
                    p.tz
                )),
                TimeLocale::C => out.push_str(&format!(
                    "{} {} {:2} {:02}:{:02}:{:02} {}",
                    wd_abbr[p.wd as usize],
                    mo_abbr[p.mo as usize - 1],
                    p.d,
                    p.h,
                    p.mi,
                    p.s,
                    p.y
                )),
            },
            b'r' => out.push_str(&format!(
                "{:02}:{:02}:{:02} {}",
                hour12(p.h),
                p.mi,
                p.s,
                ampm
            )),
            // unknown specifier: keep it literal, like gawk does for %v
            other => {
                out.push('%');
                out.push(other as char);
            }
        }
    }
    out
}

/// mktime: at least six integer fields, local time unless the flag is set.
/// Extra fields are ignored, too few fields or junk yields -1.
fn mktime(spec: &str, utc: bool) -> f64 {
    let fields: Vec<&str> = spec.split_whitespace().collect();
    if fields.len() < 6 {
        return -1.0;
    }
    let mut nums = [0i64; 6];
    for (i, f) in fields.iter().take(6).enumerate() {
        match f.parse::<i64>() {
            Ok(v) => nums[i] = v,
            Err(_) => return -1.0,
        }
    }
    let (y, mon, day, hour, min, sec) = (nums[0], nums[1], nums[2], nums[3], nums[4], nums[5]);
    use chrono::{Duration, Local, NaiveDate, NaiveTime, Offset, TimeZone};
    // months are 1-based and carry into the year
    let total_mon = y * 12 + (mon - 1);
    let y2 = total_mon.div_euclid(12);
    let m2 = total_mon.rem_euclid(12) + 1;
    if !(i32::MIN as i64..=i32::MAX as i64).contains(&y2) || !(1..=12).contains(&m2) {
        return -1.0;
    }
    let secs = hour * 3600 + min * 60 + sec;
    let extra_days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let Some(base) = NaiveDate::from_ymd_opt(y2 as i32, m2 as u32, 1) else {
        return -1.0;
    };
    let days = day - 1 + extra_days;
    let Some(date) = base.checked_add_signed(Duration::days(days)) else {
        return -1.0;
    };
    let Some(time) = NaiveTime::from_hms_opt(
        (rem / 3600) as u32,
        ((rem / 60) % 60) as u32,
        (rem % 60) as u32,
    ) else {
        return -1.0;
    };
    let naive = date.and_time(time);
    if utc {
        return naive.and_utc().timestamp() as f64;
    }
    // fixed point: local(wall time) == naive. Converges for normal and
    // ambiguous (DST fall back) times.
    let mut ts = naive.and_utc().timestamp();
    for _ in 0..8 {
        let dt = match Local.timestamp_opt(ts, 0) {
            chrono::LocalResult::Single(d) => d,
            chrono::LocalResult::Ambiguous(d, _) => d,
            chrono::LocalResult::None => break,
        };
        let diff = (naive - dt.naive_local()).num_seconds();
        if diff == 0 {
            return ts as f64;
        }
        ts += diff;
    }
    // no fixed point: DST gap. gawk (via glibc mktime) then uses the
    // offset that was in effect before the transition.
    let utc_naive = naive.and_utc().timestamp();
    let before = Local
        .timestamp_opt(utc_naive - 3 * 3600, 0)
        .single()
        .map(|d| d.offset().fix().local_minus_utc() as i64)
        .unwrap_or(0);
    (utc_naive - before) as f64
}
