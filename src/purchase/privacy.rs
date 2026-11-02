use regex::Regex;
use std::sync::OnceLock;
pub fn redact(text: &str) -> String {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            r"(?i)(?:github_pat_|ghp_|sk-)[a-z0-9_-]+",
            r"[A-Za-z0-9_.+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
            r"(?:\+?\d[\d ()-]{8,}\d)",
        ]
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
    });
    let mut value = text
        .lines()
        .filter(|line| {
            let lower = line.to_lowercase();
            !["пароль", "password", "код доступа", "логин", "access token"]
                .iter()
                .any(|word| lower.contains(word))
        })
        .collect::<Vec<_>>()
        .join("\n");
    for pattern in patterns {
        value = pattern.replace_all(&value, "[СКРЫТО]").into_owned();
    }
    value
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn masks_credentials_and_contact_details() {
        let value = redact(
            "RTX PRO 6000, 96 GB\nпароль: secret\nuser@example.com +7 (900) 123-45-67\nsk-secret",
        );
        assert!(value.contains("96 GB"));
        assert!(!value.contains("secret"));
        assert!(!value.contains("example.com"));
        assert!(!value.contains("900"));
    }
}
