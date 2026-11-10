//! Integration tests: run the CLI over small programs and inputs.

use std::io::Write;
use std::process::{Command, Stdio};

/// Run `awk` with a program and stdin input, return (stdout, exit code).
fn awk(prog: &str, input: &str) -> (String, i32) {
    let exe = env!("CARGO_BIN_EXE_awk");
    let mut child = Command::new(exe)
        .arg(prog)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn awk");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait awk");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn begin_print() {
    let (out, code) = awk(r#"BEGIN { print "hi" }"#, "");
    assert_eq!(out, "hi\n");
    assert_eq!(code, 0);
}

#[test]
fn arithmetic() {
    let (out, _) = awk(r#"BEGIN { print 3+4*2, 10%3, 2^10 }"#, "");
    assert_eq!(out, "11 1 1024\n");
}

#[test]
fn for_sum() {
    let (out, _) = awk(r#"BEGIN { s=0; for (i=1; i<=10; i++) s+=i; print s }"#, "");
    assert_eq!(out, "55\n");
}

#[test]
fn user_function() {
    let (out, _) = awk(r#"function dbl(x) { return x*2 } BEGIN { print dbl(21) }"#, "");
    assert_eq!(out, "42\n");
}

#[test]
fn gsub_regex() {
    let (out, _) = awk(r#"BEGIN { s="aaa"; gsub(/a/, "b", s); print s }"#, "");
    assert_eq!(out, "bbb\n");
}

#[test]
fn printf_fmt() {
    let (out, _) = awk(r#"BEGIN { printf "%05d|%.2f\n", 42, 3.14159 }"#, "");
    assert_eq!(out, "00042|3.14\n");
}

#[test]
fn exit_code() {
    let (_, code) = awk(r#"BEGIN { exit 3 }"#, "");
    assert_eq!(code, 3);
}

#[test]
fn input_fields_and_nr() {
    let (out, code) = awk("{ print NR, NF, $1 }", "a b c\nd e f g\n");
    assert_eq!(out, "1 3 a\n2 4 d\n");
    assert_eq!(code, 0);
}

#[test]
fn regex_pattern() {
    let (out, _) = awk(r#"$0 ~ /foo/ { print "hit" }"#, "foo\nbar\nbaz\n");
    assert_eq!(out, "hit\n");
}

#[test]
fn end_accumulator() {
    let (out, _) = awk("{ s += $1 } END { print s }", "1\n2\n3\n");
    assert_eq!(out, "6\n");
}

#[test]
fn next_skips_record() {
    let (out, _) = awk("{ if ($1 == 2) next; print }", "1\n2\n3\n");
    assert_eq!(out, "1\n3\n");
}

#[test]
fn default_fs_whitespace() {
    let (out, _) = awk(r#"{ print "[" $1 "][" $2 "]" }"#, "  a   b  \n");
    assert_eq!(out, "[a][b]\n");
}

#[test]
fn dash_f_field_separator() {
    let exe = env!("CARGO_BIN_EXE_awk");
    let mut child = Command::new(exe)
        .args(["-F:", "{ print NF, $2 }"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"a:b:c\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "3 b\n");
    assert!(out.status.success());
}

#[test]
fn regex_field_separator() {
    let exe = env!("CARGO_BIN_EXE_awk");
    let mut child = Command::new(exe)
        .args(["-F[0-9]+", "{ print NF, $2, $3 }"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"a1b22c\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "3 b c\n");
    assert!(out.status.success());
}

#[test]
fn dash_v_assignment() {
    let exe = env!("CARGO_BIN_EXE_awk");
    let out = Command::new(exe)
        .args(["-v", "n=42", "BEGIN { print n * 2 }"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "84\n");
    assert!(out.status.success());
}

#[test]
fn dash_f_program_file() {
    let exe = env!("CARGO_BIN_EXE_awk");
    let dir = std::env::temp_dir();
    let path = dir.join("awk_test_prog.awk");
    std::fs::write(&path, "BEGIN { print \"file-prog\" }\n").unwrap();
    let out = Command::new(exe)
        .args(["-f", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "file-prog\n");
    assert!(out.status.success());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn gawk_differential_sum_squares() {
    // Compare against system awk if available.
    let sys = Command::new("awk").arg("--version").output();
    if sys.is_err() {
        return;
    }
    let prog = "{ s += $1 * $1 } END { printf \"%.3f\\n\", s }";
    let input = "1\n2\n3\n4\n";
    let (mine, c1) = awk(prog, input);
    let out = Command::new("awk")
        .arg(prog)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            c.stdin.take().unwrap().write_all(input.as_bytes())?;
            c.wait_with_output()
        })
        .unwrap();
    assert_eq!(c1, 0);
    assert_eq!(mine, String::from_utf8_lossy(&out.stdout));
}
