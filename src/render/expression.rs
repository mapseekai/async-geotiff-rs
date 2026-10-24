//! Arithmetic band-math expression: parse `(b5-b4)/(b5+b4)` → RPN, eval per-pixel.
//! Grammar is arithmetic-only: + - * /, unary minus, parens, float literals, `b<N>`.
//! Numbers are plain decimals (`2.5`, `0.5`); scientific notation (`1e3`) is not supported.

use crate::error::{AgError, AgResult};

const MAX_EXPRESSION_BYTES: usize = 4096;
const MAX_EXPRESSION_TOKENS: usize = 256;

/// RPN token. `Band(i)` is 0-indexed (source `b1` → `Band(0)`).
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// A numeric constant.
    Num(f64),
    /// A zero-based source band index.
    Band(usize),
    /// Add the two preceding operands.
    Add,
    /// Subtract the right operand from the left operand.
    Sub,
    /// Multiply the two preceding operands.
    Mul,
    /// Divide the left operand by the right operand.
    Div,
    /// Negate the preceding operand.
    Neg,
}

/// Parse arithmetic and one-based band references (`b1`, `b2`, …) into RPN.
///
/// Supports parentheses, unary minus and `+`, `-`, `*`, `/`. Invalid syntax,
/// excessive nesting, more than 4096 input bytes or 256 lexical tokens returns
/// [`AgError::Query`]. Band availability is checked separately by the caller.
pub fn parse_expression(input: &str) -> AgResult<Vec<Token>> {
    if input.len() > MAX_EXPRESSION_BYTES {
        return Err(AgError::query("expression", "expression is too long"));
    }
    let toks = lex(input)?;
    if toks.len() > MAX_EXPRESSION_TOKENS {
        return Err(AgError::query(
            "expression",
            "expression has too many tokens",
        ));
    }
    let mut p = Parser {
        toks: &toks,
        pos: 0,
        out: Vec::new(),
        depth: 0,
    };
    p.expr()?;
    if p.pos != p.toks.len() {
        return Err(AgError::query("expression", "unexpected trailing input"));
    }
    if p.out.is_empty() {
        return Err(AgError::query("expression", "empty expression"));
    }
    Ok(p.out)
}

/// Highest 0-indexed band referenced, or None if no band refs.
pub fn max_band_ref(rpn: &[Token]) -> Option<usize> {
    rpn.iter()
        .filter_map(|t| match t {
            Token::Band(i) => Some(*i),
            _ => None,
        })
        .max()
}

/// Like [`eval_rpn`] but reuses a caller-provided scratch stack to avoid a
/// per-call heap allocation in hot per-pixel loops. `stack` is cleared on entry.
pub(crate) fn eval_rpn_with(rpn: &[Token], bands: &[f64], stack: &mut Vec<f64>) -> f64 {
    stack.clear();
    for t in rpn {
        match t {
            Token::Num(n) => stack.push(*n),
            Token::Band(i) => stack.push(bands.get(*i).copied().unwrap_or(f64::NAN)),
            Token::Neg => {
                let a = stack.pop().unwrap_or(f64::NAN);
                stack.push(-a);
            }
            op => {
                let b = stack.pop().unwrap_or(f64::NAN);
                let a = stack.pop().unwrap_or(f64::NAN);
                stack.push(match op {
                    Token::Add => a + b,
                    Token::Sub => a - b,
                    Token::Mul => a * b,
                    Token::Div => a / b,
                    _ => f64::NAN,
                });
            }
        }
    }
    stack.pop().unwrap_or(f64::NAN)
}

/// Evaluate RPN given per-pixel band values (`bands[i]` = value of source band i+1).
/// Caller must validate `max_band_ref < bands.len()` once before the pixel loop;
/// an out-of-range band defensively yields NaN (never panics).
pub fn eval_rpn(rpn: &[Token], bands: &[f64]) -> f64 {
    let mut stack = Vec::with_capacity(8);
    eval_rpn_with(rpn, bands, &mut stack)
}

#[derive(Debug, Clone, PartialEq)]
enum Lexeme {
    Num(f64),
    Band(usize),
    Plus,
    Minus,
    Star,
    Slash,
    LParen,
    RParen,
}

fn lex(s: &str) -> AgResult<Vec<Lexeme>> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i] as char;
        match c {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '+' => {
                out.push(Lexeme::Plus);
                i += 1;
            }
            '-' => {
                out.push(Lexeme::Minus);
                i += 1;
            }
            '*' => {
                out.push(Lexeme::Star);
                i += 1;
            }
            '/' => {
                out.push(Lexeme::Slash);
                i += 1;
            }
            '(' => {
                out.push(Lexeme::LParen);
                i += 1;
            }
            ')' => {
                out.push(Lexeme::RParen);
                i += 1;
            }
            'b' | 'B' => {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                if j == start {
                    return Err(AgError::query("expression", "band ref 'b' needs a number"));
                }
                let n: usize = s[start..j]
                    .parse()
                    .map_err(|_| AgError::query("expression", "invalid band number"))?;
                if n == 0 {
                    return Err(AgError::query(
                        "expression",
                        "bands are 1-indexed (b1 is first)",
                    ));
                }
                out.push(Lexeme::Band(n - 1));
                i = j;
            }
            c if c.is_ascii_digit() || c == '.' => {
                let start = i;
                let mut j = i;
                while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'.') {
                    j += 1;
                }
                let n: f64 = s[start..j]
                    .parse()
                    .map_err(|_| AgError::query("expression", "invalid number"))?;
                if !n.is_finite() {
                    return Err(AgError::query("expression", "number must be finite"));
                }
                out.push(Lexeme::Num(n));
                i = j;
            }
            other => {
                return Err(AgError::query(
                    "expression",
                    format!("unexpected character {other:?}"),
                ))
            }
        }
    }
    Ok(out)
}

struct Parser<'a> {
    toks: &'a [Lexeme],
    pos: usize,
    out: Vec<Token>,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Lexeme> {
        self.toks.get(self.pos)
    }
    fn expr(&mut self) -> AgResult<()> {
        self.term()?;
        while let Some(op) = self.peek() {
            let t = match op {
                Lexeme::Plus => Token::Add,
                Lexeme::Minus => Token::Sub,
                _ => break,
            };
            self.pos += 1;
            self.term()?;
            self.out.push(t);
        }
        Ok(())
    }
    fn term(&mut self) -> AgResult<()> {
        self.factor()?;
        while let Some(op) = self.peek() {
            let t = match op {
                Lexeme::Star => Token::Mul,
                Lexeme::Slash => Token::Div,
                _ => break,
            };
            self.pos += 1;
            self.factor()?;
            self.out.push(t);
        }
        Ok(())
    }
    fn factor(&mut self) -> AgResult<()> {
        self.depth += 1;
        if self.depth > 128 {
            return Err(AgError::query("expression", "expression nesting too deep"));
        }
        let r = self.factor_inner();
        self.depth -= 1;
        r
    }
    fn factor_inner(&mut self) -> AgResult<()> {
        match self.peek() {
            Some(Lexeme::Num(n)) => {
                let n = *n;
                self.out.push(Token::Num(n));
                self.pos += 1;
                Ok(())
            }
            Some(Lexeme::Band(i)) => {
                let i = *i;
                self.out.push(Token::Band(i));
                self.pos += 1;
                Ok(())
            }
            Some(Lexeme::Minus) => {
                self.pos += 1;
                self.factor()?;
                self.out.push(Token::Neg);
                Ok(())
            }
            Some(Lexeme::LParen) => {
                self.pos += 1;
                self.expr()?;
                match self.peek() {
                    Some(Lexeme::RParen) => {
                        self.pos += 1;
                        Ok(())
                    }
                    _ => Err(AgError::query("expression", "unbalanced parentheses")),
                }
            }
            _ => Err(AgError::query(
                "expression",
                "expected a number, band, or '('",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpn(s: &str) -> Vec<Token> {
        parse_expression(s).unwrap()
    }

    #[test]
    fn ndvi_value() {
        let t = rpn("(b5-b4)/(b5+b4)");
        let mut bands = vec![0.0; 5];
        bands[4] = 0.5;
        bands[3] = 0.1;
        let v = eval_rpn(&t, &bands);
        assert!((v - 0.6666667).abs() < 1e-5, "ndvi={v}");
    }
    #[test]
    fn precedence_and_parens() {
        assert_eq!(eval_rpn(&rpn("1+2*3"), &[]), 7.0);
        assert_eq!(eval_rpn(&rpn("(1+2)*3"), &[]), 9.0);
    }
    #[test]
    fn unary_minus() {
        assert_eq!(eval_rpn(&rpn("-b1+3"), &[2.0]), 1.0);
    }
    #[test]
    fn div_by_zero_is_nonfinite() {
        let v = eval_rpn(&rpn("b1/b2"), &[1.0, 0.0]);
        assert!(!v.is_finite(), "expected inf/nan, got {v}");
    }
    #[test]
    fn max_band_ref_reports_highest() {
        assert_eq!(max_band_ref(&rpn("(b5-b4)/(b5+b4)")), Some(4));
        assert_eq!(max_band_ref(&rpn("1+2")), None);
    }
    #[test]
    fn rejects_empty() {
        assert!(matches!(
            parse_expression("   "),
            Err(AgError::Query { .. })
        ));
    }

    #[test]
    fn rejects_overflowing_numeric_literal() {
        let literal = "9".repeat(400);
        assert!(matches!(
            parse_expression(&literal),
            Err(AgError::Query { name, .. }) if name == "expression"
        ));
    }
    #[test]
    fn rejects_band_zero() {
        assert!(matches!(
            parse_expression("b0+1"),
            Err(AgError::Query { .. })
        ));
    }
    #[test]
    fn rejects_bare_b() {
        assert!(matches!(
            parse_expression("b+1"),
            Err(AgError::Query { .. })
        ));
    }
    #[test]
    fn rejects_trailing_garbage() {
        assert!(matches!(
            parse_expression("b1+1 xyz"),
            Err(AgError::Query { .. })
        ));
        assert!(matches!(
            parse_expression("b1)"),
            Err(AgError::Query { .. })
        ));
    }
    #[test]
    fn rejects_unbalanced_parens() {
        assert!(matches!(
            parse_expression("(b1+1"),
            Err(AgError::Query { .. })
        ));
    }

    #[test]
    fn left_associative() {
        assert_eq!(eval_rpn(&rpn("1-2-3"), &[]), -4.0);
        assert_eq!(eval_rpn(&rpn("8/2/2"), &[]), 2.0);
    }

    #[test]
    fn unary_after_binary() {
        assert_eq!(eval_rpn(&rpn("2*-3"), &[]), -6.0);
        assert_eq!(eval_rpn(&rpn("3 - -2"), &[]), 5.0);
    }

    #[test]
    fn rejects_empty_parens_and_leading_op() {
        assert!(parse_expression("()").is_err());
        assert!(parse_expression("*b1").is_err());
        assert!(parse_expression("b1 b2").is_err());
    }

    #[test]
    fn generic_ndvi_expression_evaluates() {
        let tokens = rpn("(b5-b4)/(b5+b4)");
        let mut bands = vec![0.0; 5];
        bands[4] = 0.5;
        bands[3] = 0.1;

        assert!((eval_rpn(&tokens, &bands) - 0.6666667).abs() < 1e-5);
    }

    #[test]
    fn rejects_deeply_nested() {
        let deep = format!("{}b1{}", "(".repeat(500), ")".repeat(500));
        assert!(matches!(
            parse_expression(&deep),
            Err(AgError::Query { .. })
        ));
    }

    #[test]
    fn rejects_excessively_long_expression() {
        let long = ["b1"; 300].join("+");
        assert!(matches!(
            parse_expression(&long),
            Err(AgError::Query { .. })
        ));
    }
}
