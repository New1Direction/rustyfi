# Example: a calculator, Go → Rust

A small but real program — a command-line arithmetic evaluator with a lexer, a
recursive-descent parser (correct operator precedence, parentheses, unary minus,
right-associative `^`), and a tiny REPL. Three files across two packages:

```
calculator/
├── go.mod
├── main.go            # package main — CLI + REPL, imports calc
└── calc/
    ├── lexer.go       # package calc — TokenKind, Token, Lex()
    └── parser.go      # package calc — Parser, recursive descent
```

## Run it through Rustyfi

```bash
cd examples
zip -r /tmp/calculator.zip calculator
# drag /tmp/calculator.zip onto http://localhost:7410
```

## What comes out

A Cargo crate that **compiles clean (`cargo check` → 0 errors)** and behaves
identically to the Go original:

```
$ cargo run -- "2 + 3 * (4 - 1)"
11
$ cargo run -- "2 ^ 10"
1024
$ cargo run -- "(1 + 2) * (3 + 4)"
21
$ cargo run -- "10 / 4"
2.5
$ cargo run -- "2 +"
error: unexpected token: end of input   # exit 1
```

The two Go packages become `src/main.rs` and `src/calc/mod.rs`; `Token` and
`TokenKind` keep one canonical shape across `lexer.go` and `parser.go` (the
contract phase), and Go's `iota` enum becomes a real Rust `enum`. Operator
precedence, the `Result`-style error handling, and the REPL all survive.

This is the kind of project — pure logic, standard library, no framework or
native-library dependencies — where Rustyfi reliably lands a clean compile.
