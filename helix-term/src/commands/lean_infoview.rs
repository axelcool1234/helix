use std::sync::{Mutex, MutexGuard};

use anyhow::Context as _;
use helix_core::{diff::compare_ropes, Rope};
use helix_lsp::{
    lsp::{self, request::Request, DiagnosticSeverity, NumberOrString, Position, Range as LspRange, TextDocumentIdentifier},
    LanguageServerId,
};
use helix_core::Uri;
use helix_view::{editor::Action, DocumentId, Editor, ViewId};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

use crate::job::Callback;
use crate::job;

use super::{lsp as lsp_commands, Context};

const INFOVIEW_TITLE: &str = "Lean Infoview";
const GOAL_QUERY_OFFSETS: [i32; 5] = [0, -1, 1, -2, 2];
const GOALS_ACCOMPLISHED_TEXT: &str = "✓ Goals accomplished.";

static STATE: Lazy<Mutex<State>> = Lazy::new(|| Mutex::new(State::default()));

#[derive(Default)]
struct State {
    infoview_doc: Option<DocumentId>,
    infoview_view: Option<ViewId>,
    last_source_doc: Option<DocumentId>,
    last_source_view: Option<ViewId>,
    last_source_server: Option<LanguageServerId>,
    last_source_uri: Option<Url>,
    last_source_position: Option<lsp::Position>,
    rpc_session: Option<RpcSession>,
    spans: Vec<InfoSpan>,
    request_seq: u64,
    auto_open_suppressed_doc: Option<DocumentId>,
}

#[derive(Clone)]
struct RpcSession {
    server_id: LanguageServerId,
    uri: Url,
    session_id: String,
}

#[derive(Clone, Debug)]
struct InfoSpan {
    start: usize,
    end: usize,
    info: Value,
}

#[derive(Clone)]
struct SourceContext {
    doc_id: Option<DocumentId>,
    view_id: Option<ViewId>,
    server_id: LanguageServerId,
    uri: Url,
    position: lsp::Position,
    text_document: TextDocumentIdentifier,
}

#[derive(Clone)]
struct RefreshPayload {
    request_seq: u64,
    source_doc: Option<DocumentId>,
    source_view: Option<ViewId>,
    source_server: Option<LanguageServerId>,
    source_uri: Option<Url>,
    source_position: Option<lsp::Position>,
    session: Option<RpcSession>,
    text: String,
    spans: Vec<InfoSpan>,
}

#[derive(Clone)]
struct RenderedSegment {
    text: String,
    info: Option<Value>,
}

type RenderedLine = Vec<RenderedSegment>;

#[derive(Default)]
struct BufferBuilder {
    text: String,
    spans: Vec<InfoSpan>,
    char_len: usize,
}

#[derive(Clone, Deserialize, Serialize)]
struct PlainGoalResult {
    #[serde(default)]
    goals: Vec<String>,
    #[serde(default)]
    rendered: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct PlainTermGoalResult {
    #[serde(default)]
    goal: Option<String>,
    #[serde(default)]
    range: Option<LspRange>,
}

#[derive(Clone, Deserialize, Serialize, Default)]
struct InteractiveGoalsResult {
    #[serde(default)]
    goals: Vec<InteractiveGoal>,
}

#[derive(Clone, Deserialize, Serialize, Default)]
struct InteractiveGoal {
    #[serde(default, rename = "goalPrefix")]
    goal_prefix: Option<String>,
    #[serde(default, rename = "userName")]
    user_name: Option<String>,
    #[serde(default)]
    hyps: Vec<InteractiveHypothesis>,
    #[serde(default, rename = "type")]
    r#type: Value,
}

#[derive(Clone, Deserialize, Serialize, Default)]
struct InteractiveHypothesis {
    #[serde(default)]
    names: Vec<String>,
    #[serde(default, rename = "type")]
    r#type: Value,
    #[serde(default)]
    val: Option<Value>,
}

#[derive(Clone, Deserialize, Serialize)]
struct RpcConnectParams {
    uri: Url,
}

#[derive(Clone, Deserialize, Serialize)]
struct RpcConnectResult {
    #[serde(rename = "sessionId")]
    session_id: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct RpcCallParams {
    #[serde(rename = "textDocument")]
    text_document: TextDocumentIdentifier,
    position: Position,
    #[serde(rename = "sessionId")]
    session_id: String,
    method: String,
    params: Value,
}

#[derive(Clone, Deserialize, Serialize, Default)]
struct LeanGotoLocation {
    #[serde(default, rename = "targetUri")]
    target_uri: Option<Url>,
    #[serde(default)]
    uri: Option<Url>,
    #[serde(default, rename = "targetSelectionRange")]
    target_selection_range: Option<LspRange>,
    #[serde(default, rename = "targetRange")]
    target_range: Option<LspRange>,
    #[serde(default)]
    range: Option<LspRange>,
}

struct LeanPlainGoalRequest;
struct LeanPlainTermGoalRequest;
struct LeanRpcConnectRequest;
struct LeanRpcCallRequest;

impl Request for LeanPlainGoalRequest {
    type Params = lsp::TextDocumentPositionParams;
    type Result = PlainGoalResult;
    const METHOD: &'static str = "$/lean/plainGoal";
}

impl Request for LeanPlainTermGoalRequest {
    type Params = lsp::TextDocumentPositionParams;
    type Result = PlainTermGoalResult;
    const METHOD: &'static str = "$/lean/plainTermGoal";
}

impl Request for LeanRpcConnectRequest {
    type Params = RpcConnectParams;
    type Result = RpcConnectResult;
    const METHOD: &'static str = "$/lean/rpc/connect";
}

impl Request for LeanRpcCallRequest {
    type Params = RpcCallParams;
    type Result = Value;
    const METHOD: &'static str = "$/lean/rpc/call";
}

fn lock_state() -> MutexGuard<'static, State> {
    STATE.lock().expect("lean infoview state poisoned")
}

fn sync_state(editor: &Editor, state: &mut State) {
    let valid = match (state.infoview_doc, state.infoview_view) {
        (Some(doc_id), Some(view_id)) => {
            editor.documents.contains_key(&doc_id)
                && matches!(editor.tree.try_get(view_id), Some(view) if view.doc == doc_id)
        }
        _ => false,
    };

    if !valid {
        state.infoview_doc = None;
        state.infoview_view = None;
        state.spans.clear();
    }

    let source_valid = match (state.last_source_doc, state.last_source_view) {
        (Some(doc_id), Some(view_id)) => {
            editor.documents.contains_key(&doc_id) && editor.tree.try_get(view_id).is_some()
        }
        _ => true,
    };

    if !source_valid {
        state.last_source_doc = None;
        state.last_source_view = None;
    }
}

fn is_infoview_doc(editor: &Editor) -> bool {
    let mut state = lock_state();
    sync_state(editor, &mut state);
    let Some(doc_id) = state.infoview_doc else {
        return false;
    };
    current_ref!(editor).1.id() == doc_id
}

fn find_lean_language_server<'a>(doc: &'a helix_view::Document) -> Option<&'a helix_lsp::Client> {
    doc.language_servers()
        .find(|server| server.name() == "lean")
        .or_else(|| doc.language_servers().next())
}

fn current_source_context(editor: &Editor, prefer_stored: bool) -> Option<SourceContext> {
    let mut state = lock_state();
    sync_state(editor, &mut state);

    let current_view = editor.tree.get(editor.tree.focus);
    let current_doc = editor.documents.get(&current_view.doc)?;
    let current_is_infoview = state.infoview_doc == Some(current_doc.id());

    let (view_id, doc_id) = if !current_is_infoview {
        (Some(current_view.id), Some(current_doc.id()))
    } else if prefer_stored {
        let server_id = state.last_source_server?;
        let uri = state.last_source_uri.clone()?;
        let position = state.last_source_position?;

        return Some(SourceContext {
            doc_id: state.last_source_doc,
            view_id: state.last_source_view,
            server_id,
            uri: uri.clone(),
            position,
            text_document: TextDocumentIdentifier::new(uri),
        });
    } else {
        return None;
    };

    let view_id = view_id?;
    let doc_id = doc_id?;
    editor.tree.try_get(view_id)?;
    let doc = editor.documents.get(&doc_id)?;
    if doc.language_name() != Some("lean") || doc.path().is_none() {
        return None;
    }

    let language_server = find_lean_language_server(doc)?;
    let offset_encoding = language_server.offset_encoding();
    let uri = doc.url()?;
    let text_document = doc.identifier();
    let position = doc.position(view_id, offset_encoding);

    Some(SourceContext {
        doc_id: Some(doc_id),
        view_id: Some(view_id),
        server_id: language_server.id(),
        uri,
        position,
        text_document,
    })
}

fn ensure_infoview_open(editor: &mut Editor, focus: bool) -> Option<(DocumentId, ViewId)> {
    {
        let mut state = lock_state();
        sync_state(editor, &mut state);

        if let (Some(doc_id), Some(view_id)) = (state.infoview_doc, state.infoview_view) {
            drop(state);
            if focus {
                editor.focus(view_id);
            }
            return Some((doc_id, view_id));
        }
    }

    let previous_focus = editor.tree.focus;
    let doc_id = editor.new_file(Action::VerticalSplit);
    let view_id = editor.tree.focus;
    {
        let doc = editor.documents.get_mut(&doc_id)?;
        doc.set_path(None);
        doc.set_scratch_title(Some(INFOVIEW_TITLE.to_string()));
        doc.readonly = true;
    }

    {
        let mut state = lock_state();
        state.infoview_doc = Some(doc_id);
        state.infoview_view = Some(view_id);
        state.spans.clear();
        state.auto_open_suppressed_doc = None;
    }
    editor.tree.set_two_way_split_fraction(view_id, 2, 5);
    log::debug!(
        "lean infoview opened doc={:?} view={:?} focus={focus}",
        doc_id,
        view_id
    );

    if focus {
        editor.focus(view_id);
    } else if editor.tree.try_get(previous_focus).is_some() {
        editor.focus(previous_focus);
    } else if let Some(target_view) = goto_target_view(editor, None) {
        editor.focus(target_view);
    }

    Some((doc_id, view_id))
}

pub(crate) fn maybe_auto_open(editor: &mut Editor) {
    if is_infoview_doc(editor) {
        return;
    }

    let (visible, suppressed_doc) = {
        let mut state = lock_state();
        sync_state(editor, &mut state);
        (state.infoview_doc.is_some(), state.auto_open_suppressed_doc)
    };
    if visible {
        return;
    }

    let current_doc = editor.tree.try_get(editor.tree.focus).map(|view| view.doc);
    if current_doc.is_some() && current_doc == suppressed_doc {
        return;
    }

    let should_open = editor
        .tree
        .try_get(editor.tree.focus)
        .and_then(|view| editor.documents.get(&view.doc))
        .is_some_and(|doc| doc.language_name() == Some("lean") && doc.path().is_some());
    if !should_open {
        return;
    }

    let _ = ensure_infoview_open(editor, false);
}

fn close_infoview(editor: &mut Editor) {
    let doc_id = {
        let mut state = lock_state();
        sync_state(editor, &mut state);
        let doc_id = state.infoview_doc;
        state.infoview_doc = None;
        state.infoview_view = None;
        state.spans.clear();
        doc_id
    };

    if let Some(doc_id) = doc_id {
        log::debug!("lean infoview closed doc={:?}", doc_id);
        let _ = editor.close_document(doc_id, true);
    }
}

#[cfg(feature = "integration")]
pub fn suppress_auto_open_for_doc(editor: &mut Editor, doc_id: DocumentId) {
    {
        let mut state = lock_state();
        state.auto_open_suppressed_doc = Some(doc_id);
    }
    close_infoview(editor);
}

fn plain_text_lines(text: &str) -> Vec<RenderedLine> {
    let mut lines = Vec::new();
    let raw_lines: Vec<&str> = text.split('\n').collect();
    let mut idx = 0;
    while idx < raw_lines.len() {
        let line = raw_lines[idx];
        let next = raw_lines.get(idx + 1).copied().unwrap_or_default();
        if !(line.is_empty() && (next.starts_with('⊢') || next.starts_with("|-"))) {
            lines.push(vec![RenderedSegment {
                text: line.to_string(),
                info: None,
            }]);
        }
        idx += 1;
    }

    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

fn plain_text_has_visible_content(text: &str) -> bool {
    plain_text_lines(text).into_iter().any(|line| {
        line.into_iter()
            .any(|segment| !segment.text.trim().is_empty())
    })
}

fn plain_response_text_is_no_goals(text: &str) -> bool {
    text.trim()
        .trim_end_matches('.')
        .eq_ignore_ascii_case("no goals")
}

fn flatten_tagged(tagged: &Value, active_info: Option<&Value>, acc: &mut Vec<RenderedSegment>) {
    match tagged {
        Value::String(text) => acc.push(RenderedSegment {
            text: text.clone(),
            info: active_info.cloned(),
        }),
        Value::Array(items) => {
            for item in items {
                flatten_tagged(item, active_info, acc);
            }
        }
        Value::Object(map) => {
            if let Some(tagged) = map.get("tag") {
                let next_info = match tagged {
                    Value::Array(items) => items.first().cloned().or_else(|| active_info.cloned()),
                    Value::Object(info) => info
                        .get("info")
                        .cloned()
                        .or_else(|| active_info.cloned())
                        .or_else(|| Some(tagged.clone())),
                    Value::Null => active_info.cloned(),
                    _ => Some(tagged.clone()),
                };

                if let Some(child) = tagged.as_array().and_then(|items| items.get(1)) {
                    flatten_tagged(child, next_info.as_ref(), acc);
                } else if let Some(items) = map.get("append").and_then(Value::as_array) {
                    for item in items {
                        flatten_tagged(item, next_info.as_ref(), acc);
                    }
                } else if let Some(text) = map.get("text").and_then(Value::as_str) {
                    acc.push(RenderedSegment {
                        text: text.to_string(),
                        info: next_info,
                    });
                }
            } else if let Some(text) = map.get("text").and_then(Value::as_str) {
                acc.push(RenderedSegment {
                    text: text.to_string(),
                    info: active_info.cloned(),
                });
            } else if let Some(items) = map.get("append").and_then(Value::as_array) {
                for item in items {
                    flatten_tagged(item, active_info, acc);
                }
            }
        }
        _ => {}
    }
}

fn flatten_tagged_to_segments(tagged: &Value) -> Vec<RenderedSegment> {
    let mut segments = Vec::new();
    flatten_tagged(tagged, None, &mut segments);
    segments
}

fn tagged_has_visible_text(tagged: &Value) -> bool {
    let segments = flatten_tagged_to_segments(tagged);
    let mut text = String::new();
    for segment in segments {
        text.push_str(&segment.text);
    }
    !text.trim().is_empty()
}

fn segments_to_lines(segments: Vec<RenderedSegment>) -> Vec<RenderedLine> {
    let mut lines = vec![Vec::new()];
    for segment in segments {
        let mut start = 0;
        for (idx, ch) in segment.text.char_indices() {
            if ch == '\n' {
                if start != idx {
                    lines.last_mut().unwrap().push(RenderedSegment {
                        text: segment.text[start..idx].to_string(),
                        info: segment.info.clone(),
                    });
                }
                lines.push(Vec::new());
                start = idx + ch.len_utf8();
            }
        }
        if start < segment.text.len() {
            lines.last_mut().unwrap().push(RenderedSegment {
                text: segment.text[start..].to_string(),
                info: segment.info.clone(),
            });
        }
    }
    lines
}

fn render_hypothesis_lines(hyp: &InteractiveHypothesis) -> Vec<RenderedLine> {
    let mut segments = vec![RenderedSegment {
        text: hyp.names.join(" "),
        info: None,
    }];
    segments.push(RenderedSegment {
        text: " : ".to_string(),
        info: None,
    });
    segments.extend(flatten_tagged_to_segments(&hyp.r#type));
    if let Some(value) = &hyp.val {
        segments.push(RenderedSegment {
            text: " := ".to_string(),
            info: None,
        });
        segments.extend(flatten_tagged_to_segments(value));
    }
    segments_to_lines(segments)
}

fn render_interactive_goal_lines(goal: &InteractiveGoal) -> Vec<RenderedLine> {
    let mut lines = Vec::new();
    if let Some(case_name) = goal.user_name.as_ref().filter(|name| !name.is_empty()) {
        lines.push(vec![RenderedSegment {
            text: format!("case {case_name}"),
            info: None,
        }]);
    }

    for hypothesis in &goal.hyps {
        lines.extend(render_hypothesis_lines(hypothesis));
    }

    let mut goal_segments = vec![RenderedSegment {
        text: goal.goal_prefix.clone().unwrap_or_else(|| "⊢ ".to_string()),
        info: None,
    }];
    goal_segments.extend(flatten_tagged_to_segments(&goal.r#type));
    lines.extend(segments_to_lines(goal_segments));
    lines
}

fn plain_goal_score(result: Option<&PlainGoalResult>) -> u8 {
    match result {
        None => 0,
        Some(result) if result.goals.is_empty() => 1,
        Some(_) => 2,
    }
}

fn plain_term_goal_score(result: Option<&PlainTermGoalResult>) -> u8 {
    if result.and_then(|result| result.goal.as_ref()).is_some() {
        1
    } else {
        0
    }
}

fn interactive_goal_score(result: Option<&InteractiveGoalsResult>) -> u8 {
    let Some(result) = result else {
        return 0;
    };
    if result.goals.is_empty() {
        0
    } else if result
        .goals
        .iter()
        .all(|goal| tagged_has_visible_text(&goal.r#type))
    {
        2
    } else {
        1
    }
}

fn interactive_term_goal_score(result: Option<&InteractiveGoal>) -> u8 {
    match result {
        None => 0,
        Some(goal) if tagged_has_visible_text(&goal.r#type) => 2,
        Some(_) => 1,
    }
}

impl BufferBuilder {
    fn push_line(&mut self, line: &RenderedLine) {
        for segment in line {
            if segment.text.is_empty() {
                continue;
            }
            let start = self.char_len;
            self.text.push_str(&segment.text);
            self.char_len += segment.text.chars().count();
            if let Some(info) = &segment.info {
                self.spans.push(InfoSpan {
                    start,
                    end: self.char_len,
                    info: info.clone(),
                });
            }
        }
        self.text.push('\n');
        self.char_len += 1;
    }
}

fn push_section_separator(builder: &mut BufferBuilder) {
    if !builder.text.is_empty() && !builder.text.ends_with("\n\n") {
        builder.push_line(&Vec::new());
    }
}

fn build_rendered_payload(
    _source: &SourceContext,
    plain_goal: Option<PlainGoalResult>,
    plain_term_goal: Option<PlainTermGoalResult>,
    interactive_goals: Option<InteractiveGoalsResult>,
    interactive_term_goal: Option<InteractiveGoal>,
    diagnostics: &[lsp::Diagnostic],
    goals_accomplished: bool,
) -> (String, Vec<InfoSpan>) {
    let mut builder = BufferBuilder::default();

    let interactive_goal_score = interactive_goal_score(interactive_goals.as_ref());
    let interactive_term_goal_score = interactive_term_goal_score(interactive_term_goal.as_ref());
    let has_error_diagnostic = diagnostics
        .iter()
        .any(|diag| diag.severity == Some(DiagnosticSeverity::ERROR));

    if goals_accomplished && !has_error_diagnostic {
        builder.push_line(&vec![RenderedSegment {
            text: GOALS_ACCOMPLISHED_TEXT.to_string(),
            info: None,
        }]);
        push_section_separator(&mut builder);
    }

    if interactive_goal_score > 1 {
        for (index, goal) in interactive_goals.unwrap().goals.iter().enumerate() {
            if index > 0 {
                builder.push_line(&Vec::new());
            }
            for line in render_interactive_goal_lines(goal) {
                builder.push_line(&line);
            }
        }
    } else {
        let lines = match plain_goal {
            Some(result) if !result.goals.is_empty() => result
                .goals
                .iter()
                .filter(|goal| !plain_response_text_is_no_goals(goal))
                .flat_map(|goal| {
                    let mut lines = plain_text_lines(goal);
                    lines.push(Vec::new());
                    lines
                })
                .collect::<Vec<_>>(),
            Some(result) if plain_response_text_is_no_goals(&result.rendered) => Vec::new(),
            Some(result) if !plain_text_has_visible_content(&result.rendered) => {
                let _ = result;
                Vec::new()
            }
            Some(result) => plain_text_lines(&result.rendered),
            None => Vec::new(),
        };
        for line in lines {
            builder.push_line(&line);
        }
    }

    if interactive_term_goal_score > 1 || plain_term_goal_score(plain_term_goal.as_ref()) > 0 {
        push_section_separator(&mut builder);
        if let Some(result) = plain_term_goal.as_ref() {
            builder.push_line(&vec![RenderedSegment {
                text: term_goal_title(result),
                info: None,
            }]);
        }
        if interactive_term_goal_score > 1 {
            for line in render_interactive_goal_lines(&interactive_term_goal.unwrap()) {
                builder.push_line(&line);
            }
        } else if let Some(result) = plain_term_goal {
            if let Some(goal) = result.goal {
                if !plain_response_text_is_no_goals(&goal) {
                    for line in plain_text_lines(&goal) {
                        builder.push_line(&line);
                    }
                }
            }
        }
    }

    let diagnostic_lines = render_diagnostic_sections(diagnostics);
    if !diagnostic_lines.is_empty() {
        push_section_separator(&mut builder);
        for line in diagnostic_lines {
            builder.push_line(&line);
        }
    }

    (builder.text, builder.spans)
}

fn render_diagnostic_sections(diagnostics: &[lsp::Diagnostic]) -> Vec<RenderedLine> {
    let mut lines = Vec::new();
    for diagnostic in diagnostics {
        lines.push(vec![RenderedSegment {
            text: format_diagnostic_header(diagnostic),
            info: None,
        }]);
        for message_line in diagnostic.message.lines() {
            lines.push(vec![RenderedSegment {
                text: message_line.to_string(),
                info: None,
            }]);
        }
        if let Some(code) = diagnostic.code.as_ref() {
            lines.push(vec![RenderedSegment {
                text: format!("Error code: {}", diagnostic_code_text(code)),
                info: None,
            }]);
        }
        if let Some(url) = diagnostic_explanation_url(diagnostic) {
            lines.push(vec![RenderedSegment {
                text: format!("View explanation: {url}"),
                info: None,
            }]);
        }
        lines.push(Vec::new());
    }
    if matches!(lines.last(), Some(last) if last.is_empty()) {
        lines.pop();
    }
    lines
}

fn format_diagnostic_header(diagnostic: &lsp::Diagnostic) -> String {
    let severity = match diagnostic.severity {
        Some(DiagnosticSeverity::ERROR) => "error",
        Some(DiagnosticSeverity::WARNING) => "warning",
        Some(DiagnosticSeverity::INFORMATION) => "information",
        Some(DiagnosticSeverity::HINT) => "hint",
        _ => "diagnostic",
    };
    format!(
        "▼ {}:{}-{}:{}: {}:",
        diagnostic.range.start.line + 1,
        diagnostic.range.start.character + 1,
        diagnostic.range.end.line + 1,
        diagnostic.range.end.character + 1,
        severity
    )
}

fn diagnostic_code_text(code: &NumberOrString) -> String {
    match code {
        NumberOrString::Number(n) => n.to_string(),
        NumberOrString::String(s) => s.clone(),
    }
}

fn diagnostic_explanation_url(diagnostic: &lsp::Diagnostic) -> Option<String> {
    if let Some(description) = diagnostic.code_description.as_ref() {
        return Some(description.href.to_string());
    }

    let NumberOrString::String(code) = diagnostic.code.as_ref()? else {
        return None;
    };
    if !code.starts_with("lean.") {
        return None;
    }

    let slug = code.trim_start_matches("lean.");
    Some(format!(
        "https://lean-lang.org/doc/reference/latest/Error-Explanations/About___--{slug}/#{anchor}",
        anchor = code.replace('.', "___")
    ))
}

fn current_line_diagnostics(editor: &Editor, source: &SourceContext) -> Vec<lsp::Diagnostic> {
    let Ok(uri) = Uri::try_from(&source.uri) else {
        return Vec::new();
    };
    let line = source.position.line;
    editor
        .diagnostics
        .get(&uri)
        .into_iter()
        .flat_map(|diagnostics| diagnostics.iter().map(|(diagnostic, _provider)| diagnostic))
        .filter(|diagnostic| {
            let start = diagnostic.range.start.line;
            let end = diagnostic.range.end.line.max(start);
            start <= line && line <= end
        })
        .cloned()
        .collect()
}

fn current_line_goals_accomplished(editor: &Editor, source: &SourceContext) -> bool {
    let Some(doc_id) = source.doc_id else {
        return false;
    };
    let Some(doc) = editor.documents.get(&doc_id) else {
        return false;
    };

    editor.line_within_lean_goals_accomplished(doc, source.position.line as usize)
}

fn term_goal_title(result: &PlainTermGoalResult) -> String {
    let Some(range) = result.range.as_ref() else {
        return "▼ expected type".to_string();
    };
    format!(
        "▼ expected type ({}:{}-{}:{})",
        range.start.line + 1,
        range.start.character + 1,
        range.end.line + 1,
        range.end.character + 1
    )
}

fn update_infoview_buffer(editor: &mut Editor, payload: RefreshPayload) {
    let (doc_id, view_id, render_text, render_spans) = {
        let mut state = lock_state();
        sync_state(editor, &mut state);
        if state.request_seq != 0
            && payload.request_seq != 0
            && payload.request_seq != state.request_seq
        {
            return;
        }
        let (Some(doc_id), Some(view_id)) = (state.infoview_doc, state.infoview_view) else {
            return;
        };

        if let Some(source_doc) = payload.source_doc {
            state.last_source_doc = Some(source_doc);
        }
        if let Some(source_view) = payload.source_view {
            state.last_source_view = Some(source_view);
        }
        if let Some(source_server) = payload.source_server {
            state.last_source_server = Some(source_server);
        }
        if let Some(source_uri) = payload.source_uri.clone() {
            state.last_source_uri = Some(source_uri);
        }
        if let Some(source_position) = payload.source_position {
            state.last_source_position = Some(source_position);
        }
        if let Some(session) = payload.session.clone() {
            state.rpc_session = Some(session);
        } else if payload.source_uri.is_some() {
            state.rpc_session = None;
        }
        state.spans = payload.spans.clone();
        (doc_id, view_id, payload.text.clone(), payload.spans.clone())
    };

    let Some(doc) = editor.documents.get_mut(&doc_id) else {
        return;
    };
    let view = editor.tree.get_mut(view_id);
    let new_rope = Rope::from(render_text.as_str());
    let transaction = compare_ropes(doc.text(), &new_rope);
    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    doc.reset_modified();
    doc.readonly = true;
    log::debug!(
        "lean infoview updated doc={:?} view={:?} chars={} spans={}",
        doc_id,
        view_id,
        doc.text().len_chars(),
        render_spans.len()
    );
}

fn infoview_status_payload(source: Option<SourceContext>, message: &str) -> RefreshPayload {
    match source {
        Some(source) => RefreshPayload {
            request_seq: 0,
            source_doc: source.doc_id,
            source_view: source.view_id,
            source_server: Some(source.server_id),
            source_uri: Some(source.uri.clone()),
            source_position: Some(source.position),
            session: None,
            text: format!("{}\n", message),
            spans: Vec::new(),
        },
        None => RefreshPayload {
            request_seq: 0,
            source_doc: None,
            source_view: None,
            source_server: None,
            source_uri: None,
            source_position: None,
            session: None,
            text: format!("{}\n", message),
            spans: Vec::new(),
        },
    }
}

fn pos_with_offset(source: &SourceContext, offset: i32) -> lsp::TextDocumentPositionParams {
    let character = if offset < 0 {
        source
            .position
            .character
            .saturating_sub(offset.unsigned_abs())
    } else {
        source.position.character.saturating_add(offset as u32)
    };
    lsp::TextDocumentPositionParams {
        text_document: source.text_document.clone(),
        position: Position {
            line: source.position.line,
            character,
        },
    }
}

async fn best_plain_goal(
    client: &helix_lsp::Client,
    source: &SourceContext,
) -> Option<PlainGoalResult> {
    let mut best = None;
    for offset in GOAL_QUERY_OFFSETS {
        let params = pos_with_offset(source, offset);
        let result = client.call::<LeanPlainGoalRequest>(params).await.ok();
        if plain_goal_score(result.as_ref()) > plain_goal_score(best.as_ref()) {
            best = result;
        }
    }
    best
}

async fn best_plain_term_goal(
    client: &helix_lsp::Client,
    source: &SourceContext,
) -> Option<PlainTermGoalResult> {
    let mut best = None;
    for offset in GOAL_QUERY_OFFSETS {
        let params = pos_with_offset(source, offset);
        let result = client.call::<LeanPlainTermGoalRequest>(params).await.ok();
        if plain_term_goal_score(result.as_ref()) > plain_term_goal_score(best.as_ref()) {
            best = result;
        }
    }
    best
}

async fn ensure_rpc_session(
    client: &helix_lsp::Client,
    source: &SourceContext,
    session: Option<RpcSession>,
) -> Option<RpcSession> {
    if let Some(session) = session {
        if session.uri == source.uri && session.server_id == source.server_id {
            return Some(session);
        }
    }

    let result = client
        .call::<LeanRpcConnectRequest>(RpcConnectParams {
            uri: source.uri.clone(),
        })
        .await
        .ok()?;

    Some(RpcSession {
        server_id: source.server_id,
        uri: source.uri.clone(),
        session_id: result.session_id,
    })
}

async fn rpc_call<T: for<'de> Deserialize<'de>>(
    client: &helix_lsp::Client,
    source: &SourceContext,
    session: &RpcSession,
    method: &str,
    params: Value,
) -> Option<T> {
    let (text_document, position) = rpc_envelope_context(source, &params);
    let result = client
        .call::<LeanRpcCallRequest>(RpcCallParams {
            text_document,
            position,
            session_id: session.session_id.clone(),
            method: method.to_string(),
            params,
        })
        .await
        .ok()?;
    serde_json::from_value(result).ok()
}

fn rpc_envelope_context(
    source: &SourceContext,
    params: &Value,
) -> (TextDocumentIdentifier, Position) {
    let text_document = params
        .get("textDocument")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_else(|| source.text_document.clone());
    let position = params
        .get("position")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or(source.position);
    (text_document, position)
}

async fn best_interactive_goals(
    client: &helix_lsp::Client,
    source: &SourceContext,
    session: &RpcSession,
) -> Option<InteractiveGoalsResult> {
    let mut best = None;
    for offset in GOAL_QUERY_OFFSETS {
        let params = serde_json::to_value(pos_with_offset(source, offset)).ok()?;
        let result = rpc_call(
            client,
            source,
            session,
            "Lean.Widget.getInteractiveGoals",
            params,
        )
        .await;
        if interactive_goal_score(result.as_ref()) > interactive_goal_score(best.as_ref()) {
            best = result;
        }
    }
    best
}

async fn best_interactive_term_goal(
    client: &helix_lsp::Client,
    source: &SourceContext,
    session: &RpcSession,
) -> Option<InteractiveGoal> {
    let mut best = None;
    for offset in GOAL_QUERY_OFFSETS {
        let params = serde_json::to_value(pos_with_offset(source, offset)).ok()?;
        let result = rpc_call(
            client,
            source,
            session,
            "Lean.Widget.getInteractiveTermGoal",
            params,
        )
        .await;
        if interactive_term_goal_score(result.as_ref()) > interactive_term_goal_score(best.as_ref())
        {
            best = result;
        }
    }
    best
}

fn schedule_refresh(cx: &mut Context) {
    let source = current_source_context(cx.editor, true);
    let Some((_, _)) = ensure_infoview_open(cx.editor, false) else {
        return;
    };

    let Some(source) = source else {
        update_infoview_buffer(
            cx.editor,
            infoview_status_payload(None, "Open a Lean buffer to populate the infoview."),
        );
        return;
    };

    let Some(client) = cx.editor.language_servers.get_by_id(source.server_id).cloned() else {
        update_infoview_buffer(
            cx.editor,
            infoview_status_payload(Some(source), "Lean language server is not available."),
        );
        return;
    };

    let (request_seq, existing_session) = {
        let mut state = lock_state();
        sync_state(cx.editor, &mut state);
        state.request_seq += 1;
        (state.request_seq, state.rpc_session.clone())
    };
    let diagnostics = current_line_diagnostics(cx.editor, &source);
    let goals_accomplished = current_line_goals_accomplished(cx.editor, &source);
    enqueue_refresh(
        source,
        client,
        diagnostics,
        goals_accomplished,
        request_seq,
        existing_session,
    );
}

fn enqueue_refresh(
    source: SourceContext,
    client: std::sync::Arc<helix_lsp::Client>,
    diagnostics: Vec<lsp::Diagnostic>,
    goals_accomplished: bool,
    request_seq: u64,
    existing_session: Option<RpcSession>,
) {
    tokio::spawn(async move {
        let plain_goal = best_plain_goal(&client, &source).await;
        let plain_term_goal = best_plain_term_goal(&client, &source).await;
        let session = ensure_rpc_session(&client, &source, existing_session).await;
        let (interactive_goals, interactive_term_goal) = match session.as_ref() {
            Some(session) => (
                best_interactive_goals(&client, &source, session).await,
                best_interactive_term_goal(&client, &source, session).await,
            ),
            None => (None, None),
        };

        let (text, spans) = build_rendered_payload(
            &source,
            plain_goal,
            plain_term_goal,
            interactive_goals,
            interactive_term_goal,
            &diagnostics,
            goals_accomplished,
        );
        let payload = RefreshPayload {
            request_seq,
            source_doc: source.doc_id,
            source_view: source.view_id,
            source_server: Some(source.server_id),
            source_uri: Some(source.uri.clone()),
            source_position: Some(source.position),
            session,
            text,
            spans,
        };

        job::dispatch(move |editor, _| {
            update_infoview_buffer(editor, payload);
        })
        .await;
    });
}

pub(crate) fn maybe_auto_refresh(cx: &mut Context) {
    let visible = {
        let mut state = lock_state();
        sync_state(cx.editor, &mut state);
        state.infoview_doc.is_some()
    };
    if !visible || is_infoview_doc(cx.editor) {
        return;
    }

    if current_source_context(cx.editor, false).is_some() {
        schedule_refresh(cx);
    } else {
        update_infoview_buffer(
            cx.editor,
            infoview_status_payload(None, "Open a Lean buffer to populate the infoview."),
        );
    }
}

pub(crate) fn maybe_auto_refresh_editor(editor: &mut Editor, doc_id: Option<DocumentId>) {
    maybe_auto_open(editor);

    let visible = {
        let mut state = lock_state();
        sync_state(editor, &mut state);
        state.infoview_doc.is_some()
    };
    if !visible || is_infoview_doc(editor) {
        return;
    }

    let Some(source) = current_source_context(editor, false) else {
        update_infoview_buffer(
            editor,
            infoview_status_payload(None, "Open a Lean buffer to populate the infoview."),
        );
        return;
    };

    if let Some(doc_id) = doc_id {
        if source.doc_id != Some(doc_id) {
            return;
        }
    }

    let Some(client) = editor.language_servers.get_by_id(source.server_id).cloned() else {
        update_infoview_buffer(
            editor,
            infoview_status_payload(Some(source), "Lean language server is not available."),
        );
        return;
    };

    let (request_seq, existing_session) = {
        let mut state = lock_state();
        sync_state(editor, &mut state);
        state.request_seq += 1;
        (state.request_seq, state.rpc_session.clone())
    };
    let diagnostics = current_line_diagnostics(editor, &source);
    let goals_accomplished = current_line_goals_accomplished(editor, &source);
    enqueue_refresh(
        source,
        client,
        diagnostics,
        goals_accomplished,
        request_seq,
        existing_session,
    );
}

fn lookup_info_at_cursor(editor: &Editor) -> Option<Value> {
    let (view, doc) = current_ref!(editor);
    let cursor = doc
        .selection(view.id)
        .primary()
        .cursor(doc.text().slice(..));
    let line = doc.text().char_to_line(cursor);
    let line_start = doc.text().line_to_char(line);
    let line_end = if line + 1 < doc.text().len_lines() {
        doc.text().line_to_char(line + 1)
    } else {
        doc.text().len_chars()
    };

    let mut last = None;
    let state = lock_state();
    log::debug!(
        "lean infoview lookup doc={:?} view={:?} cursor={} spans={}",
        doc.id(),
        view.id,
        cursor,
        state.spans.len()
    );
    for span in &state.spans {
        if span.end <= line_start || span.start >= line_end {
            continue;
        }
        if cursor >= span.start && cursor < span.end {
            log::debug!(
                "lean infoview lookup hit span start={} end={} info={}",
                span.start,
                span.end,
                span.info
            );
            return Some(span.info.clone());
        }
        if span.end <= cursor {
            last = Some(span.info.clone());
        }
    }
    if last.is_some() {
        log::debug!("lean infoview lookup using trailing span info");
    } else {
        log::debug!("lean infoview lookup found no span");
    }
    last
}

fn choose_target_view(
    preferred: Option<ViewId>,
    infoview_doc: Option<DocumentId>,
    candidates: &[(ViewId, DocumentId)],
) -> Option<ViewId> {
    if let Some(view_id) = preferred {
        if candidates
            .iter()
            .any(|(candidate, doc_id)| *candidate == view_id && Some(*doc_id) != infoview_doc)
        {
            return Some(view_id);
        }
    }

    candidates
        .iter()
        .find_map(|(view_id, doc_id)| (Some(*doc_id) != infoview_doc).then_some(*view_id))
}

fn goto_target_view(editor: &Editor, preferred: Option<ViewId>) -> Option<ViewId> {
    let infoview_doc = {
        let state = lock_state();
        state.infoview_doc
    };
    let candidates: Vec<_> = editor
        .tree
        .views()
        .map(|(view, _)| (view.id, view.doc))
        .collect();
    choose_target_view(preferred, infoview_doc, &candidates)
}

fn unwrap_goto_info(info: Value) -> Value {
    info.get("info").cloned().unwrap_or(info)
}

impl LeanGotoLocation {
    fn to_location(&self) -> Option<lsp::Location> {
        let uri = self.target_uri.clone().or_else(|| self.uri.clone())?;
        let range = self
            .target_selection_range
            .or(self.target_range)
            .or(self.range)?;
        Some(lsp::Location::new(uri, range))
    }
}

pub(crate) fn goto_from_infoview(cx: &mut Context, kind: &'static str) -> bool {
    if !is_infoview_doc(cx.editor) {
        return false;
    }

    let Some(info) = lookup_info_at_cursor(cx.editor) else {
        cx.editor.set_error("No Lean info at cursor.");
        return true;
    };

    let (server_id, session_id, source_uri, source_position, offset_encoding, preferred_target_view) = {
        let state = lock_state();
        let (Some(server_id), Some(source_uri), Some(source_position), Some(session)) = (
            state.last_source_server,
            state.last_source_uri.clone(),
            state.last_source_position,
            state.rpc_session.clone(),
        ) else {
            cx.editor.set_error("Lean infoview is not ready yet.");
            return true;
        };
        if session.uri != source_uri || session.server_id != server_id {
            cx.editor
                .set_error("Lean infoview session is stale; refresh first.");
            return true;
        }
        let Some(client) = cx.editor.language_servers.get_by_id(server_id) else {
            cx.editor
                .set_error("Lean language server is not available.");
            return true;
        };
        (
            server_id,
            session.session_id,
            source_uri,
            source_position,
            client.offset_encoding(),
            state.last_source_view,
        )
    };
    let target_view = goto_target_view(cx.editor, preferred_target_view);

    let Some(client) = cx.editor.language_servers.get_by_id(server_id).cloned() else {
        cx.editor
            .set_error("Lean language server is not available.");
        return true;
    };

    cx.jobs.callback(async move {
        let result = client
            .call::<LeanRpcCallRequest>(RpcCallParams {
                text_document: TextDocumentIdentifier::new(source_uri.clone()),
                position: source_position,
                session_id,
                method: "Lean.Widget.getGoToLocation".to_string(),
                params: json!({ "kind": kind, "info": unwrap_goto_info(info) }),
            })
            .await
            .context("Lean.Widget.getGoToLocation failed")?;

        let links: Vec<LeanGotoLocation> =
            serde_json::from_value(result).context("failed to decode Lean goto locations")?;
        let locations: Vec<lsp::Location> = links
            .into_iter()
            .filter_map(|link| link.to_location())
            .collect();
        log::debug!(
            "lean infoview goto kind={} locations={}",
            kind,
            locations.len()
        );

        Ok(Callback::EditorCompositor(Box::new(
            move |editor, compositor| {
            if locations.is_empty() {
                editor.set_error("No Lean locations found.");
            } else {
                if let Some(target_view) = target_view {
                    log::debug!("lean infoview goto focusing source view={:?}", target_view);
                    editor.focus(target_view);
                    }
                    lsp_commands::show_locations(editor, compositor, locations, offset_encoding);
                }
            },
        )))
    });

    true
}

pub fn lean_infoview_toggle(cx: &mut Context) {
    if is_infoview_doc(cx.editor) {
        close_infoview(cx.editor);
        return;
    }

    let visible = {
        let mut state = lock_state();
        sync_state(cx.editor, &mut state);
        state.infoview_doc.is_some()
    };
    if visible {
        let source_doc = current_source_context(cx.editor, true).and_then(|source| source.doc_id);
        if let Some(source_doc) = source_doc {
            let mut state = lock_state();
            state.auto_open_suppressed_doc = Some(source_doc);
        }
        close_infoview(cx.editor);
        return;
    }

    if ensure_infoview_open(cx.editor, false).is_some() {
        schedule_refresh(cx);
    }
}

pub fn lean_infoview_focus(cx: &mut Context) {
    if ensure_infoview_open(cx.editor, true).is_some() {
        schedule_refresh(cx);
    }
}

pub fn lean_infoview_refresh(cx: &mut Context) {
    if ensure_infoview_open(cx.editor, false).is_some() {
        schedule_refresh(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    use helix_view::{editor::GutterConfig, graphics::Rect, tree::Tree, View};

    fn doc_id(n: usize) -> DocumentId {
        unsafe { std::mem::transmute(NonZeroUsize::new(n).unwrap()) }
    }

    #[test]
    fn plain_text_lines_drops_blank_before_goal_marker() {
        let lines = plain_text_lines("foo\n\n⊢ bar");
        let rendered: Vec<String> = lines
            .into_iter()
            .map(|line| line.into_iter().map(|segment| segment.text).collect())
            .collect();

        assert_eq!(rendered, vec!["foo".to_string(), "⊢ bar".to_string()]);
    }

    #[test]
    fn tagged_visibility_ignores_blank_text() {
        assert!(!tagged_has_visible_text(&json!({"text": "   "})));
        assert!(tagged_has_visible_text(&json!({"text": "goal"})));
    }

    #[test]
    fn flatten_tagged_extracts_spans_for_tagged_tokens_with_separator_text() {
        let tagged = json!({
            "append": [
                { "tag": { "info": { "name": "nat" } }, "text": "nat" },
                { "text": "/" },
                { "tag": { "info": { "name": "S" } }, "text": "S" }
            ]
        });

        let lines = segments_to_lines(flatten_tagged_to_segments(&tagged));
        let mut builder = BufferBuilder::default();
        builder.push_line(lines.first().unwrap());

        assert_eq!(builder.text, "nat/S\n");
        assert_eq!(builder.spans.len(), 2);
        assert_eq!(builder.spans[0].start, 0);
        assert_eq!(builder.spans[0].end, 3);
        assert_eq!(builder.spans[0].info, json!({ "name": "nat" }));
        assert_eq!(builder.spans[1].start, 4);
        assert_eq!(builder.spans[1].end, 5);
        assert_eq!(builder.spans[1].info, json!({ "name": "S" }));
    }

    #[test]
    fn rpc_envelope_context_prefers_inner_position_and_document() {
        let source = SourceContext {
            doc_id: Some(doc_id(20)),
            view_id: Some(ViewId::default()),
            server_id: LanguageServerId::default(),
            uri: Url::parse("file:///tmp/source.lean").unwrap(),
            position: Position::new(10, 20),
            text_document: TextDocumentIdentifier::new(
                Url::parse("file:///tmp/source.lean").unwrap(),
            ),
        };
        let params = json!({
            "textDocument": { "uri": "file:///tmp/inner.lean" },
            "position": { "line": 3, "character": 4 }
        });

        let (text_document, position) = rpc_envelope_context(&source, &params);
        assert_eq!(text_document.uri.as_str(), "file:///tmp/inner.lean");
        assert_eq!(position, Position::new(3, 4));
    }

    #[test]
    fn diagnostic_sections_render_information_and_error_metadata() {
        let diagnostics = vec![
            lsp::Diagnostic {
                range: LspRange {
                    start: Position::new(131, 0),
                    end: Position::new(131, 5),
                },
                severity: Some(DiagnosticSeverity::INFORMATION),
                code: None,
                code_description: None,
                source: None,
                message: "Day.monday".to_string(),
                related_information: None,
                tags: None,
                data: None,
            },
            lsp::Diagnostic {
                range: LspRange {
                    start: Position::new(131, 6),
                    end: Position::new(131, 21),
                },
                severity: Some(DiagnosticSeverity::ERROR),
                code: Some(NumberOrString::String("lean.unknownIdentifier".to_string())),
                code_description: Some(lsp::CodeDescription {
                    href: Url::parse("https://example.com/explain").unwrap(),
                }),
                source: None,
                message: "Unknown identifier `nex_working_day`".to_string(),
                related_information: None,
                tags: None,
                data: None,
            },
        ];

        let lines = render_diagnostic_sections(&diagnostics);
        let rendered: Vec<String> = lines
            .into_iter()
            .map(|line| line.into_iter().map(|segment| segment.text).collect())
            .collect();

        assert!(rendered.iter().any(|line| line.contains("132:1-132:6: information:")));
        assert!(rendered.iter().any(|line| line.contains("Day.monday")));
        assert!(rendered.iter().any(|line| line.contains("132:7-132:22: error:")));
        assert!(rendered.iter().any(|line| line.contains("lean.unknownIdentifier")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("View explanation: https://example.com/explain")));
        assert_eq!(
            diagnostic_explanation_url(&diagnostics[1]).as_deref(),
            Some("https://example.com/explain")
        );
    }

    #[test]
    fn diagnostic_explanation_url_falls_back_for_lean_error_codes() {
        let diagnostic = lsp::Diagnostic {
            range: LspRange {
                start: Position::new(131, 6),
                end: Position::new(131, 21),
            },
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(NumberOrString::String("lean.unknownIdentifier".to_string())),
            code_description: None,
            source: None,
            message: "Unknown identifier `nex_working_day`".to_string(),
            related_information: None,
            tags: None,
            data: None,
        };

        assert_eq!(
            diagnostic_explanation_url(&diagnostic).as_deref(),
            Some(
                "https://lean-lang.org/doc/reference/latest/Error-Explanations/About___--unknownIdentifier/#lean___unknownIdentifier"
            )
        );
    }

    #[test]
    fn interactive_term_goal_keeps_expected_type_title() {
        let source = SourceContext {
            doc_id: Some(doc_id(60)),
            view_id: Some(ViewId::default()),
            server_id: LanguageServerId::default(),
            uri: Url::parse("file:///tmp/source.lean").unwrap(),
            position: Position::new(131, 7),
            text_document: TextDocumentIdentifier::new(
                Url::parse("file:///tmp/source.lean").unwrap(),
            ),
        };

        let (text, _spans) = build_rendered_payload(
            &source,
            None,
            Some(PlainTermGoalResult {
                goal: Some("day".to_string()),
                range: Some(LspRange {
                    start: Position::new(131, 6),
                    end: Position::new(131, 29),
                }),
            }),
            None,
            Some(InteractiveGoal {
                r#type: json!({ "text": "day" }),
                ..InteractiveGoal::default()
            }),
            &[],
            false,
        );

        assert!(text.contains("▼ expected type (132:7-132:30)"));
        assert!(text.contains("⊢ day"));
        assert!(!text.starts_with('\n'));
    }

    #[test]
    fn diagnostics_only_do_not_start_with_blank_line() {
        let source = SourceContext {
            doc_id: Some(doc_id(60)),
            view_id: Some(ViewId::default()),
            server_id: LanguageServerId::default(),
            uri: Url::parse("file:///tmp/source.lean").unwrap(),
            position: Position::new(131, 7),
            text_document: TextDocumentIdentifier::new(
                Url::parse("file:///tmp/source.lean").unwrap(),
            ),
        };

        let (text, _spans) = build_rendered_payload(
            &source,
            None,
            None,
            None,
            None,
            &[lsp::Diagnostic {
                range: LspRange {
                    start: Position::new(131, 6),
                    end: Position::new(131, 21),
                },
                severity: Some(DiagnosticSeverity::ERROR),
                code: Some(NumberOrString::String("lean.unknownIdentifier".to_string())),
                code_description: None,
                source: None,
                message: "Unknown identifier `nex_working_day`".to_string(),
                related_information: None,
                tags: None,
                data: None,
            }],
            false,
        );

        assert!(text.starts_with("▼ 132:7-132:22: error:"));
        assert!(!text.starts_with('\n'));
    }

    #[test]
    fn goals_accomplished_replaces_no_goals_without_errors() {
        let source = SourceContext {
            doc_id: Some(doc_id(60)),
            view_id: Some(ViewId::default()),
            server_id: LanguageServerId::default(),
            uri: Url::parse("file:///tmp/source.lean").unwrap(),
            position: Position::new(10, 5),
            text_document: TextDocumentIdentifier::new(
                Url::parse("file:///tmp/source.lean").unwrap(),
            ),
        };

        let (text, _spans) = build_rendered_payload(
            &source,
            Some(PlainGoalResult {
                goals: Vec::new(),
                rendered: String::new(),
            }),
            None,
            None,
            None,
            &[],
            true,
        );

        assert!(text.contains(GOALS_ACCOMPLISHED_TEXT));
        assert!(!text.contains("No goals."));
    }

    #[test]
    fn goals_accomplished_fallback_is_top_level_with_expected_type() {
        let source = SourceContext {
            doc_id: Some(doc_id(60)),
            view_id: Some(ViewId::default()),
            server_id: LanguageServerId::default(),
            uri: Url::parse("file:///tmp/source.lean").unwrap(),
            position: Position::new(132, 9),
            text_document: TextDocumentIdentifier::new(
                Url::parse("file:///tmp/source.lean").unwrap(),
            ),
        };

        let (text, _spans) = build_rendered_payload(
            &source,
            None,
            Some(PlainTermGoalResult {
                goal: Some("day".to_string()),
                range: Some(LspRange {
                    start: Position::new(132, 7),
                    end: Position::new(132, 18),
                }),
            }),
            None,
            None,
            &[],
            true,
        );

        assert!(text.starts_with("✓ Goals accomplished.\n\n▼ expected type"));
        assert!(text.contains("\nday\n"));
    }

    #[test]
    fn plain_goal_no_goals_text_is_suppressed() {
        let source = SourceContext {
            doc_id: Some(doc_id(60)),
            view_id: Some(ViewId::default()),
            server_id: LanguageServerId::default(),
            uri: Url::parse("file:///tmp/source.lean").unwrap(),
            position: Position::new(12, 3),
            text_document: TextDocumentIdentifier::new(
                Url::parse("file:///tmp/source.lean").unwrap(),
            ),
        };

        let (text, _spans) = build_rendered_payload(
            &source,
            Some(PlainGoalResult {
                goals: Vec::new(),
                rendered: "No goals.".to_string(),
            }),
            None,
            None,
            None,
            &[],
            false,
        );

        assert!(!text.contains("No goals."));
        assert!(text.is_empty());
    }

    #[test]
    fn plain_goal_lowercase_no_goals_text_is_suppressed() {
        let source = SourceContext {
            doc_id: Some(doc_id(60)),
            view_id: Some(ViewId::default()),
            server_id: LanguageServerId::default(),
            uri: Url::parse("file:///tmp/source.lean").unwrap(),
            position: Position::new(12, 3),
            text_document: TextDocumentIdentifier::new(
                Url::parse("file:///tmp/source.lean").unwrap(),
            ),
        };

        let (text, _spans) = build_rendered_payload(
            &source,
            Some(PlainGoalResult {
                goals: Vec::new(),
                rendered: "no goals".to_string(),
            }),
            None,
            None,
            None,
            &[],
            false,
        );

        assert!(!text.to_ascii_lowercase().contains("no goals"));
        assert!(text.is_empty());
    }

    #[test]
    fn goto_location_prefers_target_selection_range_and_falls_back() {
        let preferred = LeanGotoLocation {
            target_uri: Some(Url::parse("file:///tmp/Test.lean").unwrap()),
            target_selection_range: Some(LspRange {
                start: Position::new(1, 2),
                end: Position::new(1, 4),
            }),
            target_range: Some(LspRange {
                start: Position::new(5, 6),
                end: Position::new(5, 7),
            }),
            ..Default::default()
        };

        let fallback = LeanGotoLocation {
            uri: Some(Url::parse("file:///tmp/Fallback.lean").unwrap()),
            range: Some(LspRange {
                start: Position::new(8, 9),
                end: Position::new(8, 10),
            }),
            ..Default::default()
        };

        let preferred_location = preferred.to_location().unwrap();
        assert_eq!(preferred_location.range.start, Position::new(1, 2));

        let fallback_location = fallback.to_location().unwrap();
        assert_eq!(fallback_location.range.start, Position::new(8, 9));
    }

    #[test]
    fn choose_target_view_prefers_non_infoview_source_and_falls_back() {
        let infoview_doc = Some(doc_id(10));
        let source_doc = doc_id(11);
        let other_doc = doc_id(12);

        let mut tree = Tree::new(Rect::new(0, 0, 80, 24));
        tree.insert(View::new(source_doc, GutterConfig::default()));
        let source_view = tree.focus;
        tree.split(
            View::new(infoview_doc.unwrap(), GutterConfig::default()),
            helix_view::tree::Layout::Vertical,
        );
        let infoview_view = tree.focus;
        tree.focus = source_view;
        tree.split(
            View::new(other_doc, GutterConfig::default()),
            helix_view::tree::Layout::Horizontal,
        );
        let other_view = tree.focus;

        let candidates = vec![
            (source_view, source_doc),
            (infoview_view, infoview_doc.unwrap()),
            (other_view, other_doc),
        ];

        assert_eq!(
            choose_target_view(Some(source_view), infoview_doc, &candidates),
            Some(source_view)
        );
        assert_eq!(
            choose_target_view(Some(infoview_view), infoview_doc, &candidates),
            Some(source_view)
        );
        assert_eq!(
            choose_target_view(None, infoview_doc, &candidates),
            Some(source_view)
        );
    }

    #[test]
    fn choose_target_view_returns_none_when_only_infoview_exists() {
        let infoview_doc = doc_id(10);

        let mut tree = Tree::new(Rect::new(0, 0, 80, 24));
        tree.insert(View::new(infoview_doc, GutterConfig::default()));
        let infoview_view = tree.focus;

        let candidates = vec![(infoview_view, infoview_doc)];

        assert_eq!(
            choose_target_view(Some(infoview_view), Some(infoview_doc), &candidates),
            None
        );
        assert_eq!(
            choose_target_view(None, Some(infoview_doc), &candidates),
            None
        );
    }

    #[test]
    fn choose_target_view_does_not_depend_on_state_lock() {
        let infoview_doc = Some(doc_id(30));
        let source_doc = doc_id(31);

        let mut tree = Tree::new(Rect::new(0, 0, 80, 24));
        tree.insert(View::new(source_doc, GutterConfig::default()));
        let source_view = tree.focus;
        tree.split(
            View::new(infoview_doc.unwrap(), GutterConfig::default()),
            helix_view::tree::Layout::Vertical,
        );
        let infoview_view = tree.focus;

        let candidates = vec![(source_view, source_doc), (infoview_view, infoview_doc.unwrap())];

        let _state_guard = lock_state();
        assert_eq!(
            choose_target_view(Some(source_view), infoview_doc, &candidates),
            Some(source_view)
        );
    }

}
