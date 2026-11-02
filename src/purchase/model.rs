use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    Equals,
    AtMost,
    AtLeast,
    Contains,
    OneOf,
    Excludes,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub key: String,
    pub label: String,
    pub operator: Operator,
    pub expected: Value,
    #[serde(default = "yes")]
    pub mandatory: bool,
    #[serde(default = "yes")]
    pub seller_confirmation: bool,
}
fn yes() -> bool {
    true
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub version: u32,
    pub title: String,
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub searches: Vec<String>,
    #[serde(default)]
    pub approved: bool,
    #[serde(default = "yes")]
    pub trusted_importer_required: bool,
    #[serde(default)]
    pub paused: bool,
}
impl Task {
    pub fn gpu() -> Self {
        Self {
            id: 1,
            version: 1,
            title: "NVIDIA RTX PRO 6000 Blackwell — уточнить требования".into(),
            approved: false,
            searches: vec![
                "https://www.avito.ru/rossiya?q=rtx+pro+6000".into(),
                "https://www.avito.ru/rossiya?q=rtx+6000+blackwell".into(),
            ],
            trusted_importer_required: true,
            paused: true,
            rules: vec![
                Rule {
                    key: "model".into(),
                    label: "RTX PRO 6000 Blackwell".into(),
                    operator: Operator::Contains,
                    expected: "rtx pro 6000 blackwell".into(),
                    mandatory: true,
                    seller_confirmation: true,
                },
                Rule {
                    key: "memory_gb".into(),
                    label: "96 ГБ (требует проверки владельцем)".into(),
                    operator: Operator::Equals,
                    expected: 96.into(),
                    mandatory: true,
                    seller_confirmation: true,
                },
                Rule {
                    key: "edition".into(),
                    label: "Исключить Server Edition".into(),
                    operator: Operator::Excludes,
                    expected: "server".into(),
                    mandatory: true,
                    seller_confirmation: true,
                },
            ],
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.title.trim().is_empty() && !self.rules.is_empty() && self.rules.len() <= 40,
            "Пустая или слишком большая задача"
        );
        for rule in &self.rules {
            ensure!(
                !rule.key.is_empty()
                    && rule
                        .key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "Неверный ключ условия"
            );
            match rule.operator {
                Operator::AtMost | Operator::AtLeast => ensure!(
                    rule.expected.as_f64().is_some(),
                    "Числовой порог обязателен"
                ),
                Operator::OneOf => ensure!(
                    rule.expected.as_array().is_some_and(|a| !a.is_empty()),
                    "Нужен непустой список значений"
                ),
                _ => {}
            }
        }
        for url in &self.searches {
            crate::domain::SearchUrl::parse(url)?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Listing,
    Seller,
    Buyer,
    Owner,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub value: Value,
    pub origin: Origin,
    pub reference: String,
    pub quote: String,
    pub observed_at: i64,
    #[serde(default)]
    pub contradiction: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Assessment {
    pub facts: BTreeMap<String, Fact>,
    #[serde(default)]
    pub questions: Vec<String>,
    #[serde(default)]
    pub explanation: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Candidate,
    NeedsDetails,
    Rejected,
    Matched,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub status: Status,
    pub tier: u8,
    pub reasons: Vec<String>,
}
fn normal(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}
fn satisfies(rule: &Rule, value: &Value) -> bool {
    match rule.operator {
        Operator::Equals => {
            if let (Some(a), Some(b)) = (value.as_str(), rule.expected.as_str()) {
                normal(a) == normal(b)
            } else {
                value == &rule.expected
            }
        }
        Operator::AtMost => value
            .as_f64()
            .zip(rule.expected.as_f64())
            .is_some_and(|(a, b)| a <= b),
        Operator::AtLeast => value
            .as_f64()
            .zip(rule.expected.as_f64())
            .is_some_and(|(a, b)| a >= b),
        Operator::Contains => value
            .as_str()
            .zip(rule.expected.as_str())
            .is_some_and(|(a, b)| normal(a).contains(&normal(b))),
        Operator::Excludes => value
            .as_str()
            .zip(rule.expected.as_str())
            .is_some_and(|(a, b)| !normal(a).contains(&normal(b))),
        Operator::OneOf => rule.expected.as_array().is_some_and(|a| a.contains(value)),
    }
}
pub fn evaluate(task: &Task, assessment: &Assessment, now: i64) -> Verdict {
    let mut missing = vec![];
    let mut rejected = vec![];
    for rule in task.rules.iter().filter(|r| r.mandatory) {
        match assessment.facts.get(&rule.key) {
            None => missing.push(rule.label.clone()),
            Some(f) if now - f.observed_at > 86400 || f.observed_at > now + 60 => {
                missing.push(format!(
                    "{}: противоречие или неактуальное подтверждение",
                    rule.label
                ))
            }
            Some(f) if !satisfies(rule, &f.value) => rejected.push(rule.label.clone()),
            Some(f) if f.contradiction => {
                missing.push(format!("{}: противоречивые сведения", rule.label))
            }
            Some(f) if rule.seller_confirmation && f.origin != Origin::Seller => {
                missing.push(format!("{}: нужен ответ продавца", rule.label))
            }
            _ => {}
        }
    }
    let confirmed = |key: &str| {
        assessment.facts.get(key).filter(|f| {
            f.origin == Origin::Seller
                && !f.contradiction
                && now - f.observed_at <= 86400
                && f.observed_at <= now + 60
        })
    };
    let tier = match confirmed("availability").and_then(|f| f.value.as_str()) {
        Some("in_stock") => 1,
        Some("order") => match confirmed("deposit_percent").and_then(|f| f.value.as_f64()) {
            Some(0.0) => 2,
            Some(p) if p > 0.0 && p < 100.0 => 3,
            _ => {
                missing.push("Уточнить допустимую предоплату".into());
                4
            }
        },
        _ => {
            missing.push("Нужно подтвердить наличие или заказ".into());
            4
        }
    };
    if tier == 2 || tier == 3 {
        if task.trusted_importer_required
            && !assessment
                .facts
                .get("shop_verified_by_owner")
                .is_some_and(|f| {
                    f.origin == Origin::Owner
                        && f.value == true
                        && now - f.observed_at <= 30 * 86400
                })
        {
            missing.push("Нужна проверка магазина владельцем".into());
        }
        if tier == 3
            && !task.rules.iter().any(|r| {
                r.mandatory && r.key == "deposit_percent" && matches!(r.operator, Operator::AtMost)
            })
        {
            missing.push("Не задан лимит предоплаты".into());
        }
    }
    if !task.approved {
        missing.push("Требования ещё не подтверждены владельцем".into());
    }
    let status = if !rejected.is_empty() {
        Status::Rejected
    } else if !missing.is_empty() {
        Status::NeedsDetails
    } else {
        Status::Matched
    };
    rejected.extend(missing);
    Verdict {
        status,
        tier,
        reasons: rejected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn facts(task: &Task) -> Assessment {
        let mut a = Assessment::default();
        for r in &task.rules {
            a.facts.insert(
                r.key.clone(),
                Fact {
                    value: if r.key == "edition" {
                        "workstation".into()
                    } else {
                        r.expected.clone()
                    },
                    origin: Origin::Seller,
                    reference: "msg1".into(),
                    quote: "proof".into(),
                    observed_at: 100,
                    contradiction: false,
                },
            );
        }
        a.facts.insert(
            "availability".into(),
            Fact {
                value: "in_stock".into(),
                origin: Origin::Seller,
                reference: "msg2".into(),
                quote: "stock".into(),
                observed_at: 100,
                contradiction: false,
            },
        );
        a
    }
    #[test]
    fn unknown_and_listing_evidence_never_match() {
        let mut t = Task::gpu();
        t.approved = true;
        let mut a = facts(&t);
        assert_eq!(evaluate(&t, &a, 100).status, Status::Matched);
        a.facts.get_mut("edition").unwrap().origin = Origin::Listing;
        assert_eq!(evaluate(&t, &a, 100).status, Status::NeedsDetails);
        a.facts.remove("edition");
        assert_eq!(evaluate(&t, &a, 100).status, Status::NeedsDetails);
    }
    #[test]
    fn server_is_rejected_even_with_other_facts() {
        let mut t = Task::gpu();
        t.approved = true;
        let mut a = facts(&t);
        a.facts.get_mut("edition").unwrap().value = "server edition".into();
        assert_eq!(evaluate(&t, &a, 100).status, Status::Rejected);
        assert_ne!(evaluate(&t, &facts(&t), 90000).status, Status::Matched);
    }
}
