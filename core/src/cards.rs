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
    weather_card(query)
        .or_else(|| percentage_card(query))
        .or_else(|| conversion_card(query))
        .or_else(|| calculator_card(query))
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
        || q.trim() == "weather"
        || q.contains(" weather in ")
        || q.starts_with("temperature ")
        || q.contains(" forecast")
}

fn weather_card(query: &str) -> Option<RichCard> {
    if !is_weather_query(query) {
        return None;
    }
    let path = std::env::var("OMNI_WEATHER_FILE").ok()?;
    let data = std::fs::read_to_string(path).ok()?;
    weather_card_from_str(query, &data)
}

fn weather_card_from_str(query: &str, data: &str) -> Option<RichCard> {
    let records = parse_weather_records(data);
    if records.is_empty() {
        return None;
    }
    let wanted = weather_location(query);
    let record = if wanted.is_empty() && records.len() == 1 {
        records.first()?
    } else {
        records.iter().find(|r| r.matches(&wanted))?
    };
    Some(record.card())
}

#[derive(Default)]
struct WeatherRecord {
    location: String,
    aliases: Vec<String>,
    temperature: String,
    condition: String,
    feels_like: String,
    humidity: String,
    wind: String,
    updated: String,
    source: String,
}

impl WeatherRecord {
    fn matches(&self, wanted: &str) -> bool {
        let wanted = normalize_weather_key(wanted);
        if wanted.is_empty() {
            return false;
        }
        std::iter::once(&self.location)
            .chain(self.aliases.iter())
            .any(|name| {
                let name = normalize_weather_key(name);
                name == wanted || name.contains(&wanted) || wanted.contains(&name)
            })
    }

    fn card(&self) -> RichCard {
        let mut details = Vec::new();
        if !self.condition.is_empty() {
            details.push(self.condition.clone());
        }
        if !self.feels_like.is_empty() {
            details.push(format!("Feels like {}", self.feels_like));
        }
        if !self.humidity.is_empty() {
            details.push(format!("Humidity {}", self.humidity));
        }
        if !self.wind.is_empty() {
            details.push(format!("Wind {}", self.wind));
        }
        if !self.updated.is_empty() {
            details.push(format!("Updated {}", self.updated));
        }
        if !self.source.is_empty() {
            details.push(format!("Source {}", self.source));
        }
        RichCard {
            label: "Weather",
            title: self.location.clone(),
            value: self.temperature.clone(),
            detail: (!details.is_empty()).then(|| details.join(" · ")),
        }
    }
}

fn parse_weather_records(data: &str) -> Vec<WeatherRecord> {
    data.split("\n\n")
        .filter_map(|block| {
            let mut rec = WeatherRecord::default();
            for line in block.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let Some((key, val)) = line.split_once(':') else {
                    continue;
                };
                let val = val.trim().to_string();
                match key.trim().to_ascii_lowercase().as_str() {
                    "location" | "place" => rec.location = val,
                    "alias" | "aliases" => {
                        rec.aliases = val
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    }
                    "temp" | "temperature" => rec.temperature = val,
                    "condition" | "summary" => rec.condition = val,
                    "feels_like" | "feels like" => rec.feels_like = val,
                    "humidity" => rec.humidity = val,
                    "wind" => rec.wind = val,
                    "updated" | "observed" => rec.updated = val,
                    "source" => rec.source = val,
                    _ => {}
                }
            }
            (!rec.location.is_empty() && !rec.temperature.is_empty()).then_some(rec)
        })
        .collect()
}

fn weather_location(query: &str) -> String {
    let mut q = query.trim().to_lowercase();
    if matches!(q.as_str(), "weather" | "forecast" | "temperature") {
        return String::new();
    }
    for prefix in [
        "weather forecast for ",
        "weather forecast in ",
        "weather in ",
        "weather for ",
        "weather at ",
        "weather ",
        "temperature in ",
        "temperature for ",
        "temperature ",
        "forecast in ",
        "forecast for ",
        "forecast ",
    ] {
        if let Some(rest) = q.strip_prefix(prefix) {
            q = rest.trim().to_string();
            break;
        }
    }
    q.trim_matches(|c: char| c == '?' || c.is_ascii_whitespace())
        .to_string()
}

fn normalize_weather_key(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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

fn percentage_card(query: &str) -> Option<RichCard> {
    let q = normalize_percentage_query(query);
    if let Some(card) = percentage_of_card(&q) {
        return Some(card);
    }
    if let Some(card) = percent_ratio_card(&q) {
        return Some(card);
    }
    percentage_question_card(&q)
}

fn normalize_percentage_query(query: &str) -> String {
    let mut q = query.trim().trim_end_matches('?').to_lowercase();
    for prefix in ["what is ", "what's ", "calculate ", "find "] {
        if let Some(rest) = q.strip_prefix(prefix) {
            q = rest.trim().to_string();
            break;
        }
    }
    q
}

fn percentage_of_card(q: &str) -> Option<RichCard> {
    if let Some((lhs, rhs)) = q.split_once('%') {
        let pct = parse_card_number(lhs)?;
        let rhs = rhs.trim().strip_prefix("of")?.trim();
        let amount = parse_card_number(rhs)?;
        return Some(percentage_of_result(pct, amount));
    }
    let (lhs, rhs) = q.split_once(" percent of ")?;
    let pct = parse_card_number(lhs)?;
    let amount = parse_card_number(rhs)?;
    Some(percentage_of_result(pct, amount))
}

fn percentage_of_result(pct: f64, amount: f64) -> RichCard {
    let value = pct / 100.0 * amount;
    RichCard {
        label: "Percentage",
        title: format!("{}% of {}", format_number(pct), format_number(amount)),
        value: format_number(value),
        detail: Some(format!(
            "{} / 100 * {}",
            format_number(pct),
            format_number(amount)
        )),
    }
}

fn percent_ratio_card(q: &str) -> Option<RichCard> {
    let (lhs, rhs) = q.split_once(" is what percent of ")?;
    let part = parse_card_number(lhs)?;
    let whole = parse_card_number(rhs)?;
    percent_ratio_result(part, whole)
}

fn percentage_question_card(q: &str) -> Option<RichCard> {
    let rest = q.strip_prefix("what percentage is ")?;
    let (lhs, rhs) = rest.split_once(" of ")?;
    let part = parse_card_number(lhs)?;
    let whole = parse_card_number(rhs)?;
    percent_ratio_result(part, whole)
}

fn percent_ratio_result(part: f64, whole: f64) -> Option<RichCard> {
    if whole == 0.0 {
        return None;
    }
    let pct = part / whole * 100.0;
    Some(RichCard {
        label: "Percentage",
        title: format!(
            "{} as a percentage of {}",
            format_number(part),
            format_number(whole)
        ),
        value: format!("{}%", format_number(pct)),
        detail: Some(format!(
            "{} / {} * 100",
            format_number(part),
            format_number(whole)
        )),
    })
}

fn parse_card_number(s: &str) -> Option<f64> {
    let v = s.trim().replace(',', "").parse::<f64>().ok()?;
    v.is_finite().then_some(v)
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
    fn percentage_card_handles_common_google_forms() {
        let card = percentage_card("20% of 80").unwrap();
        assert_eq!(card.label, "Percentage");
        assert_eq!(card.value, "16");
        assert_eq!(card.detail.as_deref(), Some("20 / 100 * 80"));

        assert_eq!(
            percentage_card("what is 12.5 percent of 240")
                .unwrap()
                .value,
            "30"
        );
        assert_eq!(
            percentage_card("20 is what percent of 80").unwrap().value,
            "25%"
        );
        assert_eq!(
            percentage_card("what percentage is 3 of 12").unwrap().value,
            "25%"
        );
    }

    #[test]
    fn percentage_card_rejects_invalid_ratios() {
        assert!(percentage_card("20 is what percent of 0").is_none());
        assert!(percentage_card("rust percent of search").is_none());
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

    #[test]
    fn weather_card_uses_configured_records_and_aliases() {
        let data = "location: Sydney, NSW\n\
                    aliases: sydney, syd\n\
                    temperature: 18 C\n\
                    condition: Cloudy\n\
                    feels_like: 17 C\n\
                    humidity: 72%\n\
                    wind: SE 12 km/h\n\
                    updated: 2026-06-22T09:00:00+10:00\n\
                    source: BOM cache\n\
                    \n\
                    location: Melbourne\n\
                    temperature: 11 C\n\
                    condition: Rain";

        let card = weather_card_from_str("weather in syd", data).unwrap();
        assert_eq!(card.label, "Weather");
        assert_eq!(card.title, "Sydney, NSW");
        assert_eq!(card.value, "18 C");
        assert!(card.detail.unwrap().contains("Humidity 72%"));
    }

    #[test]
    fn weather_card_requires_data_but_allows_single_default_location() {
        let data = "location: Canberra\n\
                    temperature: 9 C\n\
                    condition: Clear";
        assert_eq!(
            weather_card_from_str("weather", data).unwrap().title,
            "Canberra"
        );
        assert!(weather_card_from_str("weather in perth", data).is_none());
        assert!(weather_card_from_str("weather in perth", "").is_none());
    }
}
