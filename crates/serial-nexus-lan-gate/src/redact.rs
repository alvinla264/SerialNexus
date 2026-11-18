//! Diagnostics redaction for the LAN gate.
//!
//! The gate never logs pairing material on purpose. This is the second line of
//! defence: an error string from a future dependency is masked before it can
//! reach a container log, a terminal log or a CI transcript.

/// Replace anything that looks like a sharing key or a credential value.
///
/// A sharing key is `snx<digits>.<role>.<base64url>`; a pairing credential is
/// an opaque ASCII token. Masking whole whitespace-separated tokens (plus the
/// JSON `"credential":"..."` form) keeps the rest of the message readable.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, token) in text.split(' ').enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(&redact_token(token));
    }
    out
}

fn redact_token(token: &str) -> String {
    let trimmed = token.trim_matches(|c: char| c == '"' || c == ',' || c == '\'' || c == ')');
    let is_key = trimmed
        .strip_prefix("snx")
        .and_then(|rest| rest.split_once('.'))
        .is_some_and(|(version, rest)| {
            !version.is_empty()
                && version.bytes().all(|byte| byte.is_ascii_digit())
                && (rest.starts_with("human.") || rest.starts_with("ai."))
        });
    // Only a credential with a value is masked. The plain word "credential"
    // stays readable: diagnostics such as "invalid credential refused" name
    // the kind of check without carrying a secret.
    let lowered = trimmed.to_ascii_lowercase();
    let credential_with_value = lowered.contains("credential")
        && (lowered.contains("credential=")
            || lowered.contains("credential:")
            || lowered.contains("\"credential\""));
    if is_key || credential_with_value {
        return "<redacted>".to_string();
    }
    token.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_credentials_are_masked_and_ordinary_text_survives() {
        let key = "snx1.human.eyJ2IjoxLCJyIjoiaHVtYW4ifQ";
        let text = format!(
            "pair failed for {key} with credential=abc123 at 172.18.0.2:7331: authentication failed"
        );
        let masked = redact(&text);
        assert!(!masked.contains(key), "{masked}");
        assert!(!masked.contains("abc123"), "{masked}");
        assert!(masked.contains("172.18.0.2:7331"), "{masked}");
        assert!(masked.contains("authentication failed"), "{masked}");
    }

    #[test]
    fn ordinary_words_that_start_with_snx_are_not_touched() {
        assert_eq!(redact("snx is not a key"), "snx is not a key");
        assert_eq!(redact("snx1..bad"), "snx1..bad");
    }

    #[test]
    fn naming_a_credential_check_does_not_mask_the_words() {
        assert_eq!(
            redact("invalid credential refused"),
            "invalid credential refused"
        );
        assert_eq!(redact("role from credential"), "role from credential");
    }
}
