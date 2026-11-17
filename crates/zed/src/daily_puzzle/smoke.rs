use super::{PuzzleState, string, values};
use anyhow::{Context as _, Result, anyhow, ensure};
use gpui::{AsyncWindowContext, Context, Entity, Focusable, TaskExt as _, Window};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use workspace::Workspace;

pub fn start(state: Entity<PuzzleState>, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(plan_path) = std::env::var_os("GDP_SMOKE_PLAN").map(PathBuf::from) else {
        return;
    };
    let Some(data) = std::env::var_os("GDP_DATA").map(PathBuf::from) else {
        return;
    };
    let Some(root) = std::env::var_os("GDP_ROOT").map(PathBuf::from) else {
        return;
    };
    let Ok(data) = data.canonicalize() else {
        return;
    };
    let Ok(test_root) = root.join("local-data/t").canonicalize() else {
        return;
    };
    let Ok(plan_path) = plan_path.canonicalize() else {
        return;
    };
    // Debug scenarios may only modify disposable learning data, never the user's database.
    if !data.starts_with(&test_root) || plan_path != data.join("ui-smoke-plan.json") {
        log::error!("Daily Puzzle smoke plan must live in isolated local-data/t data");
        return;
    }
    cx.spawn_in(window, async move |_, cx| {
        let mut results = Vec::new();
        let result = async {
            let plan: Value = serde_json::from_slice(&std::fs::read(plan_path)?)?;
            wait_for(&state, cx, |state, _| !state.session().is_empty()).await?;
            for step in values(&plan, "steps") {
                perform(&state, &step, cx)
                    .await
                    .with_context(|| format!("UI step: {step}"))?;
                let evidence = state.read_with(cx, |state, cx| {
                    let op = string(&step, "op");
                    let mut evidence =
                        json!({"op":op,"puzzle_id":string(state.pack(),"id"),"passed":true});
                    if ["reproduce", "run", "grade", "cancel"].contains(&op.as_str()) {
                        evidence["output"] = json!(state.output.read(cx).text(cx));
                    }
                    if op == "source" {
                        evidence["path"] = step["path"].clone();
                        evidence["sha256"] = step["sha256"].clone();
                    }
                    evidence
                });
                results.push(evidence);
                std::fs::write(
                    data.join("ui-smoke-results.json"),
                    serde_json::to_vec_pretty(
                        &json!({"passed":false,"running":true,"steps":results}),
                    )?,
                )?;
            }
            anyhow::Ok(())
        }
        .await;
        let error = result.as_ref().err().map(|error| format!("{error:#}"));
        std::fs::write(
            data.join("ui-smoke-results.json"),
            serde_json::to_vec_pretty(
                &json!({"passed":result.is_ok(),"running":false,"error":error,"steps":results}),
            )?,
        )?;
        result
    })
    .detach_and_log_err(cx);
}

async fn wait_for(
    state: &Entity<PuzzleState>,
    cx: &mut AsyncWindowContext,
    predicate: impl Fn(&PuzzleState, &gpui::App) -> bool,
) -> Result<()> {
    let started = Instant::now();
    loop {
        if state.read_with(cx, |state, cx| predicate(state, cx)) {
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(45) {
            let status = state.read_with(cx, |state, _| state.status.clone());
            return Err(anyhow!("UI state timed out: {status}"));
        }
        cx.background_executor()
            .timer(Duration::from_millis(75))
            .await;
    }
}

async fn perform(
    state: &Entity<PuzzleState>,
    step: &Value,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    match string(step, "op").as_str() {
        "open" => {
            let puzzle_id = string(step, "puzzle_id");
            state.update_in(cx, |state, window, cx| {
                state.open(json!({"action":"open","puzzle_id":puzzle_id}), window, cx)
            })?;
            wait_for(state, cx, |state, _| {
                string(state.pack(), "id") == puzzle_id && !state.source_info.is_null()
            })
            .await?;
        }
        "edit" => {
            state.update_in(cx, |state, window, cx| -> Result<()> {
                let editor = state.answer.clone().context("No answer editor")?;
                state.show_answer_editor(window, cx);
                editor.update(cx, |editor, cx| {
                    editor.finalize_last_transaction(cx);
                    editor.set_text(string(step, "code"), window, cx);
                    editor.finalize_last_transaction(cx);
                });
                Ok(())
            })??;
            wait_for(state, cx, |state, _| state.draft == string(step, "code")).await?;
        }
        "piece-undo" => {
            let before = state
                .read_with(cx, |state, cx| {
                    state.answer.as_ref().map(|editor| editor.read(cx).text(cx))
                })
                .context("No answer editor")?;
            state.update_in(cx, |state, window, cx| state.insert_piece(true, window, cx))?;
            state.update_in(cx, |state, window, cx| -> Result<()> {
                let editor = state.answer.as_ref().context("No answer editor")?;
                ensure!(
                    editor.read(cx).text(cx) != before,
                    "Piece did not replace the answer"
                );
                editor.update(cx, |editor, cx| {
                    editor.undo(&editor::actions::Undo, window, cx)
                });
                ensure!(
                    editor.read(cx).text(cx) == before,
                    "Piece undo changed earlier edits"
                );
                Ok(())
            })??;
        }
        "reproduce" | "grade" | "run" => {
            let operation = string(step, "op");
            let action = if operation == "reproduce" {
                "reproduce"
            } else if operation == "run" {
                "run"
            } else {
                "submit"
            };
            state.update_in(cx, |state, window, cx| state.execute(action, window, cx))?;
            wait_for(state, cx, |state, _| state.busy.is_none()).await?;
            state.read_with(cx, |state, cx| -> Result<()> {
                let text = state.output.read(cx).text(cx);
                let expected = if operation == "reproduce" {
                    "수정 전 관측값:"
                } else if operation == "run" {
                    "공개 예제를 통과했습니다"
                } else {
                    "정답입니다"
                };
                ensure!(text.starts_with(expected), "Unexpected output: {text}");
                for error in [
                    "No renderers available.",
                    "Falling back to dummy PhysicsServer",
                    "Could not load global script cache.",
                ] {
                    ensure!(
                        !text.contains(error),
                        "Reproduction initialization error: {text}"
                    );
                }
                Ok(())
            })?;
        }
        "source" => {
            let path = string(step, "path");
            let key = state.read_with(cx, |state, _| {
                format!("{}:{path}", string(state.pack(), "id"))
            });
            state.update_in(cx, |state, window, cx| state.source(path, window, cx))?;
            wait_for(state, cx, |state, cx| {
                state
                    .source_tabs
                    .get(&key)
                    .and_then(|tab| tab.upgrade())
                    .is_some_and(|tab| {
                        step["semantic"].as_bool() != Some(true)
                            || tab.read(cx).header.contains("원본 타입 분석 완료")
                    })
            })
            .await?;
            state.read_with(cx, |state, cx| -> Result<()> {
                let tab = state
                    .source_tabs
                    .get(&key)
                    .and_then(|tab| tab.upgrade())
                    .context("Source tab missing")?;
                let tab = tab.read(cx);
                ensure!(tab.editor.read(cx).read_only(cx), "Source is editable");
                let text = tab.editor.read(cx).text(cx);
                use sha2::{Digest, Sha256};
                ensure!(
                    format!("{:x}", Sha256::digest(text.as_bytes())) == string(step, "sha256"),
                    "Source differs from historical commit"
                );
                ensure!(
                    !super::context_ranges(&text, &values(step, "focus")).is_empty(),
                    "No dimmed source context"
                );
                Ok(())
            })?;
        }
        "completion" => {
            state.update_in(cx, |state, window, cx| -> Result<()> {
                let editor = state.answer.clone().context("No answer editor")?;
                state.show_answer_editor(window, cx);
                window.focus(&editor.focus_handle(cx), cx);
                editor.update(cx, |editor, cx| {
                    editor.go_to_singleton_buffer_point(
                        language::Point::new(
                            step["row"].as_u64().unwrap_or(0) as u32,
                            step["column"].as_u64().unwrap_or(0) as u32,
                        ),
                        window,
                        cx,
                    );
                    editor.show_completions(&editor::actions::ShowCompletions, window, cx);
                });
                Ok(())
            })??;
            wait_for(state, cx, |state, cx| {
                state
                    .answer
                    .as_ref()
                    .is_some_and(|editor| editor.read(cx).has_visible_completions_menu())
            })
            .await?;
        }
        "history" => {
            state.update_in(cx, |state, window, cx| state.history(window, cx))?;
        }
        "cancel" => {
            state.update_in(cx, |state, window, cx| -> Result<()> {
                state.execute("run", window, cx);
                ensure!(state.busy.is_some(), "Execution did not start");
                let editor = state.answer.as_ref().context("No answer editor")?;
                editor.update(cx, |editor, cx| {
                    editor.set_text(
                        format!("{}\n// Editing remains available", string(step, "code")),
                        window,
                        cx,
                    )
                });
                state.cancel(window, cx);
                Ok(())
            })??;
            wait_for(state, cx, |state, _| state.busy.is_none()).await?;
            state.read_with(cx, |state, cx| -> Result<()> {
                ensure!(
                    state.output.read(cx).text(cx).contains("취소"),
                    "Cancellation did not reach the judge"
                );
                ensure!(
                    state
                        .answer
                        .as_ref()
                        .context("No answer editor")?
                        .read(cx)
                        .text(cx)
                        .contains("Editing remains available"),
                    "Execution lost concurrent edits"
                );
                Ok(())
            })?;
        }
        "task" => {
            state.update_in(cx, |_, window, cx| {
                use gpui::Action as _;
                window.dispatch_action(
                    tasks_ui::Spawn::ByName {
                        task_name: "문제 보충".into(),
                        reveal_target: None,
                    }
                    .boxed_clone(),
                    cx,
                );
            })?;
            let data = std::env::var_os("GDP_DATA")
                .map(PathBuf::from)
                .context("Missing test data")?;
            wait_for(state, cx, |_, _| {
                data.join("native-task-probe.json").is_file()
            })
            .await?;
        }
        operation => return Err(anyhow!("Unknown UI smoke operation: {operation}")),
    }
    Ok(())
}
