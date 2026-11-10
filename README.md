# awk

A POSIX awk implementation in Rust: a drop-in `awk` for filtering and transforming text.

```console
$ echo "hello world" | awk '{print $2}'
world

$ awk -F: '{print $1, $3}' /etc/passwd
$ awk 'BEGIN { for (i = 1; i <= 3; i++) printf "%d ", i * i }'
$ seq 1 100 | awk '{ s += $1 } END { print s }'
5050
```

## Features

- Full POSIX awk: patterns, ranges, BEGIN/END, actions, user functions
- Field splitting with default, single-char, and regex `FS` (`-F '[0-9]+'`)
- Dynamic fields (`$100`), `NF`, `NR`, `FNR`, `FILENAME`, `OFS`, `ORS`, `SUBSEP`, `ENVIRON`
- String functions: `length`, `substr`, `index`, `split`, `sub`, `gsub`, `match`, `sprintf`, `toupper`, `tolower`
- Math: `int`, `sqrt`, `log`, `exp`, `sin`, `cos`, `atan2`, `rand`, `srand`
- I/O redirection: `>`, `>>`, `|` for both `print` and `getline`
- `getline` variants: plain, `var`, `< file`, `| cmd`
- `printf`/`sprintf` with POSIX conversions (`%d %i %o %x %X %f %e %g %s %c %a %%`), field widths, precision, flags, `*` width
- Dual string/number values with POSIX comparison rules (strnum for input fields)

## CLI

```
usage: awk [-F fs] [-v var=value] [-f progfile | 'prog'] [file ...]

    -F fs           field separator (char, string, or regex)
    -v var=value    assign variable before execution
    -f progfile     read program from file
    --version       print version
```

Exit codes: `0` success, `1` `exit 1`, `2` usage or runtime error.

## Install

```console
$ cargo install awk
```

## Library

The crate also exposes the lexer, parser, AST, value model, and interpreter for embedding:

```rust
let (funcs, prog) = awk::parser::parse_full(r#"BEGIN { print "hi" }"#).unwrap();
let mut interp = awk::interp::Interp::new(funcs, match prog {
    awk::ast::Program::Rules(r) => r,
});
let code = interp.run(&[]);
assert_eq!(code, 0);
```

## Not yet implemented

- `getline` from a closed file returns 0 (EOF) but no `-1` error propagation
- POSIX-only extensions beyond the standard (e.g. GNU `ARGIND`, `RT`, `gensub`) are not covered
- `delete` inside `for (k in arr)` follows POSIX; GNU-specific forms may differ

## License

MIT
