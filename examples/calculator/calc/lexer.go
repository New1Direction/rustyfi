// Package calc implements a small arithmetic expression evaluator: a lexer,
// a recursive-descent parser, and the precedence rules that tie them together.
package calc

import (
	"fmt"
	"strconv"
)

// TokenKind enumerates the kinds of tokens the lexer can produce.
type TokenKind int

const (
	TokNumber TokenKind = iota
	TokPlus
	TokMinus
	TokStar
	TokSlash
	TokCaret
	TokLParen
	TokRParen
	TokEOF
)

// Token is a single lexical unit. Value is meaningful only for TokNumber.
type Token struct {
	Kind  TokenKind
	Value float64
}

// Describe returns a human-readable label for a token kind, used in errors.
func Describe(k TokenKind) string {
	switch k {
	case TokNumber:
		return "number"
	case TokPlus:
		return "+"
	case TokMinus:
		return "-"
	case TokStar:
		return "*"
	case TokSlash:
		return "/"
	case TokCaret:
		return "^"
	case TokLParen:
		return "("
	case TokRParen:
		return ")"
	case TokEOF:
		return "end of input"
	default:
		return "unknown"
	}
}

// Lex turns an input string into a slice of tokens, terminated by TokEOF.
// It returns an error on the first unrecognized character.
func Lex(input string) ([]Token, error) {
	var tokens []Token
	runes := []rune(input)
	i := 0
	for i < len(runes) {
		c := runes[i]
		switch {
		case c == ' ' || c == '\t':
			i++
		case c == '+':
			tokens = append(tokens, Token{Kind: TokPlus})
			i++
		case c == '-':
			tokens = append(tokens, Token{Kind: TokMinus})
			i++
		case c == '*':
			tokens = append(tokens, Token{Kind: TokStar})
			i++
		case c == '/':
			tokens = append(tokens, Token{Kind: TokSlash})
			i++
		case c == '^':
			tokens = append(tokens, Token{Kind: TokCaret})
			i++
		case c == '(':
			tokens = append(tokens, Token{Kind: TokLParen})
			i++
		case c == ')':
			tokens = append(tokens, Token{Kind: TokRParen})
			i++
		case (c >= '0' && c <= '9') || c == '.':
			start := i
			for i < len(runes) && ((runes[i] >= '0' && runes[i] <= '9') || runes[i] == '.') {
				i++
			}
			text := string(runes[start:i])
			val, err := strconv.ParseFloat(text, 64)
			if err != nil {
				return nil, fmt.Errorf("invalid number: %q", text)
			}
			tokens = append(tokens, Token{Kind: TokNumber, Value: val})
		default:
			return nil, fmt.Errorf("unexpected character: %q", string(c))
		}
	}
	tokens = append(tokens, Token{Kind: TokEOF})
	return tokens, nil
}
