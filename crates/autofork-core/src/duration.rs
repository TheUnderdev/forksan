//! Duration parsing shared by frontmatter and config: a bare number is
//! seconds; a string takes an optional `s`/`m`/`h`/`d` suffix (`"30s"`,
//! `"15m"`, `"2h"`, `"1d"`).

/// Parse a YAML duration value into seconds.
pub fn parse_duration_yaml(v: &serde_yaml::Value) -> Option<u64> {
    match v {
        serde_yaml::Value::Number(n) => n.as_u64(),
        serde_yaml::Value::String(s) => parse_duration_str(s),
        _ => None,
    }
}

/// Parse a duration string into seconds.
pub fn parse_duration_str(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        'd' => (&s[..s.len() - 1], 86400),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    num.trim().parse::<u64>().ok().map(|n| n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes_and_bare_numbers() {
        assert_eq!(parse_duration_str("30s"), Some(30));
        assert_eq!(parse_duration_str("15m"), Some(900));
        assert_eq!(parse_duration_str("2h"), Some(7200));
        assert_eq!(parse_duration_str("1d"), Some(86400));
        assert_eq!(parse_duration_str("45"), Some(45));
        assert_eq!(parse_duration_str(" 10m "), Some(600));
        assert_eq!(parse_duration_str("x"), None);
        assert_eq!(parse_duration_str(""), None);
        assert_eq!(parse_duration_str("10w"), None);
        assert_eq!(
            parse_duration_yaml(&serde_yaml::Value::Number(90.into())),
            Some(90)
        );
        assert_eq!(parse_duration_yaml(&serde_yaml::Value::Bool(true)), None);
    }
}
