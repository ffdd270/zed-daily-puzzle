use super::bridge::Backend;
use anyhow::Result;
use editor::{CompletionContext, CompletionProvider, Editor};
use gpui::{Context, Entity, Task, Window};
use language::{Anchor, Buffer, CodeLabel, ToOffset, ToPoint};
use project::{Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource};
use serde_json::json;
use std::sync::Arc;

pub struct PuzzleCompletions {
    pub backend: Arc<Backend>,
    pub session: String,
    pub vocabulary: Vec<String>,
}

impl CompletionProvider for PuzzleCompletions {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        position: Anchor,
        _: CompletionContext,
        _: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let point = position.to_point(&snapshot);
        let code = snapshot.text();
        let prefix = code.get(..offset).unwrap_or_default();
        let word_length = prefix
            .chars()
            .rev()
            .take_while(|character| character.is_alphanumeric() || *character == '_')
            .map(char::len_utf8)
            .sum::<usize>();
        let start = snapshot.anchor_before(offset.saturating_sub(word_length));
        let end = snapshot.anchor_after(offset);
        let request = self
            .backend
            .request(json!({"action":"completion","session_id":self.session,
            "code":code,"line":point.row,"column":point.column}));
        let vocabulary = self.vocabulary.clone();
        cx.spawn(async move |_, _| {
            let response = request.result().await?;
            let mut words: Vec<String> = response
                .get("items")
                .and_then(|value| value.as_array())
                .into_iter()
                .flatten()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect();
            words.extend(vocabulary);
            words.sort();
            words.dedup();
            let completions = words
                .into_iter()
                .map(|word| Completion {
                    replace_range: start..end,
                    new_text: word.clone(),
                    label: CodeLabel::plain(word, None),
                    documentation: None,
                    source: CompletionSource::Custom,
                    icon_path: None,
                    icon_color: None,
                    match_start: Some(start),
                    snippet_deduplication_key: None,
                    insert_text_mode: None,
                    confirm: None,
                    group: None,
                })
                .collect();
            Ok(vec![CompletionResponse {
                completions,
                display_options: CompletionDisplayOptions::default(),
                is_incomplete: true,
            }])
        })
    }

    fn is_completion_trigger(
        &self,
        _: &Entity<Buffer>,
        _: Anchor,
        text: &str,
        trigger_in_words: bool,
        _: &mut Context<Editor>,
    ) -> bool {
        text.ends_with('.')
            || text.ends_with("::")
            || (trigger_in_words
                && text
                    .chars()
                    .any(|character| character.is_alphanumeric() || character == '_'))
    }
}
