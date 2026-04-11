use helix_event::register_hook;
use helix_view::events::{DiagnosticsDidChange, DocumentDidOpen, LanguageServerInitialized};

use crate::commands::lean_infoview;
use crate::events::{PostCommand, PostInsertChar};

use super::Handlers;

pub(super) fn register_hooks(_handlers: &Handlers) {
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        lean_infoview::maybe_auto_open(event.editor);
        Ok(())
    });

    register_hook!(move |event: &mut LanguageServerInitialized<'_>| {
        lean_infoview::maybe_auto_open(event.editor);
        Ok(())
    });

    register_hook!(move |event: &mut DiagnosticsDidChange<'_>| {
        lean_infoview::maybe_auto_refresh_editor(event.editor, Some(event.doc));
        Ok(())
    });

    register_hook!(move |event: &mut PostCommand<'_, '_>| {
        lean_infoview::maybe_auto_open(event.cx.editor);
        lean_infoview::maybe_auto_refresh(event.cx);
        Ok(())
    });

    register_hook!(move |event: &mut PostInsertChar<'_, '_>| {
        lean_infoview::maybe_auto_open(event.cx.editor);
        lean_infoview::maybe_auto_refresh(event.cx);
        Ok(())
    });
}
