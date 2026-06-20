//! Local rich-answer cards.
//!
//! These are deterministic, no-network answers that can be rendered above search
//! results. Calculator and unit conversion cards do not depend on the index.
//! Definition cards are detected here but use the query engine's extractive
//! answer as their source in `server.rs`.

#[derive(Clone, Debug, PartialEq)]
pub struct RichCard {
    pub label: &'static str,
    pub title: String,
    pub value: String,
    pub detail: Option<String>,
}

pub fn local_card(query: &str) -> Option<RichCard> {
    if is_weather_query(query) {
        return None;
    }
    conversion_card(query).or_else(|| calculator_card(query))
}

pub fn is_definition_query(query: &str) -> bool {
    let q = query.trim().to_lowercase();
    q.strip_prefix("define ")
        .or_else(|| q.strip_prefix("definition of "))
        .or_else(|| q.strip_prefix("what is "))
        .or_else(|| q.strip_prefix("what are "))
        .map(|rest| !rest.trim().is_empty())
        .unwrap_or(false)
}

pub fn is_weather_query(query: &str) -> bool {
    let q = query.to_lowercase();
    q.starts_with("weather ")
        || q.contains(" weather in ")
        || q.starts_with("temperature ")
        || q.contains(" forecast")
}

fn calculator_card(query: &str) -> Option<RichCard> {
    let expr = query.trim();
    if !looks_like_expression(expr) {
        return None;
    }
    let value = Parser::new(expr).parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    Some(RichCard {
        label: "Calculator",
        title: expr.to_string(),
        value: format_number(value),
        detail: None,
    })
}

fn looks_like_expression(expr: &str) -> bool {
    let e = expr.trim().to_lowercase();
    if e.starts_with("sqrt") {
        return true;
    }
    e.chars()
        .any(|c| matches!(c, '+' | '-' | '*' | '/' | '^' | '(' | ')'))
        && e.chars().all(|c| {
            c.is_ascii_digit()
                || c.is_ascii_whitespace()
                || matches!(c, '.' | '+' | '-' | '*' | '/' | '^' | '(' | ')')
        })
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Parser {
            bytes: s.as_bytes(),
            pos: 0,
        }
    }

    fn parse(mut self) -> Result<f64, ()> {
        let v = self.expr()?;
        self.skip_ws();
        (self.pos == self.bytes.len()).then_some(v).ok_or(())
    }

    fn expr(&mut self) -> Result<f64, ()> {
        let mut v = self.term()?;
        loop {
            self.skip_ws();
            if self.eat(b'+') {
                v += self.term()?;
            } else if self.eat(b'-') {
                v -= self.term()?;
            } else {
                return Ok(v);
            }
        }
    }

    fn term(&mut self) -> Result<f64, ()> {
        let mut v = self.power()?;
        loop {
            self.skip_ws();
            if self.eat(b'*') {
                v *= self.power()?;
            } else if self.eat(b'/') {
                let rhs = self.power()?;
                if rhs == 0.0 {
                    return Err(());
                }
                v /= rhs;
            } else {
                return Ok(v);
            }
        }
    }

    fn power(&mut self) -> Result<f64, ()> {
        let base = self.unary()?;
        self.skip_ws();
        if self.eat(b'^') {
            Ok(base.powf(self.power()?))
        } else {
            Ok(base)
        }
    }

    fn unary(&mut self) -> Result<f64, ()> {
        self.skip_ws();
        if self.eat(b'+') {
            self.unary()
        } else if self.eat(b'-') {
            Ok(-self.unary()?)
        } else {
            self.primary()
        }
    }

    fn primary(&mut self) -> Result<f64, ()> {
        self.skip_ws();
        if self.eat_word("sqrt") {
            self.skip_ws();
            let v = if self.eat(b'(') {
                let v = self.expr()?;
                self.skip_ws();
                if !self.eat(b')') {
                    return Err(());
                }
                v
            } else {
                self.primary()?
            };
            return (v >= 0.0).then_some(v.sqrt()).ok_or(());
        }
        if self.eat(b'(') {
            let v = self.expr()?;
            self.skip_ws();
            return self.eat(b')').then_some(v).ok_or(());
        }
        self.number()
    }

    fn number(&mut self) -> Result<f64, ()> {
        self.skip_ws();
        let start = self.pos;
        let mut dot = false;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b.is_ascii_digit() {
                self.pos += 1;
            } else if b == b'.' && !dot {
                dot = true;
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(());
        }
        std::str::from_utf8(&self.bytes[start..self.pos])
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .ok_or(())
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn eat(&mut self, b: u8) -> bool {
        self.skip_ws();
        if self.pos < self.bytes.len() && self.bytes[self.pos] == b {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, word: &str) -> bool {
        self.skip_ws();
        let rest = &self.bytes[self.pos..];
        if rest.len() >= word.len() && rest[..word.len()].eq_ignore_ascii_case(word.as_bytes()) {
            self.pos += word.len();
            true
        } else {
            false
        }
    }
}

fn conversion_card(query: &str) -> Option<RichCard> {
    let parts: Vec<&str> = query.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    let amount = parts[0].replace(',', "").parse::<f64>().ok()?;
    let sep = parts
        .iter()
        .position(|p| p.eq_ignore_ascii_case("to") || p.eq_ignore_ascii_case("in"))?;
    if sep < 2 || sep + 1 >= parts.len() {
        return None;
    }
    let from = normalize_unit(&parts[1..sep].join(" "))?;
    let to = normalize_unit(&parts[sep + 1..].join(" "))?;
    let converted = convert(amount, from, to)?;
    Some(RichCard {
        label: "Unit conversion",
        title: format!(
            "{} {} to {}",
            format_number(amount),
            unit_label(from, amount),
            unit_label(to, converted)
        ),
        value: format!(
            "{} {} = {} {}",
            format_number(amount),
            unit_label(from, amount),
            format_number(converted),
            unit_label(to, converted)
        ),
        detail: None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    Meter,
    Kilometer,
    Centimeter,
    Millimeter,
    Mile,
    Foot,
    Inch,
    Kilogram,
    Gram,
    Pound,
    Ounce,
    Celsius,
    Fahrenheit,
    Kelvin,
}

fn normalize_unit(raw: &str) -> Option<Unit> {
    let u = raw.trim().to_lowercase();
    Some(match u.as_str() {
        "m" | "meter" | "meters" | "metre" | "metres" => Unit::Meter,
        "km" | "kilometer" | "kilometers" | "kilometre" | "kilometres" => Unit::Kilometer,
        "cm" | "centimeter" | "centimeters" | "centimetre" | "centimetres" => Unit::Centimeter,
        "mm" | "millimeter" | "millimeters" | "millimetre" | "millimetres" => Unit::Millimeter,
        "mi" | "mile" | "miles" => Unit::Mile,
        "ft" | "foot" | "feet" => Unit::Foot,
        "in" | "inch" | "inches" => Unit::Inch,
        "kg" | "kilogram" | "kilograms" => Unit::Kilogram,
        "g" | "gram" | "grams" => Unit::Gram,
        "lb" | "lbs" | "pound" | "pounds" => Unit::Pound,
        "oz" | "ounce" | "ounces" => Unit::Ounce,
        "c" | "celsius" | "deg c" | "degree c" | "degrees c" => Unit::Celsius,
        "f" | "fahrenheit" | "deg f" | "degree f" | "degrees f" => Unit::Fahrenheit,
        "k" | "kelvin" => Unit::Kelvin,
        _ => return None,
    })
}

fn convert(amount: f64, from: Unit, to: Unit) -> Option<f64> {
    if length_factor(from).is_some() && length_factor(to).is_some() {
        return Some(amount * length_factor(from)? / length_factor(to)?);
    }
    if mass_factor(from).is_some() && mass_factor(to).is_some() {
        return Some(amount * mass_factor(from)? / mass_factor(to)?);
    }
    if is_temp(from) && is_temp(to) {
        return Some(kelvin_to_unit(unit_to_kelvin(amount, from)?, to));
    }
    None
}

fn length_factor(u: Unit) -> Option<f64> {
    Some(match u {
        Unit::Meter => 1.0,
        Unit::Kilometer => 1000.0,
        Unit::Centimeter => 0.01,
        Unit::Millimeter => 0.001,
        Unit::Mile => 1609.344,
        Unit::Foot => 0.3048,
        Unit::Inch => 0.0254,
        _ => return None,
    })
}

fn mass_factor(u: Unit) -> Option<f64> {
    Some(match u {
        Unit::Kilogram => 1.0,
        Unit::Gram => 0.001,
        Unit::Pound => 0.453_592_37,
        Unit::Ounce => 0.028_349_523_125,
        _ => return None,
    })
}

fn is_temp(u: Unit) -> bool {
    matches!(u, Unit::Celsius | Unit::Fahrenheit | Unit::Kelvin)
}

fn unit_to_kelvin(v: f64, u: Unit) -> Option<f64> {
    Some(match u {
        Unit::Celsius => v + 273.15,
        Unit::Fahrenheit => (v - 32.0) * 5.0 / 9.0 + 273.15,
        Unit::Kelvin => v,
        _ => return None,
    })
}

fn kelvin_to_unit(v: f64, u: Unit) -> f64 {
    match u {
        Unit::Celsius => v - 273.15,
        Unit::Fahrenheit => (v - 273.15) * 9.0 / 5.0 + 32.0,
        Unit::Kelvin => v,
        _ => v,
    }
}

fn unit_label(u: Unit, v: f64) -> &'static str {
    let singular = (v.abs() - 1.0).abs() < 1e-9;
    match u {
        Unit::Meter => {
            if singular {
                "meter"
            } else {
                "meters"
            }
        }
        Unit::Kilometer => {
            if singular {
                "kilometer"
            } else {
                "kilometers"
            }
        }
        Unit::Centimeter => {
            if singular {
                "centimeter"
            } else {
                "centimeters"
            }
        }
        Unit::Millimeter => {
            if singular {
                "millimeter"
            } else {
                "millimeters"
            }
        }
        Unit::Mile => {
            if singular {
                "mile"
            } else {
                "miles"
            }
        }
        Unit::Foot => {
            if singular {
                "foot"
            } else {
                "feet"
            }
        }
        Unit::Inch => {
            if singular {
                "inch"
            } else {
                "inches"
            }
        }
        Unit::Kilogram => {
            if singular {
                "kilogram"
            } else {
                "kilograms"
            }
        }
        Unit::Gram => {
            if singular {
                "gram"
            } else {
                "grams"
            }
        }
        Unit::Pound => {
            if singular {
                "pound"
            } else {
                "pounds"
            }
        }
        Unit::Ounce => {
            if singular {
                "ounce"
            } else {
                "ounces"
            }
        }
        Unit::Celsius => "C",
        Unit::Fahrenheit => "F",
        Unit::Kelvin => "K",
    }
}

fn format_number(v: f64) -> String {
    if (v.round() - v).abs() < 1e-10 {
        return format!("{}", v.round() as i64);
    }
    let s = format!("{v:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculator_handles_precedence_parentheses_and_sqrt() {
        assert_eq!(calculator_card("2 + 2").unwrap().value, "4");
        assert_eq!(calculator_card("2 * (5 + 3)").unwrap().value, "16");
        assert_eq!(calculator_card("2^8").unwrap().value, "256");
        assert_eq!(calculator_card("sqrt(16)").unwrap().value, "4");
    }

    #[test]
    fn calculator_rejects_plain_text_and_division_by_zero() {
        assert!(calculator_card("rust ownership").is_none());
        assert!(calculator_card("1 / 0").is_none());
    }

    #[test]
    fn conversion_handles_length_mass_and_temperature() {
        assert_eq!(
            conversion_card("10 km to miles").unwrap().value,
            "10 kilometers = 6.213712 miles"
        );
        assert_eq!(
            conversion_card("5 kg in lb").unwrap().value,
            "5 kilograms = 11.023113 pounds"
        );
        assert_eq!(conversion_card("32 f to c").unwrap().value, "32 F = 0 C");
    }

    #[test]
    fn conversion_rejects_incompatible_units() {
        assert!(conversion_card("10 kg to miles").is_none());
        assert!(conversion_card("rust to miles").is_none());
    }

    #[test]
    fn detects_definition_and_weather_queries() {
        assert!(is_definition_query("define entropy"));
        assert!(is_definition_query("what is ownership"));
        assert!(is_weather_query("weather in sydney"));
        assert!(!is_weather_query("whether rust is fast"));
    }
}
