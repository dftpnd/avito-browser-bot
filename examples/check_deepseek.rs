//! Optional live smoke check on synthetic public data; sends no account messages.
use avito_browser_bot::purchase::{
    deepseek::{DeepSeek, FactExtractor, Source},
    model::{Origin, Status, Task, evaluate},
    store::now,
};
fn main() -> anyhow::Result<()> {
    let mut model = DeepSeek::load(std::path::Path::new("config.local.deepseek.json"))?;
    let task = Task::gpu();
    let source=Source{reference:"synthetic-listing".into(),origin:Origin::Listing,observed_at:now(),text:"NVIDIA RTX PRO 6000 Blackwell Server Edition, 96 GB. Это тестовый текст, не настоящее объявление. Цена и наличие неизвестны.".into()};
    let result = model.extract(&task, &[source])?;
    let verdict = evaluate(&task, &result.assessment, now());
    if verdict.status != Status::Rejected {
        println!(
            "Synthetic assessment: {}",
            serde_json::to_string(&result.assessment)?
        );
    }
    anyhow::ensure!(
        verdict.status == Status::Rejected,
        "Server Edition was not excluded"
    );
    println!(
        "DeepSeek smoke check passed; Server Edition excluded. Tokens: input={}, output={}",
        result.input_tokens, result.output_tokens
    );
    Ok(())
}
