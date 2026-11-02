use crate::domain::Listing;
use anyhow::Result;
use std::{fmt::Write, path::Path};

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn write_html(path: &Path, listings: &[Listing]) -> Result<()> {
    let mut html = String::from(
        "<!doctype html><html lang=ru><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'><title>Объявления Авито</title><style>body{font:16px system-ui;max-width:1000px;margin:40px auto;padding:0 20px}table{border-collapse:collapse;width:100%}td,th{text-align:left;padding:12px;border-bottom:1px solid #ddd}a{color:#0064ca}</style><h1>Сохранённые объявления</h1><p>История наблюдений: наличие объявления в отчёте не означает, что оно ещё активно.</p><table><tr><th>Название</th><th>Цена, ₽</th><th>Место</th></tr>",
    );
    for item in listings {
        item.validate()?;
        writeln!(
            html,
            "<tr><td><a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a></td><td>{}</td><td>{}</td></tr>",
            escape(&item.url),
            escape(&item.title),
            item.price
                .map(|p| p.to_string())
                .unwrap_or_else(|| "—".into()),
            escape(&item.location)
        )?;
    }
    html.push_str("</table></html>");
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, html)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escapes_untrusted_text_and_attributes() {
        assert_eq!(escape("<script>\"&'"), "&lt;script&gt;&quot;&amp;&#39;");
    }
}
