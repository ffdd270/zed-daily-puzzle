mod bridge;
mod completion;
mod feedback;
#[cfg(debug_assertions)]
mod smoke;

use bridge::Backend;
use editor::{Editor, EditorEvent, MultiBufferOffset, display_map::HighlightKey};
use gpui::{
    Action, AnyElement, AnyEntity, App, AppContext as _, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Global, HighlightStyle, IntoElement, ParentElement, Render,
    SharedString, Styled, Subscription, WeakEntity, Window, actions, div, px,
};
use language::{Buffer, LanguageRegistry};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{any::TypeId, collections::HashMap, path::PathBuf, sync::Arc};
use ui::{Button, ButtonStyle, Label, ListItem, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    item::{Item, ItemEvent},
};

actions!(
    daily_puzzle,
    [
        ToggleLibrary,
        ToggleLearning,
        ToggleOutput,
        Today,
        Next,
        RunExample,
        Submit,
        RunReproduction,
        Cancel,
        Hint,
        ShowAnswer,
        ShowHistory,
        ReloadLibrary,
        Feedback
    ]
);

struct PuzzleServices {
    backend: std::result::Result<Arc<Backend>, String>,
    sources: std::result::Result<Arc<Backend>, String>,
}
impl Global for PuzzleServices {}

pub fn init(cx: &mut App) {
    #[cfg(test)]
    if std::env::var_os("GDP_DATA").is_none() {
        return;
    }
    let root = std::env::var_os("GDP_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(3)
                .map(PathBuf::from)
                .unwrap_or_default()
        });
    let data = std::env::var_os("GDP_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("local-data"));
    cx.set_global(PuzzleServices {
        backend: Backend::start(root.clone(), data.clone()).map_err(|error| format!("{error:#}")),
        sources: Backend::start_sources(root, data).map_err(|error| format!("{error:#}")),
    });
}

pub fn install(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if workspace.panel::<PuzzlePanel<0>>(cx).is_some() {
        return;
    }
    let Some(services) = cx.try_global::<PuzzleServices>() else {
        return;
    };
    let backend = services.backend.clone();
    let sources = services.sources.clone();
    let workspace_handle = cx.entity().downgrade();
    let registry = workspace.app_state().languages.clone();
    let state =
        cx.new(|cx| PuzzleState::new(backend, sources, workspace_handle, registry, window, cx));
    workspace.add_panel(
        cx.new(|cx| PuzzlePanel::<0>::new(state.clone(), cx)),
        window,
        cx,
    );
    workspace.add_panel(
        cx.new(|cx| PuzzlePanel::<1>::new(state.clone(), cx)),
        window,
        cx,
    );
    workspace.add_panel(
        cx.new(|cx| PuzzlePanel::<2>::new(state.clone(), cx)),
        window,
        cx,
    );
    workspace.register_action(|workspace, _: &ToggleLibrary, window, cx| {
        workspace.toggle_panel_focus::<PuzzlePanel<0>>(window, cx);
    });
    workspace.register_action(|workspace, _: &ToggleLearning, window, cx| {
        workspace.toggle_panel_focus::<PuzzlePanel<1>>(window, cx);
    });
    workspace.register_action(|workspace, _: &ToggleOutput, window, cx| {
        workspace.toggle_panel_focus::<PuzzlePanel<2>>(window, cx);
    });
    macro_rules! register {
        ($action:ty, $method:ident $(, $argument:expr)*) => {{
            let state = state.clone();
            workspace.register_action(move |_, _: &$action, window, cx| {
                let state = state.clone();
                window.defer(cx, move |window, cx| {
                    state.update(cx, |state, cx| state.$method($($argument,)* window, cx));
                });
            });
        }};
    }
    register!(Today, open, json!({"action":"daily"}));
    register!(Next, next);
    register!(RunExample, execute, "run");
    register!(Submit, execute, "submit");
    register!(RunReproduction, execute, "reproduce");
    register!(Cancel, cancel);
    register!(Hint, assistance, "hint");
    register!(ShowAnswer, assistance, "answer");
    register!(ShowHistory, history);
    register!(ReloadLibrary, reload);
    register!(Feedback, feedback);
    workspace.focus_panel::<PuzzlePanel<0>>(window, cx);
    workspace.focus_panel::<PuzzlePanel<1>>(window, cx);
    state.update(cx, |state, cx| {
        state.send(
            json!({"action":"hello"}),
            false,
            window,
            cx,
            |state, value, window, cx| {
                state.stats = value;
                state.open(json!({"action":"daily"}), window, cx);
            },
        );
    });
    #[cfg(debug_assertions)]
    smoke::start(state, window, cx);
}

fn string(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn values(value: &Value, field: &str) -> Vec<Value> {
    value
        .get(field)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn difficulty_label(pack: &Value) -> &'static str {
    match string(pack, "difficulty").as_str() {
        "easy" => "쉬움",
        "medium" => "중간",
        "advanced" => "고급",
        _ => "",
    }
}

struct PuzzleState {
    backend: std::result::Result<Arc<Backend>, String>,
    sources: std::result::Result<Arc<Backend>, String>,
    workspace: WeakEntity<Workspace>,
    registry: Arc<LanguageRegistry>,
    stats: Value,
    opened: Value,
    source_info: Value,
    answer: Option<Entity<Editor>>,
    answer_tab: Option<Entity<CodeTab>>,
    problem_tab: Option<Entity<ReportTab>>,
    source_tabs: HashMap<String, WeakEntity<CodeTab>>,
    output: Entity<Editor>,
    draft: String,
    status: String,
    busy: Option<String>,
    track: String,
    topic: String,
    piece: usize,
    generation: u64,
    subscriptions: Vec<Subscription>,
}

impl PuzzleState {
    fn new(
        backend: std::result::Result<Arc<Backend>, String>,
        sources: std::result::Result<Arc<Backend>, String>,
        workspace: WeakEntity<Workspace>,
        registry: Arc<LanguageRegistry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let output = make_editor("", true, &registry, None, window, cx);
        Self {
            backend,
            sources,
            workspace,
            registry,
            stats: Value::Null,
            opened: Value::Null,
            source_info: Value::Null,
            answer: None,
            answer_tab: None,
            problem_tab: None,
            source_tabs: HashMap::new(),
            output,
            draft: String::new(),
            status: "검증된 문제를 불러오는 중…".into(),
            busy: None,
            track: "engine".into(),
            topic: String::new(),
            piece: 0,
            generation: 0,
            subscriptions: Vec::new(),
        }
    }

    fn pack(&self) -> &Value {
        self.opened.get("puzzle").unwrap_or(&Value::Null)
    }
    fn session(&self) -> String {
        self.opened
            .get("session")
            .map(|value| string(value, "id"))
            .unwrap_or_default()
    }

    fn send(
        &mut self,
        request: Value,
        source: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        complete: impl FnOnce(&mut Self, Value, &mut Window, &mut Context<Self>) + 'static,
    ) -> Option<String> {
        let backend = match if source { &self.sources } else { &self.backend } {
            Ok(backend) => backend.clone(),
            Err(error) => {
                self.status = error.clone();
                cx.notify();
                return None;
            }
        };
        let request = backend.request(request);
        let id = request.id.clone();
        let pending_id = id.clone();
        cx.spawn_in(window, async move |state, cx| {
            let response = request.result().await;
            state
                .update_in(cx, |state, window, cx| {
                    match response {
                        Ok(value) => complete(state, value, window, cx),
                        Err(error) => {
                            if state.busy.as_ref() == Some(&pending_id) {
                                state.busy = None;
                            }
                            state.status = format!("{error:#}");
                            state.output.update(cx, |editor, cx| {
                                editor.set_text(state.status.clone(), window, cx)
                            });
                        }
                    }
                    cx.notify();
                })
                .log_err();
        })
        .detach();
        Some(id)
    }

    fn next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open(
            json!({"action":"next","track":self.track,"topic":self.topic}),
            window,
            cx,
        );
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.send(
            json!({"action":"reload"}),
            false,
            window,
            cx,
            |state, value, _, _| state.stats = value,
        );
    }

    fn feedback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session().is_empty() {
            return;
        }
        if let Ok(backend) = &self.backend {
            let backend = backend.clone();
            let session = self.session();
            let title = string(self.pack(), "title");
            let registry = self.registry.clone();
            let tab = cx.new(|cx| {
                feedback::FeedbackTab::new(backend, session, title, &registry, window, cx)
            });
            self.add_item(tab, true, window, cx);
        }
    }

    fn open(&mut self, request: Value, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            self.status = "진행 중인 실행을 마치거나 취소한 뒤 문제를 바꿔 주세요.".into();
            cx.notify();
            return;
        }
        self.save(cx);
        self.generation += 1;
        let generation = self.generation;
        self.send(
            request,
            false,
            window,
            cx,
            move |state, value, window, cx| {
                if generation != state.generation {
                    return;
                }
                if value.get("empty").and_then(Value::as_bool) == Some(true) {
                    state.status = string(&value, "message");
                    return;
                }
                state.opened = value;
                state.source_info = Value::Null;
                state.piece = 0;
                let track = string(state.pack(), "track");
                if state.track != track {
                    state.topic.clear();
                }
                state.track = track;
                state.draft = state
                    .opened
                    .get("session")
                    .map(|session| string(session, "draft"))
                    .unwrap_or_default();
                if state.draft.is_empty() {
                    state.draft = string(state.pack(), "starter");
                }
                let answer = make_editor(
                    &state.draft,
                    false,
                    &state.registry,
                    Some(if state.track == "engine" {
                        "C++"
                    } else {
                        "GDScript"
                    }),
                    window,
                    cx,
                );
                if let Ok(backend) = &state.backend {
                    let provider = std::rc::Rc::new(completion::PuzzleCompletions {
                        backend: backend.clone(),
                        session: state.session(),
                        vocabulary: values(state.pack(), "vocabulary")
                            .iter()
                            .filter_map(|value| value.as_str().map(str::to_owned))
                            .collect(),
                    });
                    answer.update(cx, |editor, _| {
                        editor.set_completion_provider(Some(provider))
                    });
                }
                state.subscriptions.clear();
                state
                    .subscriptions
                    .push(cx.subscribe(&answer, |state, editor, event, cx| {
                        if matches!(event, EditorEvent::BufferEdited) {
                            state.draft = editor.read(cx).text(cx);
                            state.save(cx);
                        }
                    }));
                state.answer = Some(answer.clone());
                let title = if state.track == "engine" {
                    "답안.cpp"
                } else {
                    "답안.gd"
                };
                let answer_tab = if let Some(tab) = state.answer_tab.clone() {
                    tab.update(cx, |tab, cx| {
                        *tab = CodeTab::new(title, answer, "", cx);
                        cx.emit(ItemEvent::UpdateTab);
                        cx.notify();
                    });
                    tab
                } else {
                    cx.new(|cx| CodeTab::new(title, answer, "", cx))
                };
                state.answer_tab = Some(answer_tab.clone());
                let description = report_markdown(state.pack());
                state.add_item(answer_tab, false, window, cx);
                state.markdown_tab("문제", description, window, cx);
                state.status = "직접 코드를 작성하거나 조각을 삽입해 풀어 보세요.".into();
                let puzzle = string(state.pack(), "id");
                state.send(
                    json!({"action":"source_info","puzzle_id":puzzle}),
                    true,
                    window,
                    cx,
                    move |state, value, _, _| {
                        if state.generation == generation {
                            state.source_info = value;
                        }
                    },
                );
                state.send(
                    json!({"action":"prepare","session_id":state.session()}),
                    false,
                    window,
                    cx,
                    |_, _, _, _| {},
                );
            },
        );
    }

    fn save(&self, cx: &mut Context<Self>) {
        if let Ok(backend) = &self.backend
            && !self.session().is_empty()
        {
            let request = backend.request(
                json!({"action":"save","session_id":self.session(),"code":self.draft,"targets":[]}),
            );
            cx.spawn(async move |state, cx| {
                if let Err(error) = request.result().await {
                    state
                        .update(cx, |state, cx| {
                            state.status = format!("초안을 저장하지 못했습니다: {error:#}");
                            cx.notify();
                        })
                        .log_err();
                }
            })
            .detach();
        }
    }

    fn add_item<T: Item>(
        &self,
        item: Entity<T>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(Box::new(item), None, focus, window, cx);
            })
            .log_err();
    }

    fn markdown_tab(
        &mut self,
        title: &str,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let registry = self.registry.clone();
        let markdown = cx.new(|cx| Markdown::new(text.into(), Some(registry), None, cx));
        let tab = if title == "문제" {
            if let Some(tab) = self.problem_tab.clone() {
                tab.update(cx, |tab, cx| {
                    tab.markdown = markdown;
                    cx.notify();
                });
                tab
            } else {
                cx.new(|cx| ReportTab {
                    title: title.to_owned(),
                    markdown,
                    focus: cx.focus_handle(),
                })
            }
        } else {
            cx.new(|cx| ReportTab {
                title: title.to_owned(),
                markdown,
                focus: cx.focus_handle(),
            })
        };
        if title == "문제" {
            self.problem_tab = Some(tab.clone());
        }
        self.add_item(tab, true, window, cx);
    }

    fn show_answer_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.answer_tab.clone() {
            self.add_item(tab, true, window, cx);
        }
    }

    fn execute(&mut self, action: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() || self.session().is_empty() {
            return;
        }
        if let Some(answer) = &self.answer {
            self.draft = answer.read(cx).text(cx);
        }
        self.save(cx);
        let request =
            json!({"action":action,"session_id":self.session(),"code":self.draft,"targets":[]});
        self.status = if action == "reproduce" {
            "수정 전 엔진에서 재현 중…"
        } else {
            "코드를 실행하는 중…"
        }
        .into();
        self.output.update(cx, |editor, cx| {
            editor.set_text(self.status.clone(), window, cx)
        });
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.focus_panel::<PuzzlePanel<2>>(window, cx);
            })
            .log_err();
        if action != "reproduce" {
            self.show_answer_editor(window, cx);
        }
        self.busy = self.send(
            request,
            false,
            window,
            cx,
            move |state, value, window, cx| {
                state.busy = None;
                let text = if action == "reproduce" {
                    let observation = value.get("observation").unwrap_or(&Value::Null);
                    let expected = state
                        .pack()
                        .pointer("/issue_report/reproduction/expected_observation")
                        .unwrap_or(&Value::Null);
                    if string(&value, "status") == "observed" {
                        format!(
                            "수정 전 관측값: {observation}\n수정 후 기대값: {expected}\n\n{}",
                            string(&value, "output")
                        )
                    } else {
                        format!(
                            "{}\n\n{}",
                            string(&value, "message"),
                            string(&value, "output")
                        )
                    }
                } else {
                    let judge = value.get("judge").unwrap_or(&Value::Null);
                    let status = string(judge, "status");
                    let label = match status.as_str() {
                        "pass" if action == "submit" => "정답입니다",
                        "pass" => "공개 예제를 통과했습니다",
                        "fail" => "테스트를 통과하지 못했습니다",
                        "cancelled" => "실행을 취소했습니다",
                        _ => "실행 결과",
                    };
                    let outcome = value.get("outcome").unwrap_or(&Value::Null);
                    let rating = if let Some(delta) = outcome.get("delta").and_then(Value::as_f64) {
                        if outcome.get("rated").and_then(Value::as_bool) == Some(true) {
                            format!("\n레이팅 {delta:+.0}")
                        } else {
                            "\n연습 풀이로 기록했습니다.".to_owned()
                        }
                    } else {
                        String::new()
                    };
                    format!(
                        "{label}{rating}\n{}\n\n{}",
                        string(judge, "message"),
                        string(judge, "diagnostics")
                    )
                };
                state.status = text.lines().next().unwrap_or_default().to_owned();
                state
                    .output
                    .update(cx, |editor, cx| editor.set_text(text, window, cx));
                state.send(
                    json!({"action":"stats"}),
                    false,
                    window,
                    cx,
                    |state, value, _, _| state.stats = value,
                );
            },
        );
        cx.notify();
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(job) = &self.busy {
            self.send(
                json!({"action":"cancel","job":job}),
                false,
                window,
                cx,
                |_, _, _, _| {},
            );
        }
    }

    fn assistance(&mut self, action: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if self.session().is_empty() {
            return;
        }
        let generation = self.generation;
        self.send(
            json!({"action":action,"session_id":self.session()}),
            false,
            window,
            cx,
            move |state, value, window, cx| {
                if state.generation != generation {
                    return;
                }
                let content = if action == "hint" {
                    string(&value, "hint")
                } else {
                    format!(
                        "```{}\n{}\n```\n\n{}",
                        if string(state.pack(), "track") == "engine" {
                            "cpp"
                        } else {
                            "gdscript"
                        },
                        string(&value, "answer"),
                        string(&value, "explanation")
                    )
                };
                if let Some(session) = value.get("session") {
                    state.opened["session"] = session.clone();
                }
                state.markdown_tab(
                    if action == "hint" { "힌트" } else { "해설" },
                    content,
                    window,
                    cx,
                );
                state.status = if state
                    .opened
                    .pointer("/session/eligible")
                    .and_then(Value::as_i64)
                    == Some(0)
                {
                    "힌트·해설을 먼저 본 풀이는 연습으로 기록합니다."
                } else {
                    "힌트·해설을 열었습니다. 이미 시작했거나 확정된 제출의 평가는 유지됩니다."
                }
                .into();
            },
        );
    }

    fn history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.send(
            json!({"action":"stats"}),
            false,
            window,
            cx,
            |state, value, window, cx| {
                let text = history_markdown(&value);
                state.stats = value;
                state.markdown_tab("학습 기록", text, window, cx);
            },
        );
    }

    fn insert_piece(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(piece) = values(self.pack(), "pieces").get(self.piece).cloned() else {
            return;
        };
        let code = string(&piece, "code");
        if let Some(answer) = &self.answer {
            answer.update(cx, |editor, cx| {
                editor.finalize_last_transaction(cx);
                if replace {
                    editor.set_text(code, window, cx);
                } else {
                    editor.handle_input(&code, window, cx);
                }
                editor.finalize_last_transaction(cx);
            });
            window.focus(&answer.focus_handle(cx), cx);
        }
        self.show_answer_editor(window, cx);
    }

    fn source(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let key = format!("{}:{path}", string(self.pack(), "id"));
        if let Some(tab) = self.source_tabs.get(&key).and_then(WeakEntity::upgrade) {
            self.add_item(tab, true, window, cx);
            return;
        }
        let generation = self.generation;
        self.send(
            json!({"action":"source_read","puzzle_id":string(self.pack(),"id"),"path":path}),
            true,
            window,
            cx,
            move |state, value, window, cx| {
                if state.generation != generation {
                    return;
                }
                let text = string(&value, "text");
                let cpp = string(&value,"path").ends_with(".cpp") || string(&value,"path").ends_with(".h");
                let editor = make_editor(&text, true, &state.registry, cpp.then_some("C++"), window, cx);
                let focus = values(&value, "focus");
                let landing = value.get("landing").and_then(Value::as_u64).unwrap_or(1) as usize;
                let ranges = context_ranges(&text, &focus);
                editor.update(cx, |editor, cx| {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let anchors = ranges
                        .into_iter()
                        .map(|range| {
                            snapshot.anchor_after(MultiBufferOffset(range.start))..snapshot.anchor_before(MultiBufferOffset(range.end))
                        })
                        .collect();
                    editor.highlight_text(
                        HighlightKey::DailyPuzzleContext,
                        anchors,
                        HighlightStyle {
                            fade_out: Some(0.55),
                            ..Default::default()
                        },
                        cx,
                    );
                    editor.go_to_singleton_buffer_point(language::Point::new(landing.saturating_sub(1) as u32,0),window,cx);
                });
                let path = string(&value, "path");
                let commit = string(&value, "commit");
                let title = format!("{} · {}",path.rsplit('/').next().unwrap_or(&path),commit.chars().take(7).collect::<String>());
                let header = format!(
                    "{path}  ·  {}  ·  읽기 전용",
                    commit.chars().take(12).collect::<String>()
                );
                let tab = cx.new(|cx| CodeTab::new(&title, editor, &header, cx));
                state.source_tabs.insert(key, tab.downgrade());
                state.add_item(tab.clone(), true, window, cx);
                if !cpp { return; }
                let expected_hash=format!("{:x}",Sha256::digest(text.as_bytes()));
                let tab=tab.downgrade();
                state.send(json!({"action":"source_tokens","puzzle_id":string(state.pack(),"id"),"path":path}),true,window,cx,
                    move|_,tokens,_,cx| {
                        tab.update(cx,|tab,cx| {
                            if string(&tokens,"commit")!=commit || string(&tokens,"path")!=path || string(&tokens,"source_sha256")!=expected_hash {
                                tab.header.push_str(" · 의미 분석 대기 중");
                            } else {
                                tab.semantic=values(&tokens,"tokens");
                                tab.apply_semantics(cx);
                                tab.header.push_str(" · 원본 타입 분석 완료");
                            }
                            cx.notify();
                        }).log_err();
                    });
            },
        );
    }

    fn attachment(&self, file: Value, window: &mut Window, cx: &mut Context<Self>) {
        let path = string(&file, "path");
        let language = if path.ends_with(".gd") {
            Some("GDScript")
        } else {
            None
        };
        let editor = make_editor(
            &string(&file, "content"),
            true,
            &self.registry,
            language,
            window,
            cx,
        );
        let tab = cx.new(|cx| CodeTab::new(&path, editor, "최소 재현 프로젝트 · 읽기 전용", cx));
        self.add_item(tab, true, window, cx);
    }

    fn export_reproduction(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session().is_empty() {
            return;
        }
        self.send(
            json!({"action":"reproduction_export","session_id":self.session()}),
            false,
            window,
            cx,
            |state, value, _, cx| {
                let path = PathBuf::from(string(&value, "path"));
                cx.reveal_path(&path);
                state.status = "재현 ZIP을 내보냈습니다.".into();
            },
        );
    }
}

impl Drop for PuzzleState {
    fn drop(&mut self) {
        if let Ok(backend) = &self.backend
            && !self.session().is_empty()
        {
            drop(backend.request(
                json!({"action":"save","session_id":self.session(),"code":self.draft,"targets":[]}),
            ));
        }
        if let (Ok(backend), Some(job)) = (&self.backend, &self.busy) {
            drop(backend.request(json!({"action":"cancel","job":job})));
        }
    }
}

fn context_ranges(text: &str, focus: &[Value]) -> Vec<std::ops::Range<usize>> {
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    let mut offset = 0;
    for (row, line) in text.split_inclusive('\n').enumerate() {
        let related = focus.iter().any(|range| {
            range.get("start").and_then(Value::as_u64).unwrap_or(1) as usize <= row + 1
                && range.get("end").and_then(Value::as_u64).unwrap_or(0) as usize >= row + 1
        });
        if !related {
            if let Some(previous) = ranges.last_mut().filter(|range| range.end == offset) {
                previous.end += line.len();
            } else {
                ranges.push(offset..offset + line.len());
            }
        }
        offset += line.len();
    }
    ranges
}

fn make_editor<T: 'static>(
    text: &str,
    read_only: bool,
    registry: &Arc<LanguageRegistry>,
    language: Option<&str>,
    window: &mut Window,
    cx: &mut Context<T>,
) -> Entity<Editor> {
    let buffer = cx.new(|cx| Buffer::local(text.to_owned(), cx));
    buffer.read(cx).set_language_registry(registry.clone());
    if let Some(language) = language {
        let registry = registry.clone();
        let language = language.to_owned();
        let buffer = buffer.downgrade();
        cx.spawn(
            async move |_, cx| match registry.language_for_name(language.as_str()).await {
                Ok(language) => {
                    buffer
                        .update(cx, |buffer, cx| buffer.set_language(Some(language), cx))
                        .log_err();
                }
                Err(error) => log::warn!("Daily Puzzle syntax: {error:#}"),
            },
        )
        .detach();
    }
    cx.new(|cx| {
        let mut editor = Editor::for_buffer(buffer, None, window, cx);
        editor.set_read_only(read_only);
        editor
    })
}

fn report_markdown(pack: &Value) -> String {
    let report = pack.get("issue_report").unwrap_or(&Value::Null);
    let mut text = format!(
        "# {}\n\n{}\n\n## 현재 결과\n\n{}\n\n## 기대 동작\n\n{}\n\n## 재현 절차\n\n",
        string(pack, "title"),
        string(report, "summary"),
        string(report, "observed"),
        string(report, "expected")
    );
    for (index, step) in values(report, "steps").iter().enumerate() {
        text.push_str(&format!(
            "{}. {}\n",
            index + 1,
            step.as_str().unwrap_or_default()
        ));
    }
    text.push_str(&format!(
        "\n## 변경 범위\n\n{}\n\n",
        string(report, "scope")
    ));
    for reference in values(report, "references") {
        text.push_str(&format!(
            "[{}]({})  ",
            string(&reference, "label"),
            string(&reference, "url")
        ));
    }
    text.push_str(&format!(
        "\n\n{}\n\n## 풀어 볼 코드\n\n{}\n\n## 공개 예제\n\n```json\n{}\n```\n",
        string(report, "attribution"),
        string(pack, "statement"),
        serde_json::to_string_pretty(pack.get("samples").unwrap_or(&Value::Null))
            .unwrap_or_default()
    ));
    text
}

fn history_markdown(stats: &Value) -> String {
    let mut text = format!(
        "# 학습 기록\n\n연속 학습 {}일 · {}\n\n",
        stats["streak"],
        string(stats, "today")
    );
    for (track, label) in [("engine", "엔진 C++"), ("gdscript", "GDScript")] {
        let profile = &stats["profiles"][track];
        text.push_str(&format!(
            "**{label}** · {:.0} ± {:.0} · 평가 {}회{}\n\n",
            profile["value"].as_f64().unwrap_or(1500.),
            profile["deviation"].as_f64().unwrap_or(350.),
            profile["count"],
            if profile["provisional"].as_bool() == Some(true) {
                " · 잠정 레이팅"
            } else {
                ""
            }
        ));
    }
    let titles: HashMap<String, String> = values(stats, "puzzles")
        .iter()
        .map(|puzzle| (string(puzzle, "id"), string(puzzle, "title")))
        .collect();
    let title = |id: &str| {
        titles
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_owned())
            .replace('|', "/")
    };
    text.push_str("## 최근 풀이\n\n풀이 시간은 문제를 연 때부터 제출까지이며 비활동 시간을 포함합니다. 채점 대기 시간은 제외하며 레이팅에는 사용하지 않습니다.\n\n|문제|결과|시간|도움|평가|날짜|\n|---|---|---|---|---|---|\n");
    for row in values(stats, "history") {
        let result: Value = serde_json::from_str(&string(&row, "result")).unwrap_or(Value::Null);
        let milliseconds = result["solve_elapsed_ms"].as_u64().unwrap_or(0);
        let status = match string(&row, "status").as_str() {
            "pass" => "성공",
            "fail" => "실패",
            _ => "연습",
        };
        let rating = if result["rated"].as_bool() == Some(true) {
            format!("{:+.0}", result["delta"].as_f64().unwrap_or(0.))
        } else {
            "연습".into()
        };
        text.push_str(&format!(
            "|{}|{status}|{}분 {}초|힌트 {}{}|{rating}|{}|\n",
            title(&string(&row, "puzzle_id")),
            milliseconds / 60000,
            (milliseconds / 1000) % 60,
            row["hints"],
            if row["answer_seen"].as_i64() == Some(1) {
                " · 해설"
            } else {
                ""
            },
            string(&row, "date")
        ));
    }
    text.push_str(
        "\n## 문제별 기록\n\n|문제|성공 / 풀이|도움 받은 풀이|평균 시간|\n|---|---|---|---|\n",
    );
    for row in values(stats, "per_puzzle") {
        text.push_str(&format!(
            "|{}|{} / {}|{}|{:.0}초|\n",
            title(&string(&row, "puzzle_id")),
            row["passed"],
            row["played"],
            row["assisted"],
            row["mean_solve_ms"].as_f64().unwrap_or(0.) / 1000.
        ));
    }
    text.push_str("\n## 저장한 의견\n\n");
    for row in values(stats, "feedback") {
        text.push_str(&format!(
            "**{}**\n\n{}\n\n",
            title(&string(&row, "puzzle_id")),
            string(&row, "note")
        ));
    }
    text
}

struct CodeTab {
    title: String,
    editor: Entity<Editor>,
    header: String,
    semantic: Vec<Value>,
    _subscription: Subscription,
    _settings: Subscription,
}
impl CodeTab {
    fn new(title: &str, editor: Entity<Editor>, header: &str, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&editor, |_, _, event, cx| {
            <Editor as Item>::to_item_events(event, &mut |event| cx.emit(event))
        });
        let settings =
            cx.observe_global::<settings::SettingsStore>(|tab, cx| tab.apply_semantics(cx));
        Self {
            title: title.to_owned(),
            editor,
            header: header.to_owned(),
            semantic: Vec::new(),
            _subscription: subscription,
            _settings: settings,
        }
    }

    fn apply_semantics(&mut self, cx: &mut Context<Self>) {
        if self.semantic.is_empty() {
            return;
        }
        let syntax = cx.theme().syntax().clone();
        let kinds = [
            ("class", "type"),
            ("struct", "type"),
            ("enum", "type"),
            ("type", "type"),
            ("interface", "type"),
            ("namespace", "namespace"),
            ("function", "function"),
            ("method", "function"),
            ("property", "property"),
            ("variable", "variable"),
            ("parameter", "variable"),
            ("macro", "preproc"),
            ("enumMember", "constant"),
        ];
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let text = editor.text(cx);
            for (index, (kind, name)) in kinds.into_iter().enumerate() {
                let ranges = self
                    .semantic
                    .iter()
                    .filter(|token| string(token, "kind") == kind)
                    .filter_map(|token| {
                        let start = token.get("start")?.as_u64()? as usize;
                        let end = start.checked_add(token.get("length")?.as_u64()? as usize)?;
                        if end > text.len()
                            || !text.is_char_boundary(start)
                            || !text.is_char_boundary(end)
                        {
                            return None;
                        }
                        Some(
                            snapshot.anchor_after(MultiBufferOffset(start))
                                ..snapshot.anchor_before(MultiBufferOffset(end)),
                        )
                    })
                    .collect();
                editor.highlight_text(
                    HighlightKey::SemanticToken(index as u32),
                    ranges,
                    syntax.style_for_name(name).unwrap_or_default(),
                    cx,
                );
            }
        });
    }
}
impl EventEmitter<ItemEvent> for CodeTab {}
impl Focusable for CodeTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}
impl Item for CodeTab {
    type Event = ItemEvent;
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        self.title.clone().into()
    }
    fn to_item_events(event: &ItemEvent, emit: &mut dyn FnMut(ItemEvent)) {
        emit(*event);
    }
    fn capability(&self, cx: &App) -> language::Capability {
        if self.editor.read(cx).read_only(cx) {
            language::Capability::ReadOnly
        } else {
            language::Capability::ReadWrite
        }
    }
    fn as_searchable(
        &self,
        _: &Entity<Self>,
        cx: &App,
    ) -> Option<Box<dyn workspace::searchable::SearchableItemHandle>> {
        self.editor.read(cx).as_searchable(&self.editor, cx)
    }
    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<AnyEntity> {
        if type_id == TypeId::of::<Editor>() {
            Some(self.editor.clone().into())
        } else if type_id == TypeId::of::<Self>() {
            Some(handle.clone().into())
        } else {
            None
        }
    }
}
impl Render for CodeTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .when(!self.editor.read(cx).read_only(cx), |view| {
                view.key_context("DailyPuzzleAnswer")
            })
            .size_full()
            .when(!self.header.is_empty(), |view| {
                view.child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(Label::new(self.header.clone()).color(Color::Muted)),
                )
            })
            .child(self.editor.clone())
    }
}

struct ReportTab {
    title: String,
    markdown: Entity<Markdown>,
    focus: FocusHandle,
}
impl EventEmitter<ItemEvent> for ReportTab {}
impl Focusable for ReportTab {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Item for ReportTab {
    type Event = ItemEvent;
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        self.title.clone().into()
    }
}
impl Render for ReportTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("daily-puzzle-report")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .bg(cx.theme().colors().editor_background)
            .child(
                div()
                    .max_w(px(880.))
                    .mx_auto()
                    .text_ui(cx)
                    .child(MarkdownElement::new(
                        self.markdown.clone(),
                        MarkdownStyle::themed(MarkdownFont::Preview, window, cx),
                    )),
            )
    }
}

struct PuzzlePanel<const KIND: u8> {
    state: Entity<PuzzleState>,
    focus: FocusHandle,
    position: DockPosition,
    _subscription: Subscription,
}
impl<const KIND: u8> PuzzlePanel<KIND> {
    fn new(state: Entity<PuzzleState>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            focus: cx.focus_handle(),
            position: match KIND {
                0 => DockPosition::Left,
                1 => DockPosition::Right,
                _ => DockPosition::Bottom,
            },
            _subscription: subscription,
        }
    }
}
impl<const KIND: u8> EventEmitter<PanelEvent> for PuzzlePanel<KIND> {}
impl<const KIND: u8> Focusable for PuzzlePanel<KIND> {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl<const KIND: u8> Panel for PuzzlePanel<KIND> {
    fn persistent_name() -> &'static str {
        match KIND {
            0 => "Puzzle Library",
            1 => "Puzzle Practice",
            _ => "Puzzle Output",
        }
    }
    fn panel_key() -> &'static str {
        Self::persistent_name()
    }
    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }
    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }
    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }
    fn default_size(&self, _: &Window, _: &App) -> gpui::Pixels {
        px(if KIND == 2 { 210. } else { 280. })
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(match KIND {
            0 => IconName::Book,
            1 => IconName::Code,
            _ => IconName::Terminal,
        })
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some(match KIND {
            0 => "문제와 엔진 파일",
            1 => "풀이와 조각",
            _ => "실행 결과",
        })
    }
    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        match KIND {
            0 => Box::new(ToggleLibrary),
            1 => Box::new(ToggleLearning),
            _ => Box::new(ToggleOutput),
        }
    }
    fn starts_open(&self, _: &Window, _: &App) -> bool {
        KIND != 2
    }
    fn activation_priority(&self) -> u32 {
        100
    }
}
impl<const KIND: u8> Render for PuzzlePanel<KIND> {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let mut content = Vec::<AnyElement>::new();
        if KIND == 0 {
            content.push(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("daily", "오늘").on_click(|_, window, cx| {
                            window.dispatch_action(Today.boxed_clone(), cx)
                        }),
                    )
                    .child(
                        Button::new("next", "다음 문제").on_click(|_, window, cx| {
                            window.dispatch_action(Next.boxed_clone(), cx)
                        }),
                    )
                    .into_any_element(),
            );
            content.push(
                h_flex()
                    .gap_1()
                    .children(
                        [("engine", "엔진 C++"), ("gdscript", "GDScript")]
                            .into_iter()
                            .map(|(track, label)| {
                                let model = self.state.clone();
                                Button::new(track, label)
                                    .toggle_state(state.track == track)
                                    .on_click(move |_, _, cx| {
                                        model.update(cx, |state, cx| {
                                            state.track = track.into();
                                            state.topic.clear();
                                            cx.notify();
                                        })
                                    })
                            }),
                    )
                    .into_any_element(),
            );
            let topics: std::collections::BTreeSet<String> = values(&state.stats, "puzzles")
                .into_iter()
                .filter(|puzzle| string(puzzle, "track") == state.track)
                .flat_map(|puzzle| values(&puzzle, "topics"))
                .filter_map(|topic| topic.as_str().map(str::to_owned))
                .collect();
            let topic_label = if state.topic.is_empty() {
                "모든 분야".to_owned()
            } else {
                state.topic.trim_start_matches("topic:").to_owned()
            };
            let model = self.state.clone();
            content.push(
                ui::PopoverMenu::new("puzzle-topics")
                    .trigger(Button::new("puzzle-topic-button", topic_label))
                    .menu(move |window, cx| {
                        let model = model.clone();
                        let topics = topics.clone();
                        Some(ui::ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for topic in std::iter::once(String::new()).chain(topics) {
                                let model = model.clone();
                                let label = if topic.is_empty() {
                                    "모든 분야".to_owned()
                                } else {
                                    topic.trim_start_matches("topic:").to_owned()
                                };
                                menu = menu.entry(label, None, move |_, cx| {
                                    model.update(cx, |state, cx| {
                                        state.topic = topic.clone();
                                        cx.notify();
                                    })
                                });
                            }
                            menu
                        }))
                    })
                    .into_any_element(),
            );
            let mut puzzle_rows = Vec::new();
            for (index, puzzle) in values(&state.stats, "puzzles")
                .into_iter()
                .filter(|puzzle| string(puzzle, "track") == state.track)
                .filter(|puzzle| {
                    state.topic.is_empty()
                        || values(puzzle, "topics")
                            .iter()
                            .any(|topic| topic.as_str() == Some(state.topic.as_str()))
                })
                .enumerate()
            {
                let model = self.state.clone();
                let id = string(&puzzle, "id");
                let selected = id == string(state.pack(), "id");
                puzzle_rows.push(
                    ListItem::new(index)
                        .toggle_state(selected)
                        .tooltip(Tooltip::text(format!(
                            "{} · {}",
                            string(&puzzle, "title"),
                            difficulty_label(&puzzle)
                        )))
                        .child(Label::new(string(&puzzle, "title")))
                        .on_click(move |_, window, cx| {
                            model.update(cx, |state, cx| {
                                state.open(json!({"action":"open","puzzle_id":id}), window, cx)
                            })
                        })
                        .into_any_element(),
                );
            }
            content.push(
                Label::new("엔진 원본")
                    .color(Color::Muted)
                    .into_any_element(),
            );
            for (index, file) in values(&state.source_info, "files").into_iter().enumerate() {
                let model = self.state.clone();
                let path = string(&file, "path");
                let name = path.clone();
                content.push(
                    ListItem::new(("source", index))
                        .child(Label::new(name))
                        .on_click(move |_, window, cx| {
                            model.update(cx, |state, cx| state.source(path.clone(), window, cx))
                        })
                        .into_any_element(),
                );
            }
            content.push(
                Label::new("최소 재현")
                    .color(Color::Muted)
                    .into_any_element(),
            );
            let reproduction = state
                .pack()
                .pointer("/issue_report/reproduction")
                .unwrap_or(&Value::Null);
            for (index, file) in values(reproduction, "files").into_iter().enumerate() {
                let model = self.state.clone();
                content.push(
                    ListItem::new(("attachment", index))
                        .child(Label::new(string(&file, "path")))
                        .on_click(move |_, window, cx| {
                            model.update(cx, |state, cx| state.attachment(file.clone(), window, cx))
                        })
                        .into_any_element(),
                );
            }
            content.push(
                Button::new("reproduce", "수정 전 엔진 실행")
                    .disabled(state.busy.is_some())
                    .on_click(|_, window, cx| {
                        window.dispatch_action(RunReproduction.boxed_clone(), cx)
                    })
                    .into_any_element(),
            );
            content.push(
                Button::new("export-reproduction", "ZIP 내보내기")
                    .on_click({
                        let model = self.state.clone();
                        move |_, window, cx| {
                            model.update(cx, |state, cx| state.export_reproduction(window, cx))
                        }
                    })
                    .into_any_element(),
            );
            content.push(
                Button::new("history", "학습 기록")
                    .on_click(|_, window, cx| window.dispatch_action(ShowHistory.boxed_clone(), cx))
                    .into_any_element(),
            );
            content.push(
                Label::new("문제 목록")
                    .color(Color::Muted)
                    .into_any_element(),
            );
            content.push(
                h_flex()
                    .gap_1()
                    .child(Button::new("reload", "새로고침").on_click(|_, window, cx| {
                        window.dispatch_action(ReloadLibrary.boxed_clone(), cx)
                    }))
                    .child(
                        Button::new("author", "문제 보충").on_click(|_, window, cx| {
                            window.dispatch_action(
                                tasks_ui::Spawn::ByName {
                                    task_name: "문제 보충".into(),
                                    reveal_target: None,
                                }
                                .boxed_clone(),
                                cx,
                            )
                        }),
                    )
                    .into_any_element(),
            );
            content.extend(puzzle_rows);
        } else if KIND == 1 {
            content.push(Label::new("풀이").into_any_element());
            content.push(
                Label::new(difficulty_label(state.pack()))
                    .color(Color::Muted)
                    .into_any_element(),
            );
            let current_track = string(state.pack(), "track");
            if let Some(profile) = state
                .stats
                .get("profiles")
                .and_then(|profiles| profiles.get(&current_track))
            {
                let rating = profile
                    .get("value")
                    .and_then(Value::as_f64)
                    .unwrap_or(1500.);
                let provisional = profile.get("provisional").and_then(Value::as_bool) == Some(true);
                content.push(
                    Label::new(format!(
                        "{} · {rating:.0}{}",
                        if current_track == "engine" {
                            "엔진 C++"
                        } else {
                            "GDScript"
                        },
                        if provisional { " · 추정 중" } else { "" }
                    ))
                    .color(Color::Muted)
                    .into_any_element(),
                );
            }
            content.push(
                Button::new("answer-editor", "답안 열기")
                    .on_click({
                        let model = self.state.clone();
                        move |_, window, cx| {
                            model.update(cx, |state, cx| state.show_answer_editor(window, cx))
                        }
                    })
                    .into_any_element(),
            );
            content.push(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("run", "예제 실행 · F5")
                            .disabled(state.busy.is_some())
                            .on_click(|_, window, cx| {
                                window.dispatch_action(RunExample.boxed_clone(), cx)
                            }),
                    )
                    .child(
                        Button::new("submit", "제출")
                            .style(ButtonStyle::Filled)
                            .disabled(state.busy.is_some())
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Submit.boxed_clone(), cx)
                            }),
                    )
                    .into_any_element(),
            );
            if state.busy.is_some() {
                content.push(
                    Button::new("cancel", "실행 취소")
                        .on_click(|_, window, cx| window.dispatch_action(Cancel.boxed_clone(), cx))
                        .into_any_element(),
                );
            }
            content.push(
                Label::new("코드 조각")
                    .color(Color::Muted)
                    .into_any_element(),
            );
            for (index, piece) in values(state.pack(), "pieces").into_iter().enumerate() {
                let model = self.state.clone();
                let label = string(&piece, "label");
                content.push(
                    ListItem::new(index)
                        .toggle_state(state.piece == index)
                        .child(Label::new(label))
                        .on_click(move |_, window, cx| {
                            model.update(cx, |state, cx| {
                                state.piece = index;
                                let editor = make_editor(
                                    &string(&piece, "code"),
                                    true,
                                    &state.registry,
                                    Some(if string(state.pack(), "track") == "engine" {
                                        "C++"
                                    } else {
                                        "GDScript"
                                    }),
                                    window,
                                    cx,
                                );
                                let tab = cx.new(|cx| {
                                    CodeTab::new(
                                        "코드 조각",
                                        editor,
                                        "선택한 조각 · 삽입 후 Ctrl+Z로 되돌릴 수 있습니다.",
                                        cx,
                                    )
                                });
                                state.add_item(tab, true, window, cx);
                                cx.notify();
                            })
                        })
                        .into_any_element(),
                );
            }
            content.push(
                Button::new("insert", "선택 영역에 삽입")
                    .on_click({
                        let model = self.state.clone();
                        move |_, window, cx| {
                            model.update(cx, |state, cx| state.insert_piece(false, window, cx))
                        }
                    })
                    .into_any_element(),
            );
            content.push(
                Button::new("replace", "답안 전체 교체")
                    .on_click({
                        let model = self.state.clone();
                        move |_, window, cx| {
                            model.update(cx, |state, cx| state.insert_piece(true, window, cx))
                        }
                    })
                    .into_any_element(),
            );
            content.push(
                Button::new("hint", "힌트 보기")
                    .on_click(|_, window, cx| window.dispatch_action(Hint.boxed_clone(), cx))
                    .into_any_element(),
            );
            content.push(
                Button::new("explanation", "해설 보기")
                    .on_click(|_, window, cx| window.dispatch_action(ShowAnswer.boxed_clone(), cx))
                    .into_any_element(),
            );
            content.push(
                Button::new("feedback", "문제에 대한 의견")
                    .on_click(|_, window, cx| window.dispatch_action(Feedback.boxed_clone(), cx))
                    .into_any_element(),
            );
            content.push(
                Label::new("풀이 문맥")
                    .color(Color::Muted)
                    .into_any_element(),
            );
            for (index, context) in values(state.pack(), "contexts").into_iter().enumerate() {
                let model = self.state.clone();
                content.push(
                    ListItem::new(("context", index))
                        .child(Label::new(string(&context, "label")))
                        .on_click(move |_, window, cx| {
                            model.update(cx, |state, cx| {
                                let editor = make_editor(
                                    &string(&context, "code"),
                                    true,
                                    &state.registry,
                                    Some(if string(state.pack(), "track") == "engine" {
                                        "C++"
                                    } else {
                                        "GDScript"
                                    }),
                                    window,
                                    cx,
                                );
                                let title = string(&context, "label");
                                let tab = cx.new(|cx| {
                                    CodeTab::new(&title, editor, "풀이용 문맥 · 읽기 전용", cx)
                                });
                                state.add_item(tab, true, window, cx);
                            })
                        })
                        .into_any_element(),
                );
            }
        } else {
            return v_flex()
                .track_focus(&self.focus)
                .size_full()
                .child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .child(Label::new(state.status.clone())),
                )
                .child(state.output.clone())
                .into_any_element();
        }
        v_flex()
            .id(("daily-puzzle-panel", KIND as usize))
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .p_2()
            .gap_2()
            .bg(cx.theme().colors().panel_background)
            .children(content)
            .child(Label::new(state.status.clone()).color(Color::Muted))
            .into_any_element()
    }
}
