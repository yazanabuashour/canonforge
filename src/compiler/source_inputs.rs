use anyhow::{Result, bail, ensure};
use serde_json::Value;

use super::{
    SourceRole,
    json_support::{locator_str, locator_strings},
};

pub(super) struct SourceInput {
    pub path: String,
    pub role: SourceRole,
}

pub(super) fn source_inputs(source_type: &str, locator: &Value) -> Result<Vec<SourceInput>> {
    let role = match source_type {
        "canonical-markdown" => SourceRole::Markdown,
        "conversation-chatgpt" => SourceRole::ChatGpt,
        "conversation-email" => SourceRole::Email,
        "conversation-table" => SourceRole::ConversationTable,
        "docling-json" => {
            let document = locator_str(locator, "file")?;
            let original = locator_str(locator, "original_file")?;
            ensure!(
                document != original,
                "Docling JSON and original source must be different files"
            );
            return Ok(vec![
                SourceInput {
                    path: document.to_owned(),
                    role: SourceRole::Docling,
                },
                SourceInput {
                    path: original.to_owned(),
                    role: SourceRole::ReceiptOnly,
                },
            ]);
        }
        "execution-history" => {
            return Ok(locator_strings(locator, "files")?
                .into_iter()
                .map(|path| SourceInput {
                    path: path.to_owned(),
                    role: SourceRole::Execution,
                })
                .collect());
        }
        value => bail!("unsupported evidence source type {value}"),
    };
    Ok(vec![SourceInput {
        path: locator_str(locator, "file")?.to_owned(),
        role,
    }])
}
