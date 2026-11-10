//! Recursive-descent parser for POSIX awk.

use crate::ast::*;
use crate::lexer::{Lexer, Tok};
use std::rc::Rc;

pub struct Parser<'a> {
    lx: Lexer<'a>,
    cur: Tok,
    line: usize,
    // in print args, bare `>` is redirect not comparison
    no_gt: bool,
}

impl<'a> Parser<'a> {
    pub fn new(src: &'a str) -> Result<Self, String> {
        let mut lx = Lexer::new(src);
        let (cur, line) = lx.next()?;
        Ok(Parser { lx, cur, line, no_gt: false })
    }

    fn bump(&mut self) -> Result<(), String> {
        let (t, l) = self.lx.next()?;
        self.cur = t;
        self.line = l;
        Ok(())
    }

    fn eat(&mut self, t: Tok) -> Result<(), String> {
        if self.cur == t {
            self.bump()
        } else {
            Err(format!("line {}: expected {:?}, got {:?}", self.line, t, self.cur))
        }
    }

    /// Parse a full program: list of rules.
    pub fn parse_program(&mut self) -> Result<Program, String> {
        let mut rules = Vec::new();
        loop {
            // skip stray newlines / semicolons
            while matches!(self.cur, Tok::Newline | Tok::Semicolon) {
                self.bump()?;
            }
            if self.cur == Tok::Eof {
                break;
            }
            rules.push(self.parse_rule()?);
        }
        Ok(Program::Rules(rules))
    }

    fn parse_rule(&mut self) -> Result<Rule, String> {
        // function definitions are top-level only
        if self.cur == Tok::Function {
            // stash as a special rule? handled by parse_functions first.
            // For simplicity functions are parsed in parse_program via separate pass.
            return Err(format!("line {}: function must be at top level before rules", self.line));
        }

        let pattern = if self.cur == Tok::Begin {
            self.bump()?;
            Some(Pattern::Begin)
        } else if self.cur == Tok::End {
            self.bump()?;
            Some(Pattern::End)
        } else if self.is_action_start() {
            None
        } else {
            Some(self.parse_pattern()?)
        };

        // action optional
        let action = if self.cur == Tok::LBrace {
            Some(self.parse_block_stmt()?)
        } else if matches!(self.cur, Tok::Newline | Tok::Semicolon | Tok::Eof) {
            // default action: print $0
            if matches!(self.cur, Tok::Newline | Tok::Semicolon) {
                self.bump()?;
            }
            Some(default_print())
        } else if pattern.is_some() {
            // single statement action without braces: `pattern stmt`
            let s = self.parse_statement()?;
            Some(s)
        } else {
            return Err(format!("line {}: expected action", self.line));
        };

        Ok(Rule { pattern, action })
    }

    fn is_action_start(&self) -> bool {
        matches!(
            self.cur,
            Tok::LBrace
                | Tok::If
                | Tok::While
                | Tok::Do
                | Tok::For
                | Tok::Break
                | Tok::Continue
                | Tok::Next
                | Tok::Exit
                | Tok::Delete
                | Tok::Getline
                | Tok::Return
                | Tok::Function
        ) || self.is_print_like()
    }

    fn is_print_like(&self) -> bool {
        // print/printf are idents in our lexer? No — we need keywords.
        // We treat Ident("print") specially.
        matches!(&self.cur, Tok::Ident(s) if s == "print" || s == "printf")
    }

    fn parse_pattern(&mut self) -> Result<Pattern, String> {
        // regex-only pattern: /re/
        // We need lexer to produce Regex when in pattern position.
        // Our main lexer produces Slash for /. So re-lex: if cur is Slash, lex_regex.
        if self.cur == Tok::Slash {
            self.lx.back_slash();
            let (t, l) = self.lx.lex_regex()?;
            self.cur = t;
            self.line = l;
            let re = match std::mem::replace(&mut self.cur, Tok::Eof) {
                Tok::Regex(r) => r,
                _ => return Err(format!("line {}: expected regex", self.line)),
            };
            // re-advance: lex_regex consumed, next token is after /
            let (t2, l2) = self.lx.next()?;
            self.cur = t2;
            self.line = l2;
            // range?
            if self.cur == Tok::Comma {
                self.bump()?;
                let e2 = self.parse_expr()?;
                return Ok(Pattern::Range(Expr::Str(re), e2));
            }
            return Ok(Pattern::Regex(re));
        }

        let e1 = self.parse_expr()?;
        if self.cur == Tok::Comma {
            self.bump()?;
            // second can be regex or expr
            let e2 = if self.cur == Tok::Slash {
                self.lx.back_slash();
                let (t, l) = self.lx.lex_regex()?;
                let re = match t {
                    Tok::Regex(r) => r,
                    _ => return Err(format!("line {}: expected regex", self.line)),
                };
                self.line = l;
                let (t2, l2) = self.lx.next()?;
                self.cur = t2;
                self.line = l2;
                Expr::Str(re)
            } else {
                self.parse_expr()?
            };
            return Ok(Pattern::Range(e1, e2));
        }
        Ok(Pattern::Expr(e1))
    }

    fn parse_block_stmt(&mut self) -> Result<Stmt, String> {
        self.eat(Tok::LBrace)?;
        let mut stmts = Vec::new();
        loop {
            while matches!(self.cur, Tok::Newline | Tok::Semicolon) {
                self.bump()?;
            }
            if self.cur == Tok::RBrace {
                break;
            }
            if self.cur == Tok::Eof {
                return Err(format!("line {}: unclosed block", self.line));
            }
            stmts.push(self.parse_statement()?);
        }
        self.eat(Tok::RBrace)?;
        Ok(Stmt::Block(stmts))
    }

    fn parse_statement(&mut self) -> Result<Stmt, String> {
        match &self.cur {
            Tok::Newline | Tok::Semicolon => {
                self.bump()?;
                Ok(Stmt::Empty)
            }
            Tok::LBrace => self.parse_block_stmt(),
            Tok::If => {
                self.bump()?;
                self.eat(Tok::LParen)?;
                let cond = self.parse_expr()?;
                self.eat(Tok::RParen)?;
                let then = Box::new(self.parse_statement()?);
                while matches!(self.cur, Tok::Semicolon | Tok::Newline) {
                    self.bump()?;
                }
                let els = if self.cur == Tok::Else {
                    self.bump()?;
                    Some(Box::new(self.parse_statement()?))
                } else {
                    None
                };
                Ok(Stmt::If(cond, then, els))
            }
            Tok::While => {
                self.bump()?;
                self.eat(Tok::LParen)?;
                let cond = self.parse_expr()?;
                self.eat(Tok::RParen)?;
                let body = Box::new(self.parse_statement()?);
                Ok(Stmt::While(cond, body))
            }
            Tok::Do => {
                self.bump()?;
                let body = Box::new(self.parse_statement()?);
                // optional newline before while
                while matches!(self.cur, Tok::Newline) {
                    self.bump()?;
                }
                self.eat(Tok::While)?;
                self.eat(Tok::LParen)?;
                let cond = self.parse_expr()?;
                self.eat(Tok::RParen)?;
                Ok(Stmt::DoWhile(body, cond))
            }
            Tok::For => self.parse_for(),
            Tok::Break => {
                self.bump()?;
                Ok(Stmt::Break)
            }
            Tok::Continue => {
                self.bump()?;
                Ok(Stmt::Continue)
            }
            Tok::Next => {
                self.bump()?;
                Ok(Stmt::Next)
            }
            Tok::Exit => {
                self.bump()?;
                let e = if self.expr_start() {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                Ok(Stmt::Exit(e))
            }
            Tok::Return => {
                self.bump()?;
                let e = if self.expr_start() {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                Ok(Stmt::Return(e))
            }
            Tok::Delete => {
                self.bump()?;
                let name = match std::mem::replace(&mut self.cur, Tok::Eof) {
                    Tok::Ident(s) => {
                        self.bump()?;
                        s
                    }
                    _ => return Err(format!("line {}: expected array name", self.line)),
                };
                if self.cur == Tok::LBracket {
                    self.bump()?;
                    let idx = self.parse_expr()?;
                    self.eat(Tok::RBracket)?;
                    Ok(Stmt::Delete(name, Some(idx)))
                } else {
                    Ok(Stmt::Delete(name, None))
                }
            }
            Tok::Ident(s) if s == "print" => self.parse_print(false),
            Tok::Ident(s) if s == "printf" => self.parse_print(true),
            _ => {
                let e = self.parse_expr()?;
                // implicit print? no — expression statement
                Ok(Stmt::Expr(e))
            }
        }
    }

    fn parse_for(&mut self) -> Result<Stmt, String> {
        self.eat(Tok::For)?;
        self.eat(Tok::LParen)?;
        // init
        let init = if self.cur == Tok::Semicolon {
            None
        } else if let Tok::Ident(name) = &self.cur {
            // could be for (i in arr)
            let name = name.clone();
            let save_line = self.line;
            self.bump()?;
            if self.cur == Tok::In {
                self.bump()?;
                let arr = match std::mem::replace(&mut self.cur, Tok::Eof) {
                    Tok::Ident(a) => {
                        self.bump()?;
                        a
                    }
                    _ => return Err(format!("line {}: expected array", self.line)),
                };
                self.eat(Tok::RParen)?;
                let body = Box::new(self.parse_statement()?);
                return Ok(Stmt::ForIn(vec![name], arr, body));
            }
            // re-parse as expression: we already consumed ident — rebuild
            let mut e = Expr::Var(name);
            e = self.parse_postfix_after_ident(e)?;
            e = self.parse_bin_rhs(0, e)?;
            let e = Stmt::Expr(e);
            let _ = save_line;
            Some(Box::new(e))
        } else {
            let e = self.parse_expr()?;
            Some(Box::new(Stmt::Expr(e)))
        };
        self.eat(Tok::Semicolon)?;
        let cond = if self.cur == Tok::Semicolon {
            None
        } else {
            Some(self.parse_expr()?)
        };
        self.eat(Tok::Semicolon)?;
        let step = if self.cur == Tok::RParen {
            None
        } else {
            let e = self.parse_expr()?;
            Some(Box::new(Stmt::Expr(e)))
        };
        self.eat(Tok::RParen)?;
        let body = Box::new(self.parse_statement()?);
        Ok(Stmt::For(init, cond, step, body))
    }

    /// After seeing Ident in for-init, continue parsing postfix/binops.
    fn parse_postfix_after_ident(&mut self, mut e: Expr) -> Result<Expr, String> {
        loop {
            match &self.cur {
                Tok::LBracket => {
                    self.bump()?;
                    let idx = self.parse_expr()?;
                    self.eat(Tok::RBracket)?;
                    e = Expr::Index(Box::new(e), Box::new(idx));
                }
                Tok::LParen => {
                    // call — only if e is Var
                    if let Expr::Var(name) = &e {
                        let name = name.clone();
                        self.bump()?;
                        let mut args = Vec::new();
                        if self.cur != Tok::RParen {
                            loop {
                                args.push(self.parse_expr()?);
                                if self.cur == Tok::Comma {
                                    self.bump()?;
                                } else {
                                    break;
                                }
                            }
                        }
                        self.eat(Tok::RParen)?;
                        e = Expr::Call(name, args);
                    } else {
                        break;
                    }
                }
                Tok::Inc => {
                    self.bump()?;
                    e = Expr::PostInc(Box::new(e));
                }
                Tok::Dec => {
                    self.bump()?;
                    e = Expr::PostDec(Box::new(e));
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn parse_print(&mut self, is_printf: bool) -> Result<Stmt, String> {
        self.bump()?; // print / printf
        let mut args = Vec::new();
        if self.expr_start() || self.cur == Tok::LParen {
            self.no_gt = true;
            loop {
                args.push(self.parse_expr()?);
                if self.cur == Tok::Comma {
                    self.bump()?;
                } else {
                    break;
                }
            }
            self.no_gt = false;
        }
        let redir = if self.cur == Tok::Gt {
            self.bump()?;
            if self.cur == Tok::Gt {
                self.bump()?;
                Some(Redir::Append(self.parse_expr()?))
            } else {
                Some(Redir::Trunc(self.parse_expr()?))
            }
        } else if matches!(&self.cur, Tok::Ident(s) if s == "|") {
            self.bump()?;
            Some(Redir::Pipe(self.parse_expr()?))
        } else {
            None
        };
        if is_printf {
            Ok(Stmt::Printf(args, redir))
        } else {
            Ok(Stmt::Print(args, redir))
        }
    }

    fn expr_start(&self) -> bool {
        matches!(
            self.cur,
            Tok::Num(_)
                | Tok::Str(_)
                | Tok::Regex(_)
                | Tok::Ident(_)
                | Tok::Dollar
                | Tok::LParen
                | Tok::LBracket
                | Tok::Minus
                | Tok::Plus
                | Tok::Not
                | Tok::Inc
                | Tok::Dec
                | Tok::Getline
        )
    }

    // ----- expressions with precedence climbing -----

    pub fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_assign()
    }

    /// Right-associative assignment below ternary.
    fn parse_assign(&mut self) -> Result<Expr, String> {
        let lhs = self.parse_ternary()?;
        let op = match self.cur {
            Tok::Assign => 0,
            Tok::PlusAssign => 1,
            Tok::MinusAssign => 2,
            Tok::MulAssign => 3,
            Tok::DivAssign => 4,
            Tok::ModAssign => 5,
            Tok::PowAssign => 6,
            _ => return Ok(lhs),
        };
        self.bump()?;
        let rhs = self.parse_assign()?;
        let e = match op {
            0 => Expr::Assign(Box::new(lhs), Box::new(rhs)),
            1 => Expr::PlusAssign(Box::new(lhs), Box::new(rhs)),
            2 => Expr::MinusAssign(Box::new(lhs), Box::new(rhs)),
            3 => Expr::MulAssign(Box::new(lhs), Box::new(rhs)),
            4 => Expr::DivAssign(Box::new(lhs), Box::new(rhs)),
            5 => Expr::ModAssign(Box::new(lhs), Box::new(rhs)),
            6 => Expr::PowAssign(Box::new(lhs), Box::new(rhs)),
            _ => unreachable!(),
        };
        Ok(e)
    }

    fn parse_ternary(&mut self) -> Result<Expr, String> {
        let cond = self.parse_or()?;
        if self.cur == Tok::Question {
            self.bump()?;
            let a = self.parse_expr()?;
            self.eat(Tok::Colon)?;
            let b = self.parse_expr()?;
            return Ok(Expr::Ternary(Box::new(cond), Box::new(a), Box::new(b)));
        }
        Ok(cond)
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_and()?;
        while self.cur == Tok::OrOr {
            self.bump()?;
            let r = self.parse_and()?;
            e = Expr::Or(Box::new(e), Box::new(r));
        }
        Ok(e)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_in()?;
        while self.cur == Tok::AndAnd {
            self.bump()?;
            let r = self.parse_in()?;
            e = Expr::And(Box::new(e), Box::new(r));
        }
        Ok(e)
    }

    /// `expr in array` at lowest of relational level in awk.
    fn parse_in(&mut self) -> Result<Expr, String> {
        let e = self.parse_not()?;
        if self.cur == Tok::In {
            // only valid as relational — handled in parse_cmp via flag
            // Actually POSIX: `primary in array` is a relational expr.
            // We handle after not-level by checking in parse_cmp.
        }
        Ok(e)
    }

    fn parse_not(&mut self) -> Result<Expr, String> {
        if self.cur == Tok::Not {
            self.bump()?;
            let e = self.parse_not()?;
            return Ok(Expr::Not(Box::new(e)));
        }
        self.parse_match()
    }

    fn parse_match(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_cmp()?;
        loop {
            match self.cur {
                Tok::Match => {
                    self.bump()?;
                    let re = self.parse_re_or_expr()?;
                    e = Expr::Match(Box::new(e), Box::new(re));
                }
                Tok::NoMatch => {
                    self.bump()?;
                    let re = self.parse_re_or_expr()?;
                    e = Expr::NoMatch(Box::new(e), Box::new(re));
                }
                Tok::In => {
                    // e in array
                    self.bump()?;
                    let arr = match std::mem::replace(&mut self.cur, Tok::Eof) {
                        Tok::Ident(a) => {
                            self.bump()?;
                            a
                        }
                        _ => return Err(format!("line {}: expected array name", self.line)),
                    };
                    e = Expr::InArray(Box::new(e), arr);
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn parse_re_or_expr(&mut self) -> Result<Expr, String> {
        if self.cur == Tok::Slash {
            self.lx.back_slash();
            let (t, _) = self.lx.lex_regex()?;
            let re = match t {
                Tok::Regex(r) => r,
                _ => return Err(format!("line {}: expected regex", self.line)),
            };
            let (t2, l2) = self.lx.next()?;
            self.cur = t2;
            self.line = l2;
            return Ok(Expr::Regex(re));
        }
        // /re/ as string also appears as Str after lex in some positions
        self.parse_concat()
    }

    fn parse_cmp(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_concat()?;
        loop {
            let op = match self.cur {
                Tok::Lt => Tok::Lt,
                Tok::Le => Tok::Le,
                Tok::Gt if !self.no_gt => Tok::Gt,
                Tok::Ge => Tok::Ge,
                Tok::Eq => Tok::Eq,
                Tok::Ne => Tok::Ne,
                _ => break,
            };
            self.bump()?;
            let r = self.parse_concat()?;
            e = match op {
                Tok::Lt => Expr::Lt(Box::new(e), Box::new(r)),
                Tok::Le => Expr::Le(Box::new(e), Box::new(r)),
                Tok::Gt => Expr::Gt(Box::new(e), Box::new(r)),
                Tok::Ge => Expr::Ge(Box::new(e), Box::new(r)),
                Tok::Eq => Expr::Eq(Box::new(e), Box::new(r)),
                Tok::Ne => Expr::Ne(Box::new(e), Box::new(r)),
                _ => unreachable!(),
            };
        }
        Ok(e)
    }

    /// Concatenation: juxtaposition of additive expressions.
    fn parse_concat(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_add()?;
        while self.concat_start() {
            let r = self.parse_add()?;
            e = Expr::Concat(Box::new(e), Box::new(r));
        }
        Ok(e)
    }

    fn concat_start(&self) -> bool {
        matches!(
            self.cur,
            Tok::Num(_)
                | Tok::Str(_)
                | Tok::Regex(_)
                | Tok::Dollar
                | Tok::LParen
                | Tok::LBracket
        ) || matches!(&self.cur, Tok::Ident(s) if s != "|")
    }

    fn parse_add(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_mul()?;
        loop {
            match self.cur {
                Tok::Plus => {
                    self.bump()?;
                    let r = self.parse_mul()?;
                    e = Expr::Add(Box::new(e), Box::new(r));
                }
                Tok::Minus => {
                    self.bump()?;
                    let r = self.parse_mul()?;
                    e = Expr::Sub(Box::new(e), Box::new(r));
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn parse_mul(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_pow()?;
        loop {
            match self.cur {
                Tok::Star => {
                    self.bump()?;
                    let r = self.parse_pow()?;
                    e = Expr::Mul(Box::new(e), Box::new(r));
                }
                Tok::Slash => {
                    self.bump()?;
                    let r = self.parse_pow()?;
                    e = Expr::Div(Box::new(e), Box::new(r));
                }
                Tok::Percent => {
                    self.bump()?;
                    let r = self.parse_pow()?;
                    e = Expr::Mod(Box::new(e), Box::new(r));
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn parse_pow(&mut self) -> Result<Expr, String> {
        let e = self.parse_unary()?;
        if self.cur == Tok::Caret || self.cur == Tok::StarStar {
            self.bump()?;
            // right assoc
            let r = self.parse_pow()?;
            return Ok(Expr::Pow(Box::new(e), Box::new(r)));
        }
        Ok(e)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        match self.cur {
            Tok::Minus => {
                self.bump()?;
                let e = self.parse_unary()?;
                Ok(Expr::Neg(Box::new(e)))
            }
            Tok::Plus => {
                self.bump()?;
                self.parse_unary()
            }
            Tok::Inc => {
                self.bump()?;
                let e = self.parse_unary()?;
                Ok(Expr::PreInc(Box::new(e)))
            }
            Tok::Dec => {
                self.bump()?;
                let e = self.parse_unary()?;
                Ok(Expr::PreDec(Box::new(e)))
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_primary()?;
        loop {
            match &self.cur {
                Tok::LBracket => {
                    self.bump()?;
                    let idx = self.parse_expr()?;
                    self.eat(Tok::RBracket)?;
                    e = Expr::Index(Box::new(e), Box::new(idx));
                }
                Tok::LParen => {
                    if let Expr::Var(name) = &e {
                        let name = name.clone();
                        self.bump()?;
                        let mut args = Vec::new();
                        if self.cur != Tok::RParen {
                            loop {
                                args.push(self.parse_expr()?);
                                if self.cur == Tok::Comma {
                                    self.bump()?;
                                } else {
                                    break;
                                }
                            }
                        }
                        self.eat(Tok::RParen)?;
                        e = Expr::Call(name, args);
                    } else {
                        break;
                    }
                }
                Tok::Inc => {
                    self.bump()?;
                    e = Expr::PostInc(Box::new(e));
                }
                Tok::Dec => {
                    self.bump()?;
                    e = Expr::PostDec(Box::new(e));
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match std::mem::replace(&mut self.cur, Tok::Eof) {
            Tok::Num(n) => {
                self.bump()?;
                Ok(Expr::Num(n))
            }
            Tok::Str(s) => {
                self.bump()?;
                Ok(Expr::Str(s))
            }
            Tok::Regex(r) => {
                self.bump()?;
                Ok(Expr::Regex(r))
            }
            Tok::Slash => {
                // regex literal in expression position
                self.lx.back_slash();
                let (tok, l) = self.lx.lex_regex()?;
                match tok {
                    Tok::Regex(r) => {
                        let (t2, l2) = self.lx.next()?;
                        self.cur = t2;
                        self.line = l2;
                        let _ = l;
                        Ok(Expr::Regex(r))
                    }
                    other => Err(format!("line {}: expected regex, got {:?}", l, other)),
                }
            }
            Tok::Ident(s) => {
                self.bump()?;
                Ok(Expr::Var(s))
            }
            Tok::Dollar => {
                self.bump()?;
                let field = self.parse_unary()?;
                Ok(Expr::Field(Box::new(field)))
            }
            Tok::LParen => {
                self.bump()?;
                let saved = self.no_gt;
                self.no_gt = false;
                let e = self.parse_expr()?;
                self.no_gt = saved;
                self.eat(Tok::RParen)?;
                Ok(e)
            }
            Tok::Getline => {
                // getline as expression: simplified
                self.bump()?;
                // getline [var] [< file | | cmd]
                let mut var = None;
                if let Tok::Ident(name) = &self.cur {
                    var = Some(name.clone());
                    self.bump()?;
                }
                let src = if self.cur == Tok::Lt {
                    self.bump()?;
                    let f = self.parse_concat()?;
                    Some(GetlineSrc::File(f))
                } else if matches!(&self.cur, Tok::Ident(s) if s == "|") {
                    self.bump()?;
                    let c = self.parse_concat()?;
                    Some(GetlineSrc::Cmd(c))
                } else {
                    None
                };
                let target = var.map(Expr::Var).unwrap_or_else(|| Expr::Var("$0".into()));
                Ok(Expr::Call(
                    "getline".into(),
                    {
                        let mut a = vec![target];
                        if let Some(s) = src {
                            a.push(match s {
                                GetlineSrc::File(f) => Expr::Call("<".into(), vec![f]),
                                GetlineSrc::Cmd(c) => Expr::Call("|".into(), vec![c]),
                                GetlineSrc::None | GetlineSrc::VarFile(_, _) => Expr::Num(0.0),
                            });
                        }
                        a
                    },
                ))
            }
            other => Err(format!("line {}: unexpected token {:?}", self.line, other)),
        }
    }

    /// Continue binary parsing after an ident was already consumed in for-init.
    fn parse_bin_rhs(&mut self, min_prec: i32, lhs: Expr) -> Result<Expr, String> {
        let _ = min_prec;
        let e = self.parse_postfix_after_ident(lhs)?;
        // then continue with unary-level operators by feeding into full chain:
        // simplest: wrap back through cmp — not perfect but for-init is simple assigns
        let _ = e.clone();
        // re-enter at add level via a hack: put expression through parse_cmp-like
        // We'll just return and let caller use parse_expr-style:
        // Actually for-init after ident is usually `i = 0` or `i++` or `i = i + 1`
        // Handle assign ops here:
        match self.cur {
            Tok::Assign => {
                self.bump()?;
                let r = self.parse_expr()?;
                Ok(Expr::Assign(Box::new(e), Box::new(r)))
            }
            Tok::PlusAssign => {
                self.bump()?;
                let r = self.parse_expr()?;
                Ok(Expr::PlusAssign(Box::new(e), Box::new(r)))
            }
            Tok::MinusAssign => {
                self.bump()?;
                let r = self.parse_expr()?;
                Ok(Expr::MinusAssign(Box::new(e), Box::new(r)))
            }
            Tok::Inc => {
                self.bump()?;
                Ok(Expr::PostInc(Box::new(e)))
            }
            Tok::Dec => {
                self.bump()?;
                Ok(Expr::PostDec(Box::new(e)))
            }
            _ => Ok(e),
        }
    }
}

fn default_print() -> Stmt {
    Stmt::Print(vec![Expr::Field(Box::new(Expr::Num(0.0)))], None)
}

/// Parse a program that may contain function definitions first.
pub fn parse_full(src: &str) -> Result<(Vec<FuncDef>, Program), String> {
    let (funcs, rest) = split_functions(src)?;
    let mut p = Parser::new(&rest)?;
    let prog = p.parse_program()?;
    Ok((funcs, prog))
}

/// Split out `function name(a,b) { ... }` definitions from program text.
fn split_functions(src: &str) -> Result<(Vec<FuncDef>, String), String> {
    let mut funcs: Vec<FuncDef> = Vec::new();
    let mut out = String::new();
    let bytes = src.as_bytes();
    let i = 0usize;
    // naive but handles strings/comments via full re-parse of function chunks
    // Find "function" at top level by tokenizing whole file once
    let mut lx = Lexer::new(src);
    let mut tokens: Vec<(Tok, usize, usize, usize)> = Vec::new(); // tok, line, start, end
    loop {
        let start = lx_pos(&lx);
        let (t, l) = lx.next()?;
        let end = lx_pos(&lx);
        let eof = t == Tok::Eof;
        tokens.push((t, l, start, end));
        if eof {
            break;
        }
    }
    // Walk tokens; when Function, parse a function from source slice
    let out_tokens_kept = String::new();
    let mut idx = 0usize;
    while idx < tokens.len() {
        if tokens[idx].0 == Tok::Function {
            // find matching: function name ( params ) { stmts }
            let name = match &tokens[idx + 1].0 {
                Tok::Ident(s) => s.clone(),
                _ => return Err(format!("line {}: expected function name", tokens[idx].1)),
            };
            let mut j = idx + 2;
            if tokens[j].0 != Tok::LParen {
                return Err(format!("line {}: expected ( after {}", tokens[idx + 1].1, name));
            }
            j += 1;
            let mut params = Vec::new();
            while tokens[j].0 != Tok::RParen {
                if let Tok::Ident(p) = &tokens[j].0 {
                    params.push(p.clone());
                } else if tokens[j].0 == Tok::Comma {
                    // ok
                } else {
                    return Err(format!("line {}: bad parameter list", tokens[j].1));
                }
                j += 1;
            }
            j += 1; // )
            // body must be {
            if tokens[j].0 != Tok::LBrace {
                return Err(format!("line {}: expected {{ after function {}", tokens[j].1, name));
            }
            // find matching brace by depth
            let mut depth = 0i32;
            let body_start_tok = j;
            loop {
                match tokens[j].0 {
                    Tok::LBrace => depth += 1,
                    Tok::RBrace => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    Tok::Eof => return Err(format!("line {}: unclosed function {}", tokens[body_start_tok].1, name)),
                    _ => {}
                }
                j += 1;
            }
            let body_end_tok = j;
            // extract source from body_start to body_end
            let start = tokens[body_start_tok].2;
            let end = tokens[body_end_tok].3;
            let body_src = &src[start..end];
            let mut bp = Parser::new(body_src)?;
            let body = bp.parse_block_stmt()?;
            funcs.push(FuncDef { name, params, body: Rc::new(body) });
            idx = body_end_tok + 1;
            out.push(' '); // placeholder: remove function from remaining program
            continue;
        }
        // copy source for this token range into out (rebuild remaining program)
        // Better: mark ranges to skip
        idx += 1;
    }
    // Rebuild program source excluding function spans
    let mut skip: Vec<(usize, usize)> = Vec::new();
    idx = 0;
    while idx < tokens.len() {
        if tokens[idx].0 == Tok::Function {
            let mut j = idx + 1;
            while tokens[j].0 != Tok::LBrace {
                j += 1;
            }
            let mut depth = 0i32;
            loop {
                match tokens[j].0 {
                    Tok::LBrace => depth += 1,
                    Tok::RBrace => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            skip.push((tokens[idx].2, tokens[j].3));
            idx = j + 1;
        } else {
            idx += 1;
        }
    }
    let mut last = 0usize;
    for (s, e) in skip {
        out.push_str(&src[last..s]);
        last = e;
    }
    out.push_str(&src[last..]);
    let _ = out_tokens_kept;
    let _ = i;
    let _ = bytes;
    Ok((funcs, out))
}

fn lx_pos(lx: &Lexer) -> usize {
    // expose pos via a method — add to Lexer
    lx.pos()
}
