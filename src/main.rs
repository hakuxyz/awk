//! awk CLI: POSIX-compatible entry point.

use awk::interp::Interp;
use awk::parser::parse_full;
use awk::value::Scalar;
use awk::Value;
use std::io::Write;

/// Process backslash escapes in a -v value or -F field separator.
fn unescape(s: &str) -> String {
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

fn usage() -> ! {
    eprintln!(
        "usage: awk [-F fs] [-v var=value] [-f progfile | 'prog'] [file ...]\n\
         \n\
         \t-F fs\t\tfield separator\n\
         \t-v var=value\tassign variable before execution\n\
         \t-f progfile\tread program from file\n\
         \t--version\tprint version"
    );
    std::process::exit(2);
}

/// Restore the default SIGPIPE action. The Rust runtime ignores SIGPIPE, so
/// writing into a closed pipe would surface as an error instead of ending the
/// process quietly the way gawk and every other filter do.
#[cfg(unix)]
fn reset_sigpipe() {
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    unsafe extern "C" {
        fn signal(sig: i32, handler: usize) -> usize;
    }
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}

fn main() {
    reset_sigpipe();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut fs: Option<String> = None;
    let mut assigns: Vec<(String, String)> = Vec::new();
    let mut prog_file: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    let mut prog_src: Option<String> = None;

    while i < args.len() {
        let a = &args[i];
        if a == "--version" {
            println!("awk {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        } else if a == "-h" || a == "--help" {
            usage();
        } else if a == "-F" {
            i += 1;
            if i >= args.len() {
                usage();
            }
            fs = Some(args[i].clone());
        } else if let Some(rest) = a.strip_prefix("-F") {
            if rest.is_empty() {
                usage();
            }
            fs = Some(rest.to_string());
        } else if a == "-v" {
            i += 1;
            if i >= args.len() {
                usage();
            }
            if let Some((k, v)) = args[i].split_once('=') {
                assigns.push((k.to_string(), v.to_string()));
            } else {
                usage();
            }
        } else if let Some(rest) = a.strip_prefix("-v") {
            if let Some((k, v)) = rest.split_once('=') {
                assigns.push((k.to_string(), v.to_string()));
            } else {
                usage();
            }
        } else if a == "-f" {
            i += 1;
            if i >= args.len() {
                usage();
            }
            prog_file = Some(args[i].clone());
        } else if let Some(rest) = a.strip_prefix("-f") {
            if !rest.is_empty() {
                prog_file = Some(rest.to_string());
            } else {
                usage();
            }
        } else if a == "--" {
            i += 1;
            positional.extend_from_slice(&args[i..]);
            break;
        } else if a.starts_with('-') && a.len() > 1 && a != "-" {
            // unknown option
            eprintln!("awk: unrecognized option {}", a);
            std::process::exit(2);
        } else {
            // first non-option is program if no -f
            if prog_file.is_none() && prog_src.is_none() {
                prog_src = Some(a.clone());
            } else {
                positional.push(a.clone());
            }
        }
        i += 1;
    }

    let source = if let Some(f) = prog_file {
        match std::fs::read_to_string(&f) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("awk: cannot read {}: {}", f, e);
                std::process::exit(2);
            }
        }
    } else if let Some(p) = prog_src {
        p
    } else {
        usage();
    };

    let (funcs, prog) = match parse_full(&source) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("awk: {}", e);
            std::process::exit(2);
        }
    };

    let awk::ast::Program::Rules(rules) = prog;

    let mut interp = Interp::new(funcs, rules);
    if let Some(f) = fs {
        interp.fs = unescape(&f);
    }
    for (k, v) in assigns {
        // -v values look like input fields: numeric ones become strnum
        interp.set_var(&k, Value::Scalar(Scalar::from_input(unescape(&v))));
    }

    let code = interp.run(&positional);
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
