// Command calculator evaluates arithmetic expressions from the command line.
//
//	calculator "2 + 3 * (4 - 1)"     # → 11
//	echo "2 ^ 10" | calculator        # → 1024
//
// With no arguments it starts a tiny REPL reading one expression per line.
package main

import (
	"bufio"
	"fmt"
	"os"
	"strings"

	"example.com/calculator/calc"
)

// evaluate lexes and parses a single expression, returning its value.
func evaluate(expr string) (float64, error) {
	tokens, err := calc.Lex(expr)
	if err != nil {
		return 0, err
	}
	parser := calc.NewParser(tokens)
	return parser.Parse()
}

func main() {
	args := os.Args[1:]
	if len(args) > 0 {
		expr := strings.Join(args, " ")
		result, err := evaluate(expr)
		if err != nil {
			fmt.Fprintf(os.Stderr, "error: %v\n", err)
			os.Exit(1)
		}
		fmt.Printf("%g\n", result)
		return
	}

	fmt.Println("calc — type an expression, or Ctrl-D to quit")
	scanner := bufio.NewScanner(os.Stdin)
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if line == "" {
			continue
		}
		result, err := evaluate(line)
		if err != nil {
			fmt.Printf("error: %v\n", err)
			continue
		}
		fmt.Printf("= %g\n", result)
	}
}
