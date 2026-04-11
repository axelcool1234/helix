use super::*;
use std::{fs, path::PathBuf};

use helix_core::Range;
use helix_term::{commands, config::Config};
use helix_view::input::{KeyCode, KeyEvent as HxKeyEvent, KeyModifiers};
use tempfile::tempdir;
use tokio_stream::wrappers::UnboundedReceiverStream;

#[cfg(windows)]
use crossterm::event::{Event, KeyEvent as TermKeyEvent};
#[cfg(not(windows))]
use termina::event::{Event, KeyEvent as TermKeyEvent};

async fn build_lean_app() -> anyhow::Result<(tempfile::TempDir, PathBuf, Application)> {
    let temp = tempdir()?;
    let path = temp.path().join("abbr.lean");
    fs::write(&path, "")?;

    let mut config = Config::default();
    config.editor.auto_completion = false;
    config.editor.auto_info = false;
    config.editor.lsp.auto_signature_help = false;
    config.editor.lsp.display_signature_help_docs = false;

    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(&path, None)
        .build()?;

    let source_doc = app
        .editor
        .document_by_path(&path)
        .expect("source lean document should exist")
        .id();
    let source_view = app
        .editor
        .tree
        .views()
        .find_map(|(view, _)| (view.doc == source_doc).then_some(view.id))
        .expect("source lean view should exist");
    commands::suppress_auto_open_for_doc(&mut app.editor, source_doc);
    app.editor.focus(source_view);

    Ok((temp, path, app))
}

fn source_text(app: &Application, path: &PathBuf) -> String {
    app.editor
        .document_by_path(path)
        .expect("source lean document should exist")
        .text()
        .to_string()
}

async fn send_keys(app: &mut Application, keys: &[HxKeyEvent]) -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut rx_stream = UnboundedReceiverStream::new(rx);
    for key in keys {
        tx.send(Ok(Event::Key(TermKeyEvent::from(*key))))?;
    }
    app.event_loop_until_idle(&mut rx_stream).await;
    Ok(())
}

fn enter_insert_mode(app: &mut Application, path: &PathBuf) {
    app.editor.mode = helix_view::document::Mode::Insert;
    let source_doc = app
        .editor
        .document_by_path(path)
        .expect("source lean document should exist")
        .id();
    let source_view = app
        .editor
        .tree
        .views()
        .find_map(|(view, _)| (view.doc == source_doc).then_some(view.id))
        .expect("source lean view should exist");
    let doc = app.editor.document_by_path_mut(path).unwrap();
    let selection = doc
        .selection(source_view)
        .clone()
        .transform(|range| Range::new(range.to(), range.from()));
    doc.set_selection(source_view, selection);
}

fn key(ch: char) -> HxKeyEvent {
    HxKeyEvent {
        code: KeyCode::Char(ch),
        modifiers: KeyModifiers::NONE,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn space_expands_abbreviation_and_keeps_insert_mode() -> anyhow::Result<()> {
    let (_temp, path, mut app) = build_lean_app().await?;
    enter_insert_mode(&mut app, &path);
    send_keys(&mut app, &[key('\\'), key('a'), key(' ')]).await?;
    assert_eq!(source_text(&app, &path), "α ");
    assert_eq!(app.editor.mode(), helix_view::document::Mode::Insert);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn visible_chars_remain_in_buffer_before_commit() -> anyhow::Result<()> {
    let (_temp, path, mut app) = build_lean_app().await?;
    enter_insert_mode(&mut app, &path);
    send_keys(&mut app, &[key('\\'), key('a')]).await?;
    assert_eq!(source_text(&app, &path), "\\a");
    assert_eq!(app.editor.mode(), helix_view::document::Mode::Insert);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn space_keeps_suffix_after_longest_prefix_expansion() -> anyhow::Result<()> {
    let (_temp, path, mut app) = build_lean_app().await?;
    enter_insert_mode(&mut app, &path);
    send_keys(
        &mut app,
        &[
            key('\\'),
            key('a'),
            key('n'),
            key('d'),
            key('='),
            key('k'),
            key('s'),
            key('d'),
            key('l'),
            key(' '),
        ],
    )
    .await?;
    assert_eq!(source_text(&app, &path), "≙ksdl ");
    assert_eq!(app.editor.mode(), helix_view::document::Mode::Insert);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn chained_expansion_rewrites_multiple_abbreviations() -> anyhow::Result<()> {
    let (_temp, path, mut app) = build_lean_app().await?;
    enter_insert_mode(&mut app, &path);
    send_keys(&mut app, &[key('\\'), key('a'), key('\\'), key('b'), key(' ')]).await?;
    assert_eq!(source_text(&app, &path), "αβ ");
    assert_eq!(app.editor.mode(), helix_view::document::Mode::Insert);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn enter_expands_abbreviation_and_inserts_newline() -> anyhow::Result<()> {
    let (_temp, path, mut app) = build_lean_app().await?;
    enter_insert_mode(&mut app, &path);
    send_keys(
        &mut app,
        &[
            key('\\'),
            key('a'),
            HxKeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
            },
        ],
    )
    .await?;
    assert_eq!(
        source_text(&app, &path),
        format!("α{}", helix_core::NATIVE_LINE_ENDING.as_str())
    );
    assert_eq!(app.editor.mode(), helix_view::document::Mode::Insert);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn escape_expands_abbreviation_and_enters_normal_mode() -> anyhow::Result<()> {
    let (_temp, path, mut app) = build_lean_app().await?;
    enter_insert_mode(&mut app, &path);
    send_keys(
        &mut app,
        &[
            key('\\'),
            key('a'),
            HxKeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
            },
        ],
    )
    .await?;
    assert_eq!(source_text(&app, &path), "α");
    assert_eq!(app.editor.mode(), helix_view::document::Mode::Normal);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn double_backslash_stays_literal() -> anyhow::Result<()> {
    let (_temp, path, mut app) = build_lean_app().await?;
    enter_insert_mode(&mut app, &path);
    send_keys(&mut app, &[key('\\'), key('\\'), key(' ')]).await?;
    assert_eq!(source_text(&app, &path), "\\ ");
    assert_eq!(app.editor.mode(), helix_view::document::Mode::Insert);
    Ok(())
}
