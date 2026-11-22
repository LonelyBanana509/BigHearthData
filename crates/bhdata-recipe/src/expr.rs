//! The expression language of steps (spec `recipe-format.md`, section 2): arguments, values read
//! earlier, literals, `now`, `+ - * /`, comparisons, `and`/`or`/`not`, and `round`, `min`, `max`,
//! `abs`. Nothing else exists, so nothing else can be asked of the database.
//!
//! [`parse_expr`] only reads the text. Types and rounding are checked by the compiler.
//! [`Expr`]'s `Display` is the canonical text: fully bracketed, one spelling per meaning, and
//! `parse_expr(e.to_string()) == e`.

use std::fmt;

use bhdata_schema::decimal::format_scaled;

/// The longest expression text.
pub const MAX_EXPR_LEN: usize = 200;
/// The deepest nesting of brackets, `not` and unary minus.
pub const MAX_DEPTH: usize = 16;

/// A parsed expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// A number, scaled by `10^scale` (a whole number has scale 0).
    Num {
        /// The value times `10^scale`.
        value: i128,
        /// Digits after the point.
        scale: u8,
    },
    /// `true` or `false`.
    Bool(bool),
    /// The time the command runs at.
    Now,
    /// An argument, or a value read earlier (`balance`, `to.balance`).
    Var(String),
    /// `-x` or `not x`.
    Unary(UnOp, Box<Expr>),
    /// `a + b`, `a == b`, `a and b`, and so on.
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `round(x, scale, mode)`.
    Round {
        /// The value.
        value: Box<Expr>,
        /// Digits to keep after the point, 0 to 38.
        scale: u8,
        /// How ties and the rest are decided.
        mode: RoundMode,
    },
    /// `min(a, b)` or `max(a, b)`.
    MinMax {
        /// `true` for `max`.
        max: bool,
        /// The two values.
        args: Box<(Expr, Expr)>,
    },
    /// `abs(x)`.
    Abs(Box<Expr>),
}

/// A one-operand operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    /// `-`.
    Neg,
    /// `not`.
    Not,
}

/// A two-operand operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/` (only inside `round`; the compiler checks).
    Div,
    /// `==`.
    Eq,
    /// `!=`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
    /// `and`.
    And,
    /// `or`.
    Or,
}

impl BinOp {
    fn text(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "and",
            BinOp::Or => "or",
        }
    }
}

/// How `round` settles a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundMode {
    /// Ties go to the even digit (banker's rounding).
    HalfEven,
    /// Ties go away from zero.
    HalfUp,
    /// Cut off, toward zero.
    Down,
}

impl RoundMode {
    fn text(self) -> &'static str {
        match self {
            RoundMode::HalfEven => "half_even",
            RoundMode::HalfUp => "half_up",
            RoundMode::Down => "down",
        }
    }
}

/// Words an argument or read name may not take, because the expression language uses them.
pub const KEYWORDS: [&str; 10] = [
    "true", "false", "now", "and", "or", "not", "round", "min", "max", "abs",
];

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Num { value, scale } => f.write_str(&format_scaled(*value, *scale)),
            Expr::Bool(b) => write!(f, "{b}"),
            Expr::Now => f.write_str("now"),
            Expr::Var(name) => f.write_str(name),
            Expr::Unary(UnOp::Neg, x) => write!(f, "(-{x})"),
            Expr::Unary(UnOp::Not, x) => write!(f, "(not {x})"),
            Expr::Binary(op, a, b) => write!(f, "({a} {} {b})", op.text()),
            Expr::Round { value, scale, mode } => {
                write!(f, "round({value}, {scale}, {})", mode.text())
            }
            Expr::MinMax { max, args } => {
                write!(
                    f,
                    "{}({}, {})",
                    if *max { "max" } else { "min" },
                    args.0,
                    args.1
                )
            }
            Expr::Abs(x) => write!(f, "abs({x})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Num(String),
    Word(String),
    Sym(&'static str),
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'
}

fn tokenize(text: &str) -> Result<Vec<(usize, Token)>, String> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let start = i;
        if b.is_ascii_whitespace() {
            i += 1;
        } else if b.is_ascii_digit() {
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'.' {
                i += 1;
                if i >= bytes.len() || !bytes[i].is_ascii_digit() {
                    return Err(format!("at {}: a number cannot end with `.`", i));
                }
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
                return Err(format!("at {}: unexpected letter after a number", i + 1));
            }
            tokens.push((start + 1, Token::Num(text[start..i].to_string())));
        } else if b.is_ascii_lowercase() {
            while i < bytes.len() && is_word_byte(bytes[i]) {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'.' {
                i += 1;
                if i >= bytes.len() || !bytes[i].is_ascii_lowercase() {
                    return Err(format!("at {}: a name after `.` is expected", i + 1));
                }
                while i < bytes.len() && is_word_byte(bytes[i]) {
                    i += 1;
                }
            }
            tokens.push((start + 1, Token::Word(text[start..i].to_string())));
        } else {
            let sym = match text.get(i..i + 2).unwrap_or("") {
                "==" => Some("=="),
                "!=" => Some("!="),
                "<=" => Some("<="),
                ">=" => Some(">="),
                _ => None,
            };
            if let Some(sym) = sym {
                tokens.push((i + 1, Token::Sym(sym)));
                i += 2;
                continue;
            }
            let sym = match b {
                b'+' => "+",
                b'-' => "-",
                b'*' => "*",
                b'/' => "/",
                b'(' => "(",
                b')' => ")",
                b',' => ",",
                b'<' => "<",
                b'>' => ">",
                _ => {
                    return Err(format!(
                        "at {}: unexpected character {:?}",
                        i + 1,
                        text[i..].chars().next().unwrap_or('?')
                    ))
                }
            };
            tokens.push((i + 1, Token::Sym(sym)));
            i += 1;
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<(usize, Token)>,
    at: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at).map(|(_, t)| t)
    }

    fn here(&self) -> usize {
        self.tokens
            .get(self.at)
            .or(self.tokens.last())
            .map_or(1, |(p, _)| *p)
    }

    fn err<T>(&self, what: &str) -> Result<T, String> {
        Err(format!("at {}: {what}", self.here()))
    }

    fn eat_sym(&mut self, sym: &str) -> bool {
        if matches!(self.peek(), Some(Token::Sym(s)) if *s == sym) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, word: &str) -> bool {
        if matches!(self.peek(), Some(Token::Word(w)) if w == word) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect_sym(&mut self, sym: &str) -> Result<(), String> {
        if self.eat_sym(sym) {
            Ok(())
        } else {
            self.err(&format!("expected `{sym}`"))
        }
    }

    fn nested<T>(&mut self, f: impl FnOnce(&mut Parser) -> Result<T, String>) -> Result<T, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.err(&format!("nested more than {MAX_DEPTH} deep"));
        }
        let out = f(self);
        self.depth -= 1;
        out
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut left = self.and()?;
        while self.eat_word("or") {
            let right = self.and()?;
            left = Expr::Binary(BinOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut left = self.not()?;
        while self.eat_word("and") {
            let right = self.not()?;
            left = Expr::Binary(BinOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr, String> {
        if self.eat_word("not") {
            let inner = self.nested(Parser::not)?;
            return Ok(Expr::Unary(UnOp::Not, Box::new(inner)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Expr, String> {
        let left = self.sum()?;
        let op = match self.peek() {
            Some(Token::Sym("==")) => BinOp::Eq,
            Some(Token::Sym("!=")) => BinOp::Ne,
            Some(Token::Sym("<=")) => BinOp::Le,
            Some(Token::Sym(">=")) => BinOp::Ge,
            Some(Token::Sym("<")) => BinOp::Lt,
            Some(Token::Sym(">")) => BinOp::Gt,
            _ => return Ok(left),
        };
        self.at += 1;
        let right = self.sum()?;
        if matches!(
            self.peek(),
            Some(Token::Sym("==" | "!=" | "<=" | ">=" | "<" | ">"))
        ) {
            return self.err("comparisons do not chain; use `and`");
        }
        Ok(Expr::Binary(op, Box::new(left), Box::new(right)))
    }

    fn sum(&mut self) -> Result<Expr, String> {
        let mut left = self.product()?;
        loop {
            let op = if self.eat_sym("+") {
                BinOp::Add
            } else if self.eat_sym("-") {
                BinOp::Sub
            } else {
                return Ok(left);
            };
            let right = self.product()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }

    fn product(&mut self) -> Result<Expr, String> {
        let mut left = self.unary()?;
        loop {
            let op = if self.eat_sym("*") {
                BinOp::Mul
            } else if self.eat_sym("/") {
                BinOp::Div
            } else {
                return Ok(left);
            };
            let right = self.unary()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if self.eat_sym("-") {
            let inner = self.nested(Parser::unary)?;
            return Ok(match inner {
                Expr::Num { value, scale } => Expr::Num {
                    value: -value,
                    scale,
                },
                other => Expr::Unary(UnOp::Neg, Box::new(other)),
            });
        }
        self.primary()
    }

    fn number(&self, text: &str) -> Result<Expr, String> {
        let (int, frac) = text.split_once('.').unwrap_or((text, ""));
        let digits = format!("{int}{frac}");
        if digits.trim_start_matches('0').len() > 38 {
            return self.err("a number has at most 38 digits");
        }
        let value: i128 = digits
            .parse()
            .map_err(|_| format!("at {}: number too large", self.here()))?;
        Ok(Expr::Num {
            value,
            scale: u8::try_from(frac.len()).unwrap_or(u8::MAX),
        })
    }

    fn primary(&mut self) -> Result<Expr, String> {
        let Some(token) = self.peek().cloned() else {
            return self.err("the expression ends too soon");
        };
        match token {
            Token::Num(text) => {
                let e = self.number(&text)?;
                self.at += 1;
                Ok(e)
            }
            Token::Sym("(") => {
                self.at += 1;
                let inner = self.nested(Parser::or)?;
                self.expect_sym(")")?;
                Ok(inner)
            }
            Token::Word(word) => {
                self.at += 1;
                let call = matches!(self.peek(), Some(Token::Sym("(")));
                match word.as_str() {
                    "true" => Ok(Expr::Bool(true)),
                    "false" => Ok(Expr::Bool(false)),
                    "now" => Ok(Expr::Now),
                    "and" | "or" | "not" => {
                        self.at -= 1;
                        self.err(&format!("`{word}` is not allowed here"))
                    }
                    "round" | "min" | "max" | "abs" if call => {
                        self.at += 1;
                        self.nested(|p| p.call(&word))
                    }
                    _ if call => {
                        self.at -= 1;
                        self.err(&format!(
                            "unknown function `{word}` (there are round, min, max and abs)"
                        ))
                    }
                    _ => Ok(Expr::Var(word)),
                }
            }
            Token::Sym(s) => self.err(&format!("unexpected `{s}`")),
        }
    }

    fn call(&mut self, name: &str) -> Result<Expr, String> {
        let first = self.or()?;
        let out = match name {
            "abs" => Expr::Abs(Box::new(first)),
            "round" => {
                self.expect_sym(",")?;
                let scale = match self.peek().cloned() {
                    Some(Token::Num(t)) if !t.contains('.') => {
                        self.at += 1;
                        t.parse::<u8>().ok().filter(|s| *s <= 38)
                    }
                    _ => None,
                };
                let Some(scale) = scale else {
                    return self.err("`round` needs a whole number of digits, 0 to 38");
                };
                self.expect_sym(",")?;
                let mode = match self.peek() {
                    Some(Token::Word(w)) if w == "half_even" => RoundMode::HalfEven,
                    Some(Token::Word(w)) if w == "half_up" => RoundMode::HalfUp,
                    Some(Token::Word(w)) if w == "down" => RoundMode::Down,
                    _ => return self.err("`round` needs a mode: half_even, half_up or down"),
                };
                self.at += 1;
                Expr::Round {
                    value: Box::new(first),
                    scale,
                    mode,
                }
            }
            _ => {
                self.expect_sym(",")?;
                let second = self.or()?;
                Expr::MinMax {
                    max: name == "max",
                    args: Box::new((first, second)),
                }
            }
        };
        self.expect_sym(")")?;
        Ok(out)
    }
}

/// Reads an expression.
///
/// # Errors
/// A plain sentence with the position (counting from 1).
pub fn parse_expr(text: &str) -> Result<Expr, String> {
    if text.len() > MAX_EXPR_LEN {
        return Err(format!(
            "an expression has at most {MAX_EXPR_LEN} characters"
        ));
    }
    let tokens = tokenize(text)?;
    if tokens.is_empty() {
        return Err("the expression is empty".to_string());
    }
    let mut parser = Parser {
        tokens,
        at: 0,
        depth: 0,
    };
    let expr = parser.or()?;
    if parser.at < parser.tokens.len() {
        return parser.err("unexpected input after the expression");
    }
    Ok(expr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(text: &str) -> String {
        parse_expr(text).unwrap().to_string()
    }

    #[test]
    fn precedence_and_canonical_text() {
        for (text, want) in [
            ("1 + 2 * 3", "(1 + (2 * 3))"),
            ("(1 + 2) * 3", "((1 + 2) * 3)"),
            ("a - b - c", "((a - b) - c)"),
            ("-amount", "(-amount)"),
            ("-5", "-5"),
            ("2 - -3", "(2 - -3)"),
            (
                "a >= 1 and b < 2 or not c",
                "(((a >= 1) and (b < 2)) or (not c))",
            ),
            (
                "round(balance * rate, 2, half_even)",
                "round((balance * rate), 2, half_even)",
            ),
            ("round(a / b, 0, down)", "round((a / b), 0, down)"),
            ("min(a, max(b, 3.50))", "min(a, max(b, 3.50))"),
            ("abs(to.balance - 1)", "abs((to.balance - 1))"),
            ("now", "now"),
            ("true and false", "(true and false)"),
            ("0.050", "0.050"),
        ] {
            assert_eq!(canon(text), want, "{text}");
        }
    }

    #[test]
    fn canonical_text_reads_back_to_the_same_expression() {
        for text in [
            "a + b * (c - 1)",
            "round(x * 0.01, 2, half_up)",
            "not (a == 1) and b != 2",
            "min(abs(a), 10) <= max(b, 0)",
            "-(a + 1)",
        ] {
            let e = parse_expr(text).unwrap();
            assert_eq!(parse_expr(&e.to_string()).unwrap(), e, "{text}");
        }
    }

    #[test]
    fn numbers_keep_their_scale() {
        assert_eq!(
            parse_expr("1.50").unwrap(),
            Expr::Num {
                value: 150,
                scale: 2
            }
        );
        assert_eq!(parse_expr("007").unwrap(), Expr::Num { value: 7, scale: 0 });
        assert_eq!(
            parse_expr("-0.5").unwrap(),
            Expr::Num {
                value: -5,
                scale: 1
            }
        );
    }

    #[test]
    fn every_bad_expression_says_where() {
        for (text, fragment) in [
            ("", "empty"),
            ("1 +", "ends too soon"),
            ("1 2", "after the expression"),
            ("(1", "expected `)`"),
            ("1 < 2 < 3", "do not chain"),
            ("lower(a)", "unknown function"),
            ("round(a)", "expected `,`"),
            ("round(a, 40, down)", "0 to 38"),
            ("round(a, 2.5, down)", "0 to 38"),
            ("round(a, 2, sideways)", "mode"),
            ("min(a)", "expected `,`"),
            ("a $ b", "unexpected character"),
            ("1.", "cannot end"),
            ("1a", "letter after a number"),
            ("a.", "name after"),
            ("and", "not allowed here"),
            ("\"text\"", "unexpected character"),
            ("a = 1", "unexpected character"),
            (&format!("1{}", "0".repeat(40)), "38 digits"),
        ] {
            let e = parse_expr(text).unwrap_err();
            assert!(e.contains(fragment), "{text:?}: {e}");
        }
    }

    #[test]
    fn size_and_depth_are_bounded() {
        assert!(parse_expr(&"1+".repeat(150))
            .unwrap_err()
            .contains("at most"));
        let deep = format!(
            "{}1{}",
            "(".repeat(MAX_DEPTH + 1),
            ")".repeat(MAX_DEPTH + 1)
        );
        assert!(parse_expr(&deep).unwrap_err().contains("deep"));
        let ok = format!("{}1{}", "(".repeat(MAX_DEPTH), ")".repeat(MAX_DEPTH));
        assert!(parse_expr(&ok).is_ok());
        let nots = format!("{}a", "not ".repeat(MAX_DEPTH + 1));
        assert!(parse_expr(&nots).unwrap_err().contains("deep"));
    }

    #[test]
    fn non_ascii_text_is_an_error_not_a_panic() {
        assert!(parse_expr("é")
            .unwrap_err()
            .contains("unexpected character"));
        assert!(parse_expr("a + ß").is_err());
    }
}
