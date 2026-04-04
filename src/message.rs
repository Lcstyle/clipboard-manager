use std::sync::{Arc, Mutex};

use crate::{
    clipboard::ClipboardMessage,
    config::Config,
    db::{DbMessage, EntryId, MimeDataMap},
    editor_ipc::EditorToApp,
    ipc::EntrySummary,
    navigation::EventMsg,
};

/// Wraps a oneshot reply channel in `Arc<Mutex<Option<_>>>` so the message can
/// derive `Clone` (required by iced). Call `.reply(value)` to send the response
/// — the first caller wins, subsequent calls are no-ops.
#[derive(Debug, Clone)]
pub struct ReplyHandle<T>(pub Arc<Mutex<Option<tokio::sync::oneshot::Sender<T>>>>);

impl<T> ReplyHandle<T> {
    pub fn new(sender: tokio::sync::oneshot::Sender<T>) -> Self {
        Self(Arc::new(Mutex::new(Some(sender))))
    }

    pub fn reply(&self, value: T) {
        if let Some(sender) = self.0.lock().unwrap().take() {
            let _ = sender.send(value);
        }
    }
}

#[derive(Clone, Debug)]
pub enum AppMsg {
    ChangeConfig(Config),
    TogglePopup,
    ToggleQuickSettings,
    ClosePopup,
    Search(String),
    ClipboardEvent(ClipboardMessage),
    PrimaryClipboardEvent(ClipboardMessage),
    #[allow(dead_code)]
    RetryConnectingClipboard,
    Copy(EntryId),
    CopySpecial(MimeDataMap),
    Clear,
    Navigation(EventMsg),
    Db(DbMessage),
    ReturnToClipboard,
    Config(ConfigMsg),
    NextPage,
    PreviousPage,
    ContextMenu(ContextMenuMsg),
    LinkClicked(markdown::Url),
    DbusToggle,
    DbusListEntries {
        reply: ReplyHandle<Vec<EntrySummary>>,
    },
    DbusCopyEntry {
        id: EntryId,
        reply: ReplyHandle<Result<(), String>>,
    },
    DbusGetEntry {
        id: EntryId,
        reply: ReplyHandle<Result<(String, Vec<u8>), String>>,
    },
    EditLatest,
    ToggleFavoritesFilter,
    EditorEvent(EditorToApp),
    EditorProcessExited,
    DbusFavorites,
    DbusToggleSelections,
    DbusListFavorites {
        reply: ReplyHandle<Vec<FavoriteSummary>>,
    },
    BeginFavorite(EntryId),
    CancelFavorite,
    ConfirmFavorite(EntryId, Option<String>),
    #[allow(dead_code)]
    SuggestTitle(EntryId),
    TitleSuggested(EntryId, Option<String>),
    SetFavoriteTitle(EntryId, String),
    FavoriteTitleInput(String),
    SelectionSearch(String),
    SelectionCopy(u64),
    SelectionToFavorite(u64),
    ClearSelections,
    CursorCaptured {
        position: cosmic::iced_core::Point,
        target: crate::app::PopupKind,
    },
    /// Raw cursor position from event subscription — used to capture position
    /// on the fullscreen overlay without requiring physical mouse movement.
    RawCursorMoved(cosmic::iced_core::Point),
    /// No-op message used for fire-and-forget async tasks.
    Noop,
    /// Background DB persist completed (insert, delete, or clear).
    DbPersistComplete(Result<(), String>),
    /// Logical screen size from a wayland output event, used to clamp popup position.
    OutputSize(f32, f32),
    /// No longer used — kept to avoid breaking handler match.
    #[allow(dead_code)]
    OpenPositionedPopup {
        position: cosmic::iced_core::Point,
        target: crate::app::PopupKind,
    },
}

/// Summary of a favorite entry for CLI listing.
#[derive(Clone, Debug)]
pub struct FavoriteSummary {
    pub id: i64,
    pub title: Option<String>,
    pub preview: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Copy)]
pub enum ContextMenuMsg {
    RemoveFavorite(EntryId),
    AddFavorite(EntryId),
    Edit(EntryId),
    ShowQrCode(EntryId),
    Delete(EntryId),
}

use cosmic::widget::{markdown, menu::action::MenuAction};

impl MenuAction for ContextMenuMsg {
    type Message = AppMsg;

    fn message(&self) -> Self::Message {
        AppMsg::ContextMenu(*self)
    }
}

#[derive(Clone, Debug)]
pub enum ConfigMsg {
    PrivateMode(bool),
    #[expect(dead_code)]
    Horizontal(bool),
    UniqueSession(bool),
    SelectionBufferEnabled(bool),
    SelectionBufferSyncClipboard(bool),
}
