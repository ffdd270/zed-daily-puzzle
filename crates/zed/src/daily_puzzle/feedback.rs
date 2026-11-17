use super::{bridge::Backend, make_editor};
use anyhow::{Result, anyhow};
use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Subscription, Task, TaskExt as _, Window,
};
use language::LanguageRegistry;
use project::Project;
use serde_json::json;
use std::sync::Arc;
use ui::{Button, Label, prelude::*};
use workspace::item::{Item, ItemEvent, SaveOptions};

pub struct FeedbackTab {
    backend: Arc<Backend>,
    session: String,
    title: String,
    editor: Entity<Editor>,
    saved: String,
    pending: bool,
    status: String,
    _subscription: Subscription,
}

impl FeedbackTab {
    pub fn new(
        backend: Arc<Backend>,
        session: String,
        title: String,
        registry: &Arc<LanguageRegistry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = make_editor("", false, registry, None, window, cx);
        let subscription = cx.subscribe(&editor, |_, _, event, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            }
        });
        Self {
            backend,
            session,
            title,
            editor,
            saved: String::new(),
            pending: false,
            status: String::new(),
            _subscription: subscription,
        }
    }

    fn save_note(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        if self.pending {
            return Task::ready(Err(anyhow!("의견을 저장 중입니다.")));
        }
        let note = self.editor.read(cx).text(cx);
        if note.trim().is_empty() {
            return Task::ready(Ok(()));
        }
        if note.chars().count() > 4000 {
            return Task::ready(Err(anyhow!("의견은 4000자까지 저장할 수 있습니다.")));
        }
        self.pending = true;
        self.status = "저장 중…".into();
        cx.notify();
        let request = self
            .backend
            .request(json!({"action":"feedback","session_id":self.session,"note":note}));
        cx.spawn(async move |tab, cx| {
            let result = request.result().await.and_then(|response| {
                if response.get("saved").and_then(|value| value.as_bool()) == Some(true) {
                    Ok(())
                } else {
                    Err(anyhow!("의견 저장을 확인할 수 없습니다."))
                }
            });
            tab.update(cx, |tab, cx| {
                tab.pending = false;
                match &result {
                    Ok(()) => {
                        tab.saved = note;
                        tab.status = "의견을 저장했습니다.".into();
                    }
                    Err(error) => tab.status = format!("저장하지 못했습니다: {error:#}"),
                }
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            })?;
            result
        })
    }
}

impl EventEmitter<ItemEvent> for FeedbackTab {}
impl Focusable for FeedbackTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}
impl Item for FeedbackTab {
    type Event = ItemEvent;
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        "문제에 대한 의견".into()
    }
    fn to_item_events(event: &ItemEvent, emit: &mut dyn FnMut(ItemEvent)) {
        emit(*event);
    }
    fn is_dirty(&self, cx: &App) -> bool {
        self.editor.read(cx).text(cx) != self.saved
    }
    fn can_save(&self, _: &App) -> bool {
        true
    }
    fn save(
        &mut self,
        _: SaveOptions,
        _: Entity<Project>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.save_note(cx)
    }
}
impl Render for FeedbackTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let length = self.editor.read(cx).text(cx).chars().count();
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .child(Label::new(self.title.clone()))
            .child(
                Label::new("헷갈린 설명이나 실행 결과에 대한 의견을 남겨 주세요.")
                    .color(Color::Muted),
            )
            .child(self.editor.clone())
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("save-feedback", "의견 저장")
                            .disabled(self.pending || length == 0 || length > 4000)
                            .on_click(cx.listener(|tab, _, _, cx| {
                                tab.save_note(cx).detach_and_log_err(cx)
                            })),
                    )
                    .child(Label::new(format!("{length}/4000 · Ctrl+S")).color(Color::Muted))
                    .child(Label::new(self.status.clone())),
            )
    }
}
