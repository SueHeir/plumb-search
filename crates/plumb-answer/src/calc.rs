//! Sums: `12 * (3 + 4)`, `2^10`, `sqrt(2)`, `15% of 80`, `5!`.
//!
//! A query is a sum only when all of it reads as one, with at least one
//! operation in it, so "2024" and "f1" stay searches. Two numbers joined by
//! `/` or `-` with no spaces ("9/11", "24/7", "1-800") are left alone too:
//! they are more often names and dates than sums.

use crate::format::format_number;
use crate::{Answer, Kind};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Token {
    Num(f64),
    Plus,
    Minus,
    Times,
    Divide,
    Power,
    Percent,
    Factorial,
    Mod,
    Open,
    Close,
    Func(Func),
    Const(f64, &'static str),
    Degrees,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Func {
    Sqrt,
    Cbrt,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Ln,
    Log,
    Log2,
    Exp,
    Abs,
    Round,
    Floor,
    Ceil,
}

impl Func {
    fn named(name: &str) -> Option<Func> {
        Some(match name {
            "sqrt" | "√" => Func::Sqrt,
            "cbrt" => Func::Cbrt,
            "sin" => Func::Sin,
            "cos" => Func::Cos,
            "tan" => Func::Tan,
            "asin" | "arcsin" => Func::Asin,
            "acos" | "arccos" => Func::Acos,
            "atan" | "arctan" => Func::Atan,
            "ln" => Func::Ln,
            "log" | "log10" => Func::Log,
            "log2" => Func::Log2,
            "exp" => Func::Exp,
            "abs" => Func::Abs,
            "round" => Func::Round,
            "floor" => Func::Floor,
            "ceil" => Func::Ceil,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Func::Sqrt => "√",
            Func::Cbrt => "cbrt",
            Func::Sin => "sin",
            Func::Cos => "cos",
            Func::Tan => "tan",
            Func::Asin => "asin",
            Func::Acos => "acos",
            Func::Atan => "atan",
            Func::Ln => "ln",
            Func::Log => "log",
            Func::Log2 => "log2",
            Func::Exp => "exp",
            Func::Abs => "abs",
            Func::Round => "round",
            Func::Floor => "floor",
            Func::Ceil => "ceil",
        }
    }

    fn apply(self, x: f64) -> f64 {
        match self {
            Func::Sqrt => x.sqrt(),
            Func::Cbrt => x.cbrt(),
            Func::Sin => snap(x.sin(), x),
            Func::Cos => snap(x.cos(), x),
            // Undefined at odd multiples of π/2, not 1.6 × 10¹⁶.
            Func::Tan if snap(x.cos(), x) == 0.0 => f64::NAN,
            Func::Tan => snap(x.tan(), x),
            Func::Asin => x.asin(),
            Func::Acos => x.acos(),
            Func::Atan => x.atan(),
            Func::Ln => x.ln(),
            Func::Log => x.log10(),
            Func::Log2 => x.log2(),
            Func::Exp => x.exp(),
            Func::Abs => x.abs(),
            Func::Round => x.round(),
            Func::Floor => x.floor(),
            Func::Ceil => x.ceil(),
        }
    }
}

/// `result`, a sine, cosine or tangent of `x`, as 0 when it is only float
/// noise: sin(π) is 0, not 1.2 × 10⁻¹⁶. The noise grows with `x`, as π
/// itself is off by about `x` × 10⁻¹⁶.
fn snap(result: f64, x: f64) -> f64 {
    if result.abs() < 1e-12 * x.abs() {
        0.0
    } else {
        result
    }
}

/// `text` as tokens, `None` when any of it is not part of a sum.
fn tokens(text: &str) -> Option<Vec<Token>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_digit()
                    || chars[i] == '.'
                    // 1,000,000: a comma followed by three digits.
                    || (chars[i] == ','
                        && chars.get(i + 1..i + 4).is_some_and(|d| d.iter().all(char::is_ascii_digit))
                        && !chars.get(i + 4).is_some_and(char::is_ascii_digit)))
            {
                i += 1;
            }
            // 1e6, 2.5e-3
            if i < chars.len()
                && (chars[i] == 'e' || chars[i] == 'E')
                && (chars.get(i + 1).is_some_and(char::is_ascii_digit)
                    || (matches!(chars.get(i + 1), Some('-' | '+'))
                        && chars.get(i + 2).is_some_and(char::is_ascii_digit)))
            {
                i += 2;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let number: String = chars[start..i].iter().filter(|&&c| c != ',').collect();
            out.push(Token::Num(number.parse().ok()?));
            continue;
        }
        if c.is_alphabetic() || c == 'π' || c == 'τ' || c == '√' {
            let start = i;
            if c == 'π' || c == 'τ' || c == '√' {
                i += 1;
            } else {
                while i < chars.len() && (chars[i].is_alphanumeric()) {
                    i += 1;
                }
            }
            let word: String = chars[start..i].iter().collect::<String>().to_lowercase();
            out.push(match word.as_str() {
                "pi" | "π" => Token::Const(std::f64::consts::PI, "π"),
                "tau" | "τ" => Token::Const(std::f64::consts::TAU, "τ"),
                "e" => Token::Const(std::f64::consts::E, "e"),
                "x" => Token::Times,
                "of" => Token::Times,
                "mod" => Token::Mod,
                "deg" | "degrees" => Token::Degrees,
                _ => Token::Func(Func::named(&word)?),
            });
            continue;
        }
        let token = match c {
            '+' => Token::Plus,
            '-' | '−' => Token::Minus,
            '*' if chars.get(i + 1) == Some(&'*') => {
                i += 1;
                Token::Power
            }
            '*' | '×' | '·' => Token::Times,
            '/' | '÷' => Token::Divide,
            '^' => Token::Power,
            '%' => Token::Percent,
            '!' => Token::Factorial,
            '(' | '[' => Token::Open,
            ')' | ']' => Token::Close,
            '°' => Token::Degrees,
            '²' => {
                out.push(Token::Power);
                Token::Num(2.0)
            }
            '³' => {
                out.push(Token::Power);
                Token::Num(3.0)
            }
            _ => return None,
        };
        out.push(token);
        i += 1;
    }
    Some(out)
}

/// A recursive-descent reader of a token list, working the sum out and
/// writing it back neatly as it goes.
struct Parser {
    tokens: Vec<Token>,
    at: usize,
    /// Whether an operation was seen: a lone number is no sum.
    operations: usize,
}

type Value = (f64, String);

impl Parser {
    fn peek(&self) -> Option<Token> {
        self.tokens.get(self.at).copied()
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.peek();
        self.at += 1;
        token
    }

    /// expr := term (('+' | '-') term)*
    fn expr(&mut self) -> Option<Value> {
        let (mut value, mut text) = self.term()?;
        while let Some(op @ (Token::Plus | Token::Minus)) = self.peek() {
            self.at += 1;
            self.operations += 1;
            let (right, right_text) = self.term()?;
            if op == Token::Plus {
                value += right;
                text = format!("{text} + {right_text}");
            } else {
                value -= right;
                text = format!("{text} − {right_text}");
            }
        }
        Some((value, text))
    }

    /// term := unary (('*' | '/' | 'mod' | implicit) unary)*
    fn term(&mut self) -> Option<Value> {
        let (mut value, mut text) = self.unary()?;
        loop {
            match self.peek() {
                Some(op @ (Token::Times | Token::Divide | Token::Mod)) => {
                    self.at += 1;
                    self.operations += 1;
                    let (right, right_text) = self.unary()?;
                    match op {
                        Token::Times => {
                            value *= right;
                            text = format!("{text} × {right_text}");
                        }
                        Token::Divide => {
                            if right == 0.0 {
                                return None;
                            }
                            value /= right;
                            text = format!("{text} ÷ {right_text}");
                        }
                        _ => {
                            if right == 0.0 {
                                return None;
                            }
                            value = value.rem_euclid(right);
                            text = format!("{text} mod {right_text}");
                        }
                    }
                }
                // 2π, 3(4 + 5), 2 sqrt(2)
                Some(Token::Open | Token::Const(..) | Token::Func(_)) => {
                    self.operations += 1;
                    let (right, right_text) = self.unary()?;
                    value *= right;
                    text = format!("{text} × {right_text}");
                }
                _ => return Some((value, text)),
            }
        }
    }

    /// unary := ('-' | '+') unary | power
    fn unary(&mut self) -> Option<Value> {
        match self.peek() {
            Some(Token::Minus) => {
                self.at += 1;
                let (value, text) = self.unary()?;
                Some((-value, format!("−{text}")))
            }
            Some(Token::Plus) => {
                self.at += 1;
                self.unary()
            }
            _ => self.power(),
        }
    }

    /// power := postfix ('^' unary)?
    fn power(&mut self) -> Option<Value> {
        let (base, text) = self.postfix()?;
        if self.peek() == Some(Token::Power) {
            self.at += 1;
            self.operations += 1;
            let (exponent, exponent_text) = self.unary()?;
            return Some((base.powf(exponent), format!("{text}^{exponent_text}")));
        }
        Some((base, text))
    }

    /// postfix := primary ('!' | '%' | '°')*
    fn postfix(&mut self) -> Option<Value> {
        let (mut value, mut text) = self.primary()?;
        loop {
            match self.peek() {
                Some(Token::Factorial) => {
                    self.at += 1;
                    self.operations += 1;
                    if value < 0.0 || value.fract() != 0.0 || value > 170.0 {
                        return None;
                    }
                    value = (1..=value as u64).map(|n| n as f64).product();
                    text.push('!');
                }
                Some(Token::Percent) => {
                    self.at += 1;
                    self.operations += 1;
                    value /= 100.0;
                    text.push('%');
                }
                Some(Token::Degrees) => {
                    self.at += 1;
                    value = value.to_radians();
                    text.push('°');
                }
                _ => return Some((value, text)),
            }
        }
    }

    /// primary := number | constant | '(' expr ')' | function primary
    fn primary(&mut self) -> Option<Value> {
        match self.next()? {
            Token::Num(n) => Some((n, format_number(n, 15)?)),
            Token::Const(value, name) => {
                self.operations += 1;
                Some((value, name.to_string()))
            }
            Token::Open => {
                let (value, text) = self.expr()?;
                // A missing closing bracket at the end is forgiven.
                if self.peek() == Some(Token::Close) {
                    self.at += 1;
                } else if self.peek().is_some() {
                    return None;
                }
                Some((value, format!("({text})")))
            }
            Token::Func(func) => {
                self.operations += 1;
                let (value, text) = self.power()?;
                let text = if text.starts_with('(') {
                    format!("{}{text}", func.name())
                } else {
                    format!("{}({text})", func.name())
                };
                Some((func.apply(value), text))
            }
            _ => None,
        }
    }
}

/// Whether `text` is two whole numbers joined by `/` or `-` with nothing
/// around them: "9/11", "24/7", "1-800".
fn is_name_like(text: &str) -> bool {
    let Some((left, right)) = text.split_once(['/', '-']) else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    digits(left) && digits(right)
}

pub(crate) fn answer(query: &str) -> Option<Answer> {
    let mut text = query.trim();
    for prefix in ["calculate ", "calc ", "what is ", "what's ", "whats ", "="] {
        // Matched on the text itself: lowercasing can change byte lengths.
        if text
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        {
            text = text[prefix.len()..].trim();
        }
    }
    let text = text.trim_end_matches(['=', '?']).trim();
    if is_name_like(text) {
        return None;
    }
    let mut parser = Parser {
        tokens: tokens(text)?,
        at: 0,
        operations: 0,
    };
    if parser.tokens.is_empty() {
        return None;
    }
    let (value, shown) = parser.expr()?;
    if parser.at != parser.tokens.len() || parser.operations == 0 {
        return None;
    }
    // "pi" and "e" alone are searches.
    if parser.tokens.len() == 1 {
        return None;
    }
    // Round away float noise: 0.1 + 0.2 is 0.3.
    let answer = format_number(value, 12)?;
    Some(Answer {
        kind: Kind::Calculation,
        question: format!("{shown} ="),
        answer,
        note: None,
    })
}

#[cfg(test)]
mod tests {
    use super::answer;

    fn calc(query: &str) -> Option<String> {
        answer(query).map(|a| a.answer)
    }

    #[test]
    fn works_sums_out() {
        assert_eq!(calc("2+2").unwrap(), "4");
        assert_eq!(calc("12 * (3 + 4)").unwrap(), "84");
        assert_eq!(calc("2^10").unwrap(), "1,024");
        assert_eq!(calc("2**3**2").unwrap(), "512");
        assert_eq!(calc("-2^2").unwrap(), "-4");
        assert_eq!(calc("sqrt(2)").unwrap(), "1.41421356237");
        assert_eq!(calc("sqrt 16").unwrap(), "4");
        assert_eq!(calc("15% of 80").unwrap(), "12");
        assert_eq!(calc("5!").unwrap(), "120");
        assert_eq!(calc("2pi").unwrap(), "6.28318530718");
        assert_eq!(calc("sin(90°)").unwrap(), "1");
        assert_eq!(calc("3 x 4").unwrap(), "12");
        assert_eq!(calc("1,000,000 / 3").unwrap(), "333,333.333333");
        assert_eq!(calc("10 mod 3").unwrap(), "1");
        assert_eq!(calc("0.1+0.2").unwrap(), "0.3");
        assert_eq!(calc("what is 6*7?").unwrap(), "42");
        assert_eq!(calc("9 / 11").unwrap(), "0.818181818182");
        assert_eq!(calc("1e3 + 1").unwrap(), "1,001");
        assert_eq!(calc("(1+2").unwrap(), "3");
        assert_eq!(calc("log(1000)").unwrap(), "3");
        assert_eq!(calc("5²").unwrap(), "25");
    }

    #[test]
    fn trig_drops_float_noise() {
        assert_eq!(calc("cos(90°)").unwrap(), "0");
        assert_eq!(calc("sin(pi)").unwrap(), "0");
        assert_eq!(calc("sin(180°)").unwrap(), "0");
        assert_eq!(calc("tan(180°)").unwrap(), "0");
        assert_eq!(calc("tan(45°)").unwrap(), "1");
        assert_eq!(calc("sin(1e-13) * 1").unwrap(), "1 × 10⁻¹³");
        assert_eq!(answer("tan(90°)"), None);
        assert_eq!(answer("tan(pi/2)"), None);
        assert_eq!(answer("tan(270°)"), None);
    }

    #[test]
    fn subnormals_are_written_out() {
        assert_eq!(calc("1e-320 * 1").unwrap(), "9.99988867 × 10⁻³²¹");
    }

    #[test]
    fn writes_the_sum_back() {
        assert_eq!(answer("12*(3+4)").unwrap().question, "12 × (3 + 4) =");
        assert_eq!(answer("sqrt 2").unwrap().question, "√(2) =");
    }

    #[test]
    fn prefixes_whose_lowercase_changes_length_do_not_panic() {
        assert_eq!(answer("what is \u{1E9E}"), None);
        assert_eq!(
            answer("calc \u{130}\u{130}\u{130}\u{130}\u{130}\u{130}\u{130}\u{130}"),
            None
        );
        assert_eq!(answer("\u{130}\u{130}\u{130}\u{130}\u{130}"), None);
        assert_eq!(calc("WHAT IS 6*7").unwrap(), "42");
    }

    #[test]
    fn leaves_searches_alone() {
        for query in [
            "2024", "9/11", "24/7", "1-800", "python 3", "f1", "e", "pi", "1/0", "3 4", "covid-19",
            "e coli", "x", "of", "log", "100 km", "(", "1 +",
        ] {
            assert_eq!(answer(query), None, "{query:?}");
        }
    }
}
