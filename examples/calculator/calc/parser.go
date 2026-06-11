package calc

import "fmt"

// Parser evaluates a token stream using recursive descent with the usual
// arithmetic precedence: ^ binds tightest, then * and /, then + and -.
type Parser struct {
	tokens []Token
	pos    int
}

// NewParser creates a parser over the given tokens.
func NewParser(tokens []Token) *Parser {
	return &Parser{tokens: tokens, pos: 0}
}

func (p *Parser) peek() Token {
	return p.tokens[p.pos]
}

func (p *Parser) advance() Token {
	t := p.tokens[p.pos]
	p.pos++
	return t
}

// Parse evaluates the full expression and verifies all input was consumed.
func (p *Parser) Parse() (float64, error) {
	value, err := p.parseAddSub()
	if err != nil {
		return 0, err
	}
	if p.peek().Kind != TokEOF {
		return 0, fmt.Errorf("unexpected trailing token: %s", Describe(p.peek().Kind))
	}
	return value, nil
}

func (p *Parser) parseAddSub() (float64, error) {
	left, err := p.parseMulDiv()
	if err != nil {
		return 0, err
	}
	for p.peek().Kind == TokPlus || p.peek().Kind == TokMinus {
		op := p.advance().Kind
		right, err := p.parseMulDiv()
		if err != nil {
			return 0, err
		}
		if op == TokPlus {
			left += right
		} else {
			left -= right
		}
	}
	return left, nil
}

func (p *Parser) parseMulDiv() (float64, error) {
	left, err := p.parsePower()
	if err != nil {
		return 0, err
	}
	for p.peek().Kind == TokStar || p.peek().Kind == TokSlash {
		op := p.advance().Kind
		right, err := p.parsePower()
		if err != nil {
			return 0, err
		}
		if op == TokStar {
			left *= right
		} else {
			if right == 0 {
				return 0, fmt.Errorf("division by zero")
			}
			left /= right
		}
	}
	return left, nil
}

func (p *Parser) parsePower() (float64, error) {
	base, err := p.parseUnary()
	if err != nil {
		return 0, err
	}
	if p.peek().Kind == TokCaret {
		p.advance()
		exp, err := p.parsePower() // right-associative
		if err != nil {
			return 0, err
		}
		return intPow(base, exp), nil
	}
	return base, nil
}

func (p *Parser) parseUnary() (float64, error) {
	if p.peek().Kind == TokMinus {
		p.advance()
		v, err := p.parseUnary()
		if err != nil {
			return 0, err
		}
		return -v, nil
	}
	return p.parsePrimary()
}

func (p *Parser) parsePrimary() (float64, error) {
	t := p.advance()
	switch t.Kind {
	case TokNumber:
		return t.Value, nil
	case TokLParen:
		v, err := p.parseAddSub()
		if err != nil {
			return 0, err
		}
		if p.advance().Kind != TokRParen {
			return 0, fmt.Errorf("expected a closing parenthesis")
		}
		return v, nil
	default:
		return 0, fmt.Errorf("unexpected token: %s", Describe(t.Kind))
	}
}

// intPow raises base to an integer power via repeated multiplication, handling
// negative exponents by reciprocal. Good enough for a demo calculator.
func intPow(base float64, exp float64) float64 {
	result := 1.0
	n := int(exp)
	if n < 0 {
		for i := 0; i < -n; i++ {
			result /= base
		}
		return result
	}
	for i := 0; i < n; i++ {
		result *= base
	}
	return result
}
