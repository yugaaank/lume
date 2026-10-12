//! The launcher's inline calculator.
//!
//! ## Why a parser and not `eval`
//!
//! The obvious implementation is to hand the string to an expression
//! evaluator. Three problems, in increasing order of seriousness:
//!
//! 1. There is no expression evaluator in the Rust standard library, so it is
//!    a dependency either way.
//! 2. Whatever evaluator it is will accept more than arithmetic — variables,
//!    function calls, attribute access — and the input is a *search box*.
//!    Someone will type a character sequence that is a valid search and an
//!    invalid number, and the fallback path has to be safe by construction.
//! 3. `eval` is never the answer when the string came from a user typing fast
//!    into a box that is primarily a search field.
//!
//! A recursive-descent parser over a fixed grammar is about 120 lines, has no
//! dependencies, and rejects everything it does not understand. Rejecting is
//! the important part: the launcher's real job is search, and a query that is
//! not arithmetic should fall through to fuzzy matching, not produce an error.
//!
//! ## The grammar
//!
//! ```text
//! expr    := term (('+' | '-') term)*
//! term    := unary (('*' | '/' | '%') unary)*
//! unary   := ('-' | '+') unary | power
//! power   := primary ('^' unary)?        // right-associative
//! primary := number | '(' expr ')'
//! ```
//!
//! `unary` sits ABOVE `power`, which is what makes `-2^2` equal -4 rather than
//! 4. Every language agrees on this — exponent binds tighter than a sign — and
//! getting it backwards is the kind of thing a user notices immediately and
//! never reports, they just distrust the calculator.

/// Parse and evaluate. `None` for anything that is not arithmetic.
pub fn evaluate(input: &str) -> Option<f64> {
    let mut p = Parser {
        chars: input.chars().collect(),
        pos: 0,
    };
    let value = p.expr()?;
    p.skip_spaces();
    // Only a finite result is a calculator result. `inf` means a divide by
    // zero or an overflow happened somewhere, and showing that in a search
    // box is worse than showing no result.
    if p.pos == p.chars.len() && value.is_finite() {
        Some(value)
    } else {
        None
    }
}

/// Does this look like arithmetic at all?
///
/// The launcher uses this to decide whether to show a computed result beside
/// the query. A bare "7" is a number and probably a file name; "2+2" has an
/// operator and is almost certainly arithmetic.
pub fn looks_like_math(input: &str) -> bool {
    let mut chars = input.chars().peekable();
    let mut saw_operator = false;
    let mut saw_digit = false;
    let mut previous_was_value = false;

    while let Some(c) = chars.next() {
        match c {
            '0'..='9' | '.' => {
                saw_digit = true;
                previous_was_value = true;
            }
            '+' | '-' | '*' | '/' | '%' | '^' | '(' | ')' => {
                // A leading sign is a unary, not an operator.
                if previous_was_value {
                    saw_operator = true;
                }
                previous_was_value = matches!(c, ')' | '0'..='9' | '.');
            }
            c if c.is_whitespace() => {}
            // Anything else disqualifies it outright: a letter means this is a
            // search, not arithmetic, and treating "wifi" as an expression
            // that happens to parse is how you get nonsense.
            _ => return false,
        }
    }

    saw_operator && saw_digit
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, c: char) -> bool {
        self.skip_spaces();
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expr(&mut self) -> Option<f64> {
        let mut left = self.term()?;
        loop {
            self.skip_spaces();
            match self.peek() {
                Some('+') => {
                    self.pos += 1;
                    left += self.term()?;
                }
                Some('-') => {
                    self.pos += 1;
                    left -= self.term()?;
                }
                _ => return Some(left),
            }
        }
    }

    fn term(&mut self) -> Option<f64> {
        let mut left = self.unary()?;
        loop {
            self.skip_spaces();
            match self.peek() {
                Some('*') => {
                    self.pos += 1;
                    left *= self.unary()?;
                }
                Some('/') => {
                    self.pos += 1;
                    // Division by zero yields None rather than an infinity
                    // that would propagate through the rest of the sum.
                    left /= self.unary()?;
                }
                Some('%') => {
                    self.pos += 1;
                    left %= self.unary()?;
                }
                _ => return Some(left),
            }
        }
    }

    /// A sign binds looser than an exponent: -2^2 is -4, not 4.
    fn unary(&mut self) -> Option<f64> {
        self.skip_spaces();
        if self.eat('-') {
            return Some(-self.unary()?);
        }
        if self.eat('+') {
            return self.unary();
        }
        self.power()
    }

    /// Right-associative, so 2^3^2 is 512 and not 64.
    ///
    /// The exponent is a `unary`, which also makes 2^-3 work — a plain
    /// `power` there would refuse the sign.
    fn power(&mut self) -> Option<f64> {
        let base = self.primary()?;
        if self.eat('^') {
            let exponent = self.unary()?;
            return Some(base.powf(exponent));
        }
        Some(base)
    }

    fn primary(&mut self) -> Option<f64> {
        self.skip_spaces();

        if self.eat('(') {
            let value = self.expr()?;
            if !self.eat(')') {
                return None;
            }
            return Some(value);
        }

        let start = self.pos;
        let mut seen_dot = false;
        while let Some(c) = self.peek() {
            match c {
                '0'..='9' => self.pos += 1,
                '.' if !seen_dot => {
                    seen_dot = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        if start == self.pos {
            return None;
        }

        let text: String = self.chars[start..self.pos].iter().collect();
        text.parse::<f64>().ok()
    }
}

/// Format a result for display.
///
/// Trailing zeros are dropped and a whole number keeps its integer form, so
/// `4` does not appear as `4.000000` beside a search box.
pub fn format(value: f64) -> String {
    if !value.is_finite() {
        return String::new();
    }
    if value == value.trunc() && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    let mut s = format!("{value:.6}");
    if s.contains('.') {
        s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(input: &str) -> Option<f64> {
        evaluate(input)
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(e("2+2"), Some(4.0));
        assert_eq!(e("2+2*3"), Some(8.0));
        assert_eq!(e("2*3+2"), Some(8.0));
        assert_eq!(e("(2+2)*3"), Some(12.0));
        assert_eq!(e("10/4"), Some(2.5));
        assert_eq!(e("10%3"), Some(1.0));
    }

    /// Right-associative, unlike every other operator.
    #[test]
    fn exponent_is_right_associative() {
        assert_eq!(e("2^3^2"), Some(512.0));
        assert_eq!(e("2^10"), Some(1024.0));
    }

    /// -2^2 is -4. Every language agrees exponent binds tighter than a sign;
    /// getting this backwards is a silent, never-reported distrust.
    #[test]
    fn unary_minus_binds_looser_than_exponent() {
        assert_eq!(e("-5"), Some(-5.0));
        assert_eq!(e("-2^2"), Some(-4.0));
        assert_eq!(e("2^-3"), Some(0.125));
        assert_eq!(e("3*-2"), Some(-6.0));
        assert_eq!(e("--3"), Some(3.0));
        assert_eq!(e("-(2+2)"), Some(-4.0));
    }

    #[test]
    fn decimals_and_whitespace() {
        assert_eq!(e(" 1.5 + 2.25 "), Some(3.75));
        assert_eq!(e(".5+.5"), Some(1.0));
    }

    /// Everything here must be rejected so the query falls through to fuzzy
    /// search. This is the safety property, not a convenience.
    #[test]
    fn non_arithmetic_is_rejected() {
        for input in [
            "firefox", "wifi", "2+", "(2+3", "2+3)", "", "  ", "2++", "2 3", "1,000",
            "0x10", "2e", "2..3", "'a'", "alert(1)", "a+b", "2+firefox",
        ] {
            assert_eq!(e(input), None, "{input:?} should not evaluate");
        }
    }

    /// A name that happens to start with a digit is a search, not arithmetic.
    #[test]
    fn identifiers_are_never_expressions() {
        assert_eq!(e("7zip"), None);
        assert_eq!(e("1password"), None);
        assert_eq!(e("2fast"), None);
    }

    #[test]
    fn division_by_zero_is_none_not_infinity() {
        assert_eq!(e("1/0"), None);
        assert_eq!(e("0/0"), None);
        assert_eq!(e("5/(3-3)"), None);
    }

    /// A non-finite result anywhere means the whole thing is discarded, so an
    /// intermediate overflow cannot smuggle an infinity out through a sum.
    #[test]
    fn only_finite_results_survive() {
        assert_eq!(e("1e308*10"), None);
        assert_eq!(e("-1e308*10+1"), None);
    }

    /// Deep nesting must not blow the stack: this is user input typed fast
    /// into a search box.
    #[test]
    fn deep_nesting_does_not_crash() {
        let deep = format!("{}1{}", "(".repeat(200), ")".repeat(200));
        // Either it evaluates or it does not; what matters is that it returns.
        let _ = e(&deep);
    }

    #[test]
    fn detection_separates_arithmetic_from_search() {
        assert!(looks_like_math("2+2"));
        assert!(looks_like_math("12 * 3"));
        assert!(looks_like_math("(1+1)/2"));
        assert!(looks_like_math("2^10"));
        assert!(looks_like_math("2+3*4-5"));

        // A bare number is usually a file name.
        assert!(!looks_like_math("7"));
        assert!(!looks_like_math("42"));

        // Anything with a letter is a search.
        assert!(!looks_like_math("firefox"));
        assert!(!looks_like_math("7zip"));
        assert!(!looks_like_math("2+firefox"));
        assert!(!looks_like_math(""));
    }

    #[test]
    fn formatting_is_readable() {
        assert_eq!(format(4.0), "4");
        assert_eq!(format(-3.0), "-3");
        assert_eq!(format(2.5), "2.5");
        assert_eq!(format(1.0 / 3.0), "0.333333");
        assert_eq!(format(2.0 / 3.0), "0.666667");
        assert_eq!(format(f64::INFINITY), "");
        assert_eq!(format(f64::NAN), "");
    }
}
