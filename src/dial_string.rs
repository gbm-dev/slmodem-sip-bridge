use anyhow::{bail, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialString(String);

impl DialString {
    pub fn parse(input: &str) -> Result<Self> {
        let stripped = strip_tone_pulse_prefix(input);
        if stripped.is_empty() {
            return Ok(Self(String::new()));
        }

        if !stripped.chars().all(is_allowed_dial_char) {
            bail!("dial string contains unsupported characters: {stripped}");
        }

        Ok(Self(stripped.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn strip_tone_pulse_prefix(input: &str) -> &str {
    input.trim_start_matches(['T', 't', 'P', 'p'])
}

fn is_allowed_dial_char(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '*' | '#' | '+' | ',' | 'w' | 'W')
}

#[cfg(test)]
mod tests {
    use super::DialString;

    #[test]
    fn strips_tone_prefix() {
        let dial = DialString::parse("T18005551212").unwrap();
        assert_eq!(dial.as_str(), "18005551212");
    }

    #[test]
    fn strips_multiple_prefix_chars() {
        let dial = DialString::parse("tPpW123").unwrap();
        assert_eq!(dial.as_str(), "W123");
    }

    #[test]
    fn rejects_empty_after_prefix_strip() {
        let err = DialString::parse("TPtp").expect_err("must reject empty dial string");
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn rejects_invalid_characters() {
        let err = DialString::parse("1800-555-1212").expect_err("must reject '-' characters");
        assert!(err.to_string().contains("unsupported characters"));
    }

    #[test]
    fn allows_common_modem_dial_chars() {
        let dial = DialString::parse("+15551212,,W9#").unwrap();
        assert_eq!(dial.as_str(), "+15551212,,W9#");
    }
}
