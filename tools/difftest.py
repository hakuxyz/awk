#!/usr/bin/env python3
"""Differential test: our awk vs system gawk on the same programs."""
import subprocess, sys, tempfile, os, shutil

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OURS = os.environ.get("AWK_BIN") or os.path.join(ROOT, "target", "release", "awk")
GAWK = os.environ.get("GAWK", "gawk")

TMP = tempfile.mkdtemp(prefix="awkdiff_")
AV1 = os.path.join(TMP, "av1.txt")   # two records
AV2 = os.path.join(TMP, "av2.txt")   # one record
COL = os.path.join(TMP, "col.txt")   # a:b
open(AV1, "w").write("a\nb\n")
open(AV2, "w").write("c\n")
open(COL, "w").write("a:b\n")
MISSING = os.path.join(TMP, "no-such-file")

cases = []


def add(prog, stdin="", opts=None, post=None):
    cases.append((prog, stdin, opts or [], post or []))


# --- basics ---
add('BEGIN { print "hi" }')
add('BEGIN { print 3+4*2, 10%3, 2^10 }')
add('BEGIN { s=0; for (i=1; i<=10; i++) s+=i; print s }')
add('BEGIN { print 1/3, 2/3, 10/4 }')
add('BEGIN { printf "%d %5.2f %-6s|%s|\\n", 42, 3.14159, "ab", "x" }')
add('BEGIN { printf "%o %x %X %e %g %c %%\\n", 8, 255, 255, 1234.5, 0.000012, 65 }')
add('BEGIN { printf "%*d|%-*s|\\n", 5, 42, 8, "hi" }')

# --- fields / input ---
add('{ print $1, $NF, NF, NR }', "a b c\nd e\n")
add('{ print $0 }', "  spaced  out  \n")
add('{ $2 = "X"; print }', "a b c\n")
add('{ print $(NF+1) }', "a b\n")
add('{ print length($0), length }', "hello\n")
add('{ print $1 $2 }', "ab cd\n")

# --- FS ---
add('{ print NF, $2 }', "a1b22c\n", ["-F", "1|2+"])
add('{ print NF, $1, $3 }', "x::y::z\n", ["-F", ":"])
add('{ print NF, $2 }', "  a  b  c  \n", ["-F", " "])
add('{ print NF, $2 }', "aXXbXXc\n", ["-F", "XX"])
add('BEGIN { FS = "[0-9]+" } { print NF, $2 }', "a1b22c\n")
add('{ print NF }', "one two three\n")

# --- regex ---
add('/foo/{ print "hit", NR }', "foo\nbar\nfoobar\n")
add('$1 ~ /^a/{ print }', "abc\nxyz\nadc\n")
add('$0 !~ /b/{ print }', "abc\nxyz\n")
add('BEGIN { print ("abc" ~ /b/), ("abc" ~ /z/) }')
add('BEGIN { print match("hello world", /o /), RSTART, RLENGTH }')
add('BEGIN { n = split("a1b22c", arr, /[0-9]+/); print n; for (i=1; i<=n; i++) printf "[%s]", arr[i]; print "" }')
add('BEGIN { s = "the cat sat"; gsub(/cat/, "dog", s); print s }')
add('BEGIN { s = "aaa"; print gsub(/a/, "b", s), s }')
add('BEGIN { s = "hello"; print sub(/l/, "L", s), s }')
add('BEGIN { print gensub(/(.)(.)/ "[\\2\\1]", "g", "abcd") }')

# --- user functions ---
add('BEGIN { print f(2, 3) } function f(a, b) { return a * b }')
add('BEGIN { print f(1) } function f(a, b) { return "[" a "][" b "]" }')
add('function fib(n) { return n < 2 ? n : fib(n-1) + fib(n-2) } BEGIN { print fib(12) }')

# --- strings ---
add('BEGIN { print toupper("aBc"), tolower("AbC") }')
add('BEGIN { print index("hello", "ll"), substr("hello", 2, 3), substr("hello", 4) }')
add('BEGIN { print length("héllo") }')
add('BEGIN { x = " 7 "; print x + 1, x == 7, typeof(x) }')
add('BEGIN { print (1 == "1"), ("1" == " 1"), (1 == "a") }')
add('BEGIN { printf "%s|%d|\\n", "12abc", "12abc" }')

# --- arrays ---
add('BEGIN { a["x"] = 1; a["y"] = 2; for (k in a) print k, a[k] }')
add('BEGIN { for (i=1; i<=5; i++) a[i] = i*i; print a[3]; delete a[3]; print (3 in a), length(a) }')
add('BEGIN { a[1]=1; a[2]=2; print asort(a); print a[1], a[2] }')
add('BEGIN { a["b"]=1; a["a"]=2; print asorti(a); print a[1], a[2] }')
add('BEGIN { a[1,2] = "multi"; print a[1,2] }')
add('BEGIN { SUBSEP = ":"; a[1,2] = "m"; print a[1,2] }')

# --- getline / redirection ---
add('BEGIN { while ((getline line < "/etc/hostname") > 0) n++; print n + 0 }')
add('{ print }', "x\ny\n")
add('NR == 1 { print "first" } END { print NR }', "a\nb\nc\n")
add('{ print > "/dev/null" } END { print "done" }', "a\nb\n")
add('BEGIN { print "pipe" | "cat" }')
add('BEGIN { cmd = "echo from-cmd"; cmd | getline v; print v; close(cmd) }')

# --- GNU extensions ---
add('BEGIN { print strtonum("0x1F"), strtonum("010") }')
add('BEGIN { print typeof(1), typeof("s"), typeof($0) }')
add('BEGIN { print compl(0), and(6, 3), or(4, 1), xor(5, 3), lshift(1, 4), rshift(16, 2) }')
add('{ print typeof($1), typeof($9) }', "a b\n")
add('BEGIN { print isarray(a), typeof(a) }')
add('BEGIN { print (mktime("2001 02 03 04 05 06") > 0) }')
add('BEGIN { print (systime() > 0) }')
add('BEGIN { srand(42); r1 = rand(); srand(42); r2 = rand(); print (r1 == r2) }')
add('BEGIN { IGNORECASE = 1; print ("ABC" ~ /abc/) }')
add('{ print FILENAME != "", FNR, NR }', "a\nb\n", [])
add('BEGIN { print ENVIRON["HOME"] != "" }')

# --- control flow ---
add('BEGIN { if (1) print "t"; else print "f" }')
add('BEGIN { x = 0; while (x < 3) x++; print x }')
add('BEGIN { do { x++ } while (x < 4); print x }')
add('BEGIN { for (i=0; i<5; i++) { if (i==2) continue; if (i==4) break; printf "%d", i } print "" }')
add('BEGIN { exit 3 }')
add('BEGIN { print "a"; exit; print "b" }')
add('{ next } { print "never" }', "x\n")
add('BEGIN { x = 1; y = x > 0 ? "pos" : "neg"; print y }')
add('BEGIN { print 1 ? 2 : 3, 0 ? "y" : "n" }')

# --- ranges ---
add('/start/,/end/{ print NR }', "a\nstart\nb\nend\nc\n")
add('NR==2, NR==3{ print }', "a\nb\nc\nd\n")

# --- comparisons / types ---
add('BEGIN { print (2 < 10), ("2" < "10"), (2 < "10") }')
add('BEGIN { print (1 == 1.0), ("abc" < "abd") }')
add('BEGIN { x = "10"; y = 9; print (x > y) }')

# --- printf edge ---
add('BEGIN { printf "%5s|\\n", "ab" }')
add('BEGIN { printf "%.3f|\\n", 2 }')
add('BEGIN { printf "%d|\\n", 3.99 }')
add('BEGIN { printf "%c|\\n", 65 }')
add('BEGIN { printf "%i %u\\n", -5, 5 }')
add('BEGIN { printf "%g %g %g\\n", 100000, 1000000, 0.0001 }')

# --- misc builtins ---
add('BEGIN { print int(3.9), int(-3.9), sqrt(16), log(1), exp(0) }')
add('BEGIN { print sin(0), cos(0), (atan2(1, 1) > 0) }')
add('BEGIN { x = "abc"; y = x; print (x == y) }')
add('BEGIN { printf "%s\\n", 1/3 }')
add('BEGIN { OFS = "-"; print "a", "b", "c" }')
add('BEGIN { ORS = "|"; print "a"; print "b" }')
add('BEGIN { CONVFMT = "%.2f"; x = 3.14159; print x "" }')
add('BEGIN { OFMT = "%.2f"; print 3.14159 }')
add('BEGIN { print 1e10, 1e-10, 0.1 + 0.2 }')
add('BEGIN { print -0, 100000000000000000000 }')

# --- ARGC / ARGV / FILENAME / operands ---
add('BEGIN { print ARGC, ARGV[0] != "" }')
add('BEGIN { print ARGC, ARGV[1], ARGV[2] }', "", [], ["a", "b"])
add('BEGIN { for (i = 1; i < ARGC; i++) printf "[%s]", ARGV[i]; print "" }', "", ["-v", "q=1"], ["x=7", "f.txt"])
add('{ print FILENAME, ARGIND }', "x\n", [], [])
add('{ print FILENAME, ARGIND }', "x\n", [], ["-"])
add('{ print FILENAME, ARGIND }', "", [], [AV1])
add('{ print FILENAME, ARGIND }', "", [], [AV1, AV2])
add('BEGIN { print ARGC }', "", ["-v", "q=1"], [])
add('{ print "[" x "]" }', "a\n", [], ["x=7"])
add('{ print "[" x "]" }', "", [], ["x=7", AV1])
add('END { print "[" x "]" }', "a\n", [], [AV1, "x=7"])
add('END { print "[" v "]" }', "a\n", [], ["v=a=b"])
add('END { print length(x) }', "a\n", [], ["x=a\\tb"])
add('END { print (x == 7), typeof(x) }', "a\n", [], ["x=7"])
add('BEGIN { print "[" x "]" }', "", [], ["x=7"])
add('BEGIN { ARGC = 1 } { print "got", $0 }', "z\n", [], [AV1])
add('BEGIN { ARGV[1] = "" } { print "got", $0 }', "z\n", [], [AV1])
add('BEGIN { delete ARGV[1] } { print "got", $0 }', "z\n", [], [AV1])
add('BEGIN { ARGV[1] = "%s" } { print FILENAME, $0 }' % AV2, "", [], [AV1])
add('{ print "got", $0 }', "", [], ["a[1]=5", AV1])
add('BEGIN { print ARGC }', "", [], ["1x=5"])
add('BEGIN { print ARGC }', "", [], ["=x=7"])
add('END { print NR }', "", [], [AV1])
add('BEGIN { print NR }', "", [], [AV1])
add('BEGIN { print "ok" }', "", [], [MISSING])
add('{ print }', "", [], [MISSING])

# --- option parsing order ---
add('{ print NF }', "a:b\n", ["-F:"])
add('{ print NF }', "a:b\n", [], ["-F:"])
add('BEGIN { print ARGC }', "", [], ["-F:"])
add('{ print NF }', "a:b\n", [], ["FS=:"])
add('BEGIN { print "x=" x }', "", ["-v", "x=1"], [])

# --- syntax errors: gawk exits 1 ---
add('BEGIN {')
add('BEGIN { print "a\nb" }')          # raw newline inside a string
add('BEGIN { print /a }')              # unterminated regex


def run(binary, prog, stdin, opts, post):
    p = subprocess.run([binary] + opts + [prog] + post, input=stdin.encode(),
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
    return p.returncode, p.stdout.decode(errors="replace"), p.stderr.decode(errors="replace")


def norm(err):
    out = []
    for l in err.splitlines():
        if l.startswith(("gawk:", "awk:")):
            continue
        if l.startswith("\t") or l == "'" or l.lstrip().startswith("^"):
            continue
        out.append(l)
    return out


fails = 0
for i, (prog, stdin, opts, post) in enumerate(cases):
    rc_o, out_o, err_o = run(OURS, prog, stdin, opts, post)
    rc_g, out_g, err_g = run(GAWK, prog, stdin, opts, post)
    problems = []
    if out_o != out_g:
        problems.append("stdout\n  gawk: %r\n  ours: %r" % (out_g[:300], out_o[:300]))
    if rc_o != rc_g:
        problems.append("exit code gawk=%d ours=%d" % (rc_g, rc_o))
    n_o, n_g = norm(err_o), norm(err_g)
    if n_o != n_g:
        problems.append("stderr\n  gawk: %r\n  ours: %r" % (n_g[:300], n_o[:300]))
    if problems:
        fails += 1
        print("FAIL %d: %s" % (i, prog))
        for p in problems:
            print("  " + p)

shutil.rmtree(TMP, ignore_errors=True)
print("%d cases, %d failures" % (len(cases), fails))
sys.exit(1 if fails else 0)
