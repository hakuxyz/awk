//! Abstract syntax tree for awk programs.

use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum Program {
    Rules(Vec<Rule>),
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub pattern: Option<Pattern>,
    pub action: Option<Stmt>,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Begin,
    End,
    Expr(Expr),
    // range pattern: expr1, expr2 - active until expr2 true
    Range(Expr, Expr),
    Regex(String),
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Block(Vec<Stmt>),
    Expr(Expr),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    While(Expr, Box<Stmt>),
    DoWhile(Box<Stmt>, Expr),
    For(
        Option<Box<Stmt>>,
        Option<Expr>,
        Option<Box<Stmt>>,
        Box<Stmt>,
    ),
    ForIn(Vec<String>, String, Box<Stmt>),
    Break,
    Continue,
    Next,
    Nextfile,
    Exit(Option<Expr>),
    Return(Option<Expr>),
    Delete(String, Option<Expr>),
    Print(Vec<Expr>, Option<Redir>),
    Printf(Vec<Expr>, Option<Redir>),
    Getline(Box<Expr>, Option<GetlineSrc>),
    Empty,
}

#[derive(Debug, Clone)]
pub enum Redir {
    Trunc(Expr),  // >
    Append(Expr), // >>
    Pipe(Expr),   // |
}

#[derive(Debug, Clone)]
pub enum GetlineSrc {
    // getline var < file
    File(Expr),
    // getline var | cmd
    Cmd(Expr),
    // plain getline / getline var / cmd | getline / file | getline
    None,
    VarFile(Expr, Expr), // handled via Getline with var
}

#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
    Str(String),
    Regex(String),
    Var(String),
    Field(Box<Expr>),
    Index(Box<Expr>, Box<Expr>), // a[i]
    Sublist(Vec<Expr>),          // a[i,j] subscript joined with SUBSEP
    Assign(Box<Expr>, Box<Expr>),
    PlusAssign(Box<Expr>, Box<Expr>),
    MinusAssign(Box<Expr>, Box<Expr>),
    MulAssign(Box<Expr>, Box<Expr>),
    DivAssign(Box<Expr>, Box<Expr>),
    ModAssign(Box<Expr>, Box<Expr>),
    PowAssign(Box<Expr>, Box<Expr>),
    PostInc(Box<Expr>),
    PostDec(Box<Expr>),
    PreInc(Box<Expr>),
    PreDec(Box<Expr>),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
    Mod(Box<Expr>, Box<Expr>),
    Pow(Box<Expr>, Box<Expr>),
    Concat(Box<Expr>, Box<Expr>),
    Lt(Box<Expr>, Box<Expr>),
    Le(Box<Expr>, Box<Expr>),
    Gt(Box<Expr>, Box<Expr>),
    Ge(Box<Expr>, Box<Expr>),
    Eq(Box<Expr>, Box<Expr>),
    Ne(Box<Expr>, Box<Expr>),
    Match(Box<Expr>, Box<Expr>),
    NoMatch(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
    // builtin calls handled as Call with special names
    InArray(Box<Expr>, String), // x in arr - as expression
}

/// Function definition.
#[derive(Debug, Clone)]
pub struct FuncDef {
    pub name: String,
    pub params: Vec<String>,
    pub body: Rc<Stmt>,
}
