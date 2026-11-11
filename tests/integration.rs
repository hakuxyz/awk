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
    let (out, _) = awk(
        r#"function dbl(x) { return x*2 } BEGIN { print dbl(21) }"#,
        "",
    );
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

// ----- GNU extensions and edge cases -----

#[test]
fn hex_literal() {
    let (out, _) = awk(r#"BEGIN { printf "%c|%d\n", 0x263a, 0x41 }"#, "");
    assert_eq!(out, "\u{263a}|65\n");
}

#[test]
fn binary_literal_is_zero() {
    // gawk has no 0b literals and evaluates them to 0
    let (out, _) = awk(r#"BEGIN { print 0b101 }"#, "");
    assert_eq!(out, "0\n");
}

#[test]
fn gensub_group_swap() {
    let (out, _) = awk(
        r#"BEGIN { print gensub(/(.)(.)/, "[\\2\\1]", "g", "abcd") }"#,
        "",
    );
    assert_eq!(out, "[ba][dc]\n");
}

#[test]
fn gensub_nth_match() {
    let (out, _) = awk(r#"BEGIN { print gensub(/a/, "X", 3, "aaaa") }"#, "");
    assert_eq!(out, "aaXa\n");
}

#[test]
fn nextfile_skips_rest_of_file() {
    let exe = env!("CARGO_BIN_EXE_awk");
    let dir = std::env::temp_dir();
    let f1 = dir.join("awk_nf1.txt");
    let f2 = dir.join("awk_nf2.txt");
    std::fs::write(&f1, "alpha\nbeta\n").unwrap();
    std::fs::write(&f2, "gamma\n").unwrap();
    let out = Command::new(exe)
        .args([
            "{ print $0; nextfile } END { print NR }",
            f1.to_str().unwrap(),
            f2.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "alpha\ngamma\n2\n");
    let _ = std::fs::remove_file(&f1);
    let _ = std::fs::remove_file(&f2);
}

#[test]
fn getline_file_handle_stays_open_at_eof() {
    let exe = env!("CARGO_BIN_EXE_awk");
    let dir = std::env::temp_dir();
    let f = dir.join("awk_gl.txt");
    std::fs::write(&f, "one\ntwo\n").unwrap();
    let prog = format!(
        "BEGIN {{ n=0; while ((getline l < \"{}\") > 0) n++; print n; print (getline z < \"{}\"); close(\"{}\"); print (getline l < \"{}\"), l }}",
        f.display(),
        f.display(),
        f.display(),
        f.display()
    );
    let out = Command::new(exe).arg(&prog).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "2\n0\n1 one\n");
    let _ = std::fs::remove_file(&f);
}

#[test]
fn division_by_zero_is_fatal() {
    let (out, code) = awk(r#"BEGIN { print 1/0 }"#, "");
    assert_eq!(out, "");
    assert_eq!(code, 2);
}

#[test]
fn modulo_by_zero_is_fatal() {
    let (_, code) = awk(r#"BEGIN { print 5%0 }"#, "");
    assert_eq!(code, 2);
}

#[test]
fn user_function_takes_fewer_args() {
    let (out, code) = awk(
        r#"BEGIN { print f(1) } function f(a, b) { return a + b }"#,
        "",
    );
    assert_eq!(out, "1\n");
    assert_eq!(code, 0);
}

#[test]
fn typeof_distinguishes_unassigned() {
    let (out, _) = awk(r#"BEGIN { print typeof(u), typeof($9) }"#, "");
    assert_eq!(out, "untyped unassigned\n");
}

#[test]
fn typeof_array_element() {
    let (out, _) = awk(r#"BEGIN { a[1]; print typeof(a[1]) }"#, "");
    assert_eq!(out, "unassigned\n");
}

#[test]
fn rstart_counts_characters() {
    let (out, _) = awk(
        r#"BEGIN { print match("héllo", /l+/), RSTART, RLENGTH }"#,
        "",
    );
    assert_eq!(out, "3 3 2\n");
}

#[test]
fn infinity_prints_with_sign() {
    let (out, _) = awk(r#"BEGIN { x = 1e308 * 10; print x, -x, (x - x) }"#, "");
    assert_eq!(out, "+inf -inf -nan\n");
}

#[test]
fn printf_literal_percent() {
    let (out, _) = awk(r#"BEGIN { printf "100%% sure\n" }"#, "");
    assert_eq!(out, "100% sure\n");
}

#[test]
fn printf_unknown_spec_is_literal() {
    let (out, _) = awk(r#"BEGIN { printf "%p|%q|%%\n", 65 }"#, "");
    assert_eq!(out, "%p|%q|%\n");
}

#[test]
fn v_value_with_blanks_is_strnum() {
    let exe = env!("CARGO_BIN_EXE_awk");
    let out = Command::new(exe)
        .args(["-v", "x= 7 ", "BEGIN { print typeof(x) }"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "strnum\n");
}

#[test]
fn gsub_literal_target_only_counts() {
    let (out, code) = awk(r#"BEGIN { print gsub(/a/, "b", "aabbcc") }"#, "");
    assert_eq!(out, "2\n");
    assert_eq!(code, 0);
}

#[test]
fn asorti_sorts_index_strings() {
    let (out, _) = awk(
        r#"BEGIN { a[10]="x"; a[9]="y"; a[2]="z"; print asorti(a); print a[1], a[2], a[3] }"#,
        "",
    );
    assert_eq!(out, "3\n10 2 9\n");
}

#[test]
fn multi_subscript_joins_with_subsep() {
    let (out, _) = awk(
        r#"BEGIN { a[1,2] = "x"; print a[1,2]; print (1,2) in a; delete a[1,2]; print length(a) }"#,
        "",
    );
    assert_eq!(out, "x\n1\n0\n");
}

#[test]
fn strtonum_prefixes() {
    let (out, _) = awk(
        r#"BEGIN { print strtonum("0x1F"), strtonum("010"), strtonum("018"), strtonum("0b101") }"#,
        "",
    );
    assert_eq!(out, "31 8 18 0\n");
}

#[test]
fn strftime_and_mktime_roundtrip() {
    let (out, _) = awk(
        r#"BEGIN { t = mktime("2001 02 03 04 05 06"); print strftime("%Y%m%d%H%M%S", t) }"#,
        "",
    );
    assert_eq!(out, "20010203040506\n");
}

#[test]
fn bitwise_gnu_functions() {
    // compl masks to 53 bits, so compl(0) is 2^53 - 1, exactly like gawk
    let (out, _) = awk(
        r#"BEGIN { print compl(0), and(5, 3), or(1, 2), xor(5, 3), lshift(1, 4), rshift(16, 2) }"#,
        "",
    );
    assert_eq!(out, "9007199254740991 1 3 6 16 4\n");
}
