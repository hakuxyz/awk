//! Token stream for awk source.

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Num(f64),
    Str(String),
    Regex(String),
    Ident(String),
    Eof,
    Begin,
    End,
    If,
    Else,
    While,
    Do,
    For,
    In,
    Break,
    Continue,
    Next,
    Exit,
    Function,
    Return,
    Delete,
    Getline,
    Nextfile,
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Semicolon,
    Colon,
    Question,
    Newline,
    Assign,
    PlusAssign,
    MinusAssign,
    MulAssign,
    DivAssign,
    ModAssign,
    PowAssign,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    StarStar,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Match,
    NoMatch,
    AndAnd,
    OrOr,
    Not,
    Inc,
    Dec,
    Dollar,
    // printed by parser only
    Concat,
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
    pending_newline: bool,
    // whether a `/` at the current position starts a regex literal
    prev_regex_ok: bool,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer { src: src.as_bytes(), pos: 0, line: 1, pending_newline: false, prev_regex_ok: true }
    }

    pub fn line(&self) -> usize {
        self.line
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.src.get(self.pos + 1).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.pos += 1;
        if c == b'\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(b' ') | Some(b'\t') | Some(b'\r') => {
                    self.pos += 1;
                }
                Some(b'\\') if self.peek2() == Some(b'\n') => {
                    self.pos += 2;
                    self.line += 1;
                }
                Some(b'#') => {
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    /// Next token; Newline tokens separate statements when needed.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<(Tok, usize), String> {
        let r = self.next_tok()?;
        self.prev_regex_ok = regex_ok_after(&r.0);
        Ok(r)
    }

    fn next_tok(&mut self) -> Result<(Tok, usize), String> {
        if self.pending_newline {
            self.pending_newline = false;
            return Ok((Tok::Newline, self.line));
        }
        self.skip_ws_and_comments();
        let start_line = self.line;
        let c = match self.peek() {
            None => return Ok((Tok::Eof, start_line)),
            Some(c) => c,
        };

        // newlines are significant
        if c == b'\n' {
            self.pos += 1;
            self.line += 1;
            return Ok((Tok::Newline, start_line));
        }

        // string literal
        if c == b'"' {
            return self.lex_string(start_line);
        }

        // regex literal in match context is handled by parser calling lex_regex;
        // here / is division. Parser will re-lex when expecting regex.

        // number
        if c.is_ascii_digit() || (c == b'.' && self.peek2().is_some_and(|d| d.is_ascii_digit())) {
            return self.lex_number(start_line);
        }

        // identifier / keyword
        if c.is_ascii_alphabetic() || c == b'_' {
            let begin = self.pos;
            while let Some(ch) = self.peek() {
                if ch.is_ascii_alphanumeric() || ch == b'_' {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            let s = std::str::from_utf8(&self.src[begin..self.pos]).unwrap().to_string();
            if let Some(k) = keyword(&s) {
                return Ok((k, start_line));
            }
            return Ok((Tok::Ident(s), start_line));
        }

        // operators
        self.pos += 1;
        let two = |lx: &Self, a: u8, b: u8| lx.peek() == Some(a) || (lx.peek() == Some(b));
        let _ = two;
        match c {
            b'{' => Ok((Tok::LBrace, start_line)),
            b'}' => Ok((Tok::RBrace, start_line)),
            b'(' => Ok((Tok::LParen, start_line)),
            b')' => Ok((Tok::RParen, start_line)),
            b'[' => Ok((Tok::LBracket, start_line)),
            b']' => Ok((Tok::RBracket, start_line)),
            b',' => Ok((Tok::Comma, start_line)),
            b';' => Ok((Tok::Semicolon, start_line)),
            b':' => Ok((Tok::Colon, start_line)),
            b'?' => Ok((Tok::Question, start_line)),
            b'$' => Ok((Tok::Dollar, start_line)),
            b'+' => {
                if self.peek() == Some(b'+') {
                    self.pos += 1;
                    Ok((Tok::Inc, start_line))
                } else if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Ok((Tok::PlusAssign, start_line))
                } else {
                    Ok((Tok::Plus, start_line))
                }
            }
            b'-' => {
                if self.peek() == Some(b'-') {
                    self.pos += 1;
                    Ok((Tok::Dec, start_line))
                } else if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Ok((Tok::MinusAssign, start_line))
                } else {
                    Ok((Tok::Minus, start_line))
                }
            }
            b'*' => {
                if self.peek() == Some(b'*') {
                    self.pos += 1;
                    Ok((Tok::StarStar, start_line))
                } else if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Ok((Tok::MulAssign, start_line))
                } else {
                    Ok((Tok::Star, start_line))
                }
            }
            b'/' => {
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Ok((Tok::DivAssign, start_line))
                } else if self.prev_regex_ok {
                    // step back over the slash already consumed, then lex a regex
                    self.pos -= 1;
                    self.lex_regex()
                } else {
                    Ok((Tok::Slash, start_line))
                }
            }
            b'%' => {
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Ok((Tok::ModAssign, start_line))
                } else {
                    Ok((Tok::Percent, start_line))
                }
            }
            b'^' => {
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Ok((Tok::PowAssign, start_line))
                } else {
                    Ok((Tok::Caret, start_line))
                }
            }
            b'=' => {
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Ok((Tok::Eq, start_line))
                } else {
                    Ok((Tok::Assign, start_line))
                }
            }
            b'!' => {
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Ok((Tok::Ne, start_line))
                } else if self.peek() == Some(b'~') {
                    self.pos += 1;
                    Ok((Tok::NoMatch, start_line))
                } else {
                    Ok((Tok::Not, start_line))
                }
            }
            b'<' => {
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Ok((Tok::Le, start_line))
                } else {
                    Ok((Tok::Lt, start_line))
                }
            }
            b'>' => {
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Ok((Tok::Ge, start_line))
                } else {
                    Ok((Tok::Gt, start_line))
                }
            }
            b'~' => Ok((Tok::Match, start_line)),
            b'&' => {
                if self.peek() == Some(b'&') {
                    self.pos += 1;
                    Ok((Tok::AndAnd, start_line))
                } else {
                    Err(format!("line {}: lone &", start_line))
                }
            }
            b'|' => {
                if self.peek() == Some(b'|') {
                    self.pos += 1;
                    Ok((Tok::OrOr, start_line))
                } else {
                    // single | used by print | cmd
                    Ok((Tok::Ident("|".into()), start_line))
                }
            }
            other => Err(format!("line {}: unexpected byte {:?}", start_line, other)),
        }
    }

    /// Lex a string starting at opening quote.
    fn lex_string(&mut self, line: usize) -> Result<(Tok, usize), String> {
        self.pos += 1; // opening "
        let mut out = String::new();
        loop {
            let c = self.bump().ok_or_else(|| format!("line {}: unterminated string", line))?;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = self.bump().ok_or_else(|| format!("line {}: bad escape", line))?;
                    match e {
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'f' => out.push('\u{c}'),
                        b'b' => out.push('\u{8}'),
                        b'\\' => out.push('\\'),
                        b'"' => out.push('"'),
                        b'a' => out.push('\u{7}'),
                        b'v' => out.push('\u{b}'),
                        b'/' => out.push('/'),
                        b'\n' => { /* line continuation */ }
                        b'0'..=b'7' => {
                            let mut v = (e - b'0') as u32;
                            for _ in 0..2 {
                                if let Some(d @ b'0'..=b'7') = self.peek() {
                                    v = v * 8 + (d - b'0') as u32;
                                    self.pos += 1;
                                }
                            }
                            out.push(char::from_u32(v).unwrap_or('\0'));
                        }
                        b'x' => {
                            let mut v = 0u32;
                            let mut n = 0;
                            while n < 2 {
                                if let Some(d) = self.peek().and_then(|c| (c as char).to_digit(16)) {
                                    v = v * 16 + d;
                                    self.pos += 1;
                                    n += 1;
                                } else {
                                    break;
                                }
                            }
                            out.push(char::from_u32(v).unwrap_or('\0'));
                        }
                        other => out.push(other as char),
                    }
                }
                other => {
                    // preserve UTF-8 multibyte
                    if other < 0x80 {
                        out.push(other as char);
                    } else {
                        // put back and copy full utf8 sequence
                        self.pos -= 1;
                        let rest = &self.src[self.pos..];
                        let s = std::str::from_utf8(rest)
                            .map_err(|_| format!("line {}: invalid utf8", line))?;
                        let ch = s.chars().next().unwrap();
                        out.push(ch);
                        self.pos += ch.len_utf8();
                    }
                }
            }
        }
        Ok((Tok::Str(out), line))
    }

    fn lex_number(&mut self, line: usize) -> Result<(Tok, usize), String> {
        let begin = self.pos;
        // 0x1f style hex literal
        if self.peek() == Some(b'0')
            && matches!(self.peek2(), Some(b'x') | Some(b'X'))
            && self.src.get(self.pos + 2).copied().is_some_and(|c| c.is_ascii_hexdigit())
        {
            self.pos += 2;
            while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                self.pos += 1;
            }
            let s = std::str::from_utf8(&self.src[begin + 2..self.pos]).unwrap();
            let n = u64::from_str_radix(s, 16).unwrap_or(0);
            return Ok((Tok::Num(n as f64), line));
        }
        // gawk has no binary literals: 0b101 parses as 0
        if self.peek() == Some(b'0') && matches!(self.peek2(), Some(b'b') | Some(b'B'))
            && matches!(self.src.get(self.pos + 2), Some(b'0' | b'1'))
        {
            self.pos += 2;
            while matches!(self.peek(), Some(b'0') | Some(b'1')) {
                self.pos += 1;
            }
            return Ok((Tok::Num(0.0), line));
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'.' {
                self.pos += 1;
            } else if (c == b'e' || c == b'E')
                && matches!(self.peek2(), Some(d) if d.is_ascii_digit() || d == b'+' || d == b'-')
            {
                self.pos += 1;
                if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.src[begin..self.pos]).unwrap();
        let n: f64 = s.parse().map_err(|_| format!("line {}: bad number {}", line, s))?;
        Ok((Tok::Num(n), line))
    }

    /// Lex a regex literal after `~`, `!~`, or in pattern position.
    /// Caller has ensured next char is `/`.
    /// Step back over a `/` already consumed when producing Tok::Slash.
    pub fn back_slash(&mut self) {
        if self.pos > 0 && self.src[self.pos - 1] == b'/' {
            self.pos -= 1;
        }
    }

    pub fn lex_regex(&mut self) -> Result<(Tok, usize), String> {
        let line = self.line;
        debug_assert_eq!(self.peek(), Some(b'/'));
        self.pos += 1;
        let mut out = String::new();
        loop {
            let c = self.bump().ok_or_else(|| format!("line {}: unterminated regex", line))?;
            match c {
                b'/' => break,
                b'\\' => {
                    let e = self.bump().ok_or_else(|| format!("line {}: bad regex escape", line))?;
                    if e == b'\n' {
                        continue;
                    }
                    out.push('\\');
                    if e < 0x80 {
                        out.push(e as char);
                    } else {
                        self.pos -= 1;
                        let s = std::str::from_utf8(&self.src[self.pos..]).unwrap();
                        let ch = s.chars().next().unwrap();
                        out.push(ch);
                        self.pos += ch.len_utf8();
                    }
                }
                b'\n' => return Err(format!("line {}: unterminated regex", line)),
                other => {
                    if other < 0x80 {
                        out.push(other as char);
                    } else {
                        self.pos -= 1;
                        let s = std::str::from_utf8(&self.src[self.pos..]).unwrap();
                        let ch = s.chars().next().unwrap();
                        out.push(ch);
                        self.pos += ch.len_utf8();
                    }
                }
            }
        }
        // optional flags i, m - store as part of pattern string with (?i) prefix later
        let mut flags = String::new();
        while let Some(c) = self.peek() {
            if c == b'i' || c == b'm' || c == b'x' {
                flags.push(c as char);
                self.pos += 1;
            } else {
                break;
            }
        }
        let pattern = if flags.is_empty() {
            out
        } else {
            format!("(?{}){}", flags, out)
        };
        Ok((Tok::Regex(pattern), line))
    }

    /// Remember a newline to emit before next token (after `}` etc).
    pub fn push_newline(&mut self) {
        self.pending_newline = true;
    }
}

/// True when a `/` after this token starts a regex literal rather than
/// being the division operator.
fn regex_ok_after(t: &Tok) -> bool {
    match t {
        Tok::Num(_) | Tok::Str(_) | Tok::Regex(_) | Tok::RParen | Tok::RBracket
        | Tok::Inc | Tok::Dec => false,
        Tok::Ident(s) => matches!(s.as_str(), "print" | "printf"),
        _ => true,
    }
}

fn keyword(s: &str) -> Option<Tok> {
    Some(match s {
        "BEGIN" => Tok::Begin,
        "END" => Tok::End,
        "if" => Tok::If,
        "else" => Tok::Else,
        "while" => Tok::While,
        "do" => Tok::Do,
        "for" => Tok::For,
        "in" => Tok::In,
        "break" => Tok::Break,
        "continue" => Tok::Continue,
        "next" => Tok::Next,
        "exit" => Tok::Exit,
        "function" | "func" => Tok::Function,
        "return" => Tok::Return,
        "delete" => Tok::Delete,
        "getline" => Tok::Getline,
        "nextfile" => Tok::Nextfile,
        _ => return None,
    })
}
