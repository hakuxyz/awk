//! awk CLI: POSIX-compatible entry point.

use awk::Value;
use awk::interp::Interp;
use awk::parser::parse_full;
use awk::value::{Scalar, unescape};
use std::io::Write;

fn usage() -> ! {
    eprintln!(
        "usage: awk [-F fs] [-v var=value] [-f progfile | 'prog'] [file ...]\n\
         \n\
         \t-F fs\t\tfield separator\n\
         \t-v var=value\tassign variable before execution\n\
         \t-f progfile\tread program from file\n\
         \t--version\tprint version"
    );
    std::process::exit(1);
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
    // gawk parses options only up to the first operand; everything after
    // the program (or after the first file with -f) is an operand
    let mut operands = false;

    while i < args.len() {
        let a = &args[i];
        if operands {
            positional.push(a.clone());
            i += 1;
            continue;
        }
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
            std::process::exit(1);
        } else if prog_file.is_none() && prog_src.is_none() {
            // first non-option is the program
            prog_src = Some(a.clone());
            operands = true;
        } else {
            // with -f the first non-option is already a file operand
            positional.push(a.clone());
            operands = true;
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
            std::process::exit(1);
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
