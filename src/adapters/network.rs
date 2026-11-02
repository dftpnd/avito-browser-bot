//! Fail closed when transparent policy routing or proxies prevent verification.
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::process::Command;
#[derive(Debug, Serialize)]
pub struct NetworkAudit {
    pub direct: bool,
    pub reasons: Vec<String>,
}
pub fn audit() -> Result<NetworkAudit> {
    let mut reasons = vec![];
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        if std::env::var(name).is_ok_and(|v| !v.is_empty()) {
            reasons.push(format!("Задан {name}"));
        }
    }
    let rules = Command::new("ip")
        .args(["-j", "rule"])
        .output()
        .context("Cannot inspect network policy")?;
    ensure!(rules.status.success(), "Cannot inspect network policy");
    let rules: serde_json::Value = serde_json::from_slice(&rules.stdout)?;
    for rule in rules
        .as_array()
        .context("Invalid policy routing response")?
    {
        if rule.get("fwmark").is_some()
            || !matches!(rule["table"].as_str(), Some("local" | "main" | "default"))
        {
            reasons.push(
                "Есть дополнительная политика маршрутизации: прямой трафик Авито не подтверждён"
                    .into(),
            );
        }
    }
    let routes = Command::new("ip")
        .args(["-j", "route", "show", "default"])
        .output()?;
    ensure!(routes.status.success(), "Cannot inspect default route");
    let routes: serde_json::Value = serde_json::from_slice(&routes.stdout)?;
    if !routes.as_array().is_some_and(|r| {
        r.iter().any(|r| {
            r["dev"]
                .as_str()
                .is_some_and(|d| d.starts_with("en") || d.starts_with("wl"))
        })
    }) {
        reasons.push("Не найден обычный физический маршрут по умолчанию".into());
    }
    Ok(NetworkAudit {
        direct: reasons.is_empty(),
        reasons,
    })
}
pub fn require_direct() -> Result<()> {
    let state = audit()?;
    ensure!(
        state.direct,
        "Авито приостановлен: {}",
        state.reasons.join("; ")
    );
    Ok(())
}
