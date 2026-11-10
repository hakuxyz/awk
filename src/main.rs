//! awk CLI: POSIX-compatible entry point.

use awk::interp::Interp;
use awk::parser::parse_full;
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
    std::process::exit(2);
}

fn main() {
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
        interp.fs = f;
    }
    for (k, v) in assigns {
        match k.as_str() {
            "FS" => interp.fs = v,
            "OFS" => interp.ofs = v,
            "ORS" => interp.ors = v,
            "SUBSEP" => interp.subsep = v,
            _ => {
                interp.globals.insert(k, awk::Value::str(v));
            }
        }
    }

    let code = interp.run(&positional);
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
