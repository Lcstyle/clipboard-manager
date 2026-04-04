use chrono::Utc;
use cosmic::app::Core;

use cosmic::iced::clipboard::mime::AsMimeTypes;
use cosmic::iced::keyboard::key::Named;
use cosmic::iced::window::Id;
use cosmic::iced::{self, Limits};

use cosmic::iced_core::widget::operation;
use cosmic::iced_futures::Subscription;
use cosmic::iced_runtime::core::window;
use cosmic::iced_runtime::platform_specific::wayland::layer_surface::SctkLayerSurfaceSettings;
use cosmic::iced_widget::qr_code;
use cosmic::iced_widget::scrollable::RelativeOffset;
use cosmic::iced_winit::commands::layer_surface::{
    self, KeyboardInteractivity, destroy_layer_surface, get_layer_surface,
};
use cosmic::iced_winit::commands::popup::{destroy_popup, get_popup};
use cosmic::widget::{MouseArea, Space};

use cosmic::{Element, app::Task};
use futures::StreamExt;
use futures::executor::block_on;
use regex::Regex;

use crate::clipboard::ClipboardError;
use crate::config::{Config, PRIVATE_MODE, SELECTION_BUFFER_ENABLED, SKIP_NEXT_CLIPBOARD};
use crate::db::{Content, DbMessage, DbTrait, EntryId, EntryTrait, MimeDataMap, MimeType, persist_op};
use crate::editor_ipc::{self, EditorToApp};
use crate::message::{AppMsg, ConfigMsg, ContextMenuMsg, FavoriteSummary};
use crate::navigation::EventMsg;
use crate::ipc::EntrySummary;
use crate::utils::{sanitize_preview, task_message};
use crate::ai;
use crate::selection_buffer::SelectionBuffer;
use crate::view::SCROLLABLE_ID;
use crate::{clipboard, clipboard_watcher, config, ipc, navigation};

use cosmic::{cosmic_config, iced_runtime};
use std::sync::atomic::{self, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const QUALIFIER: &str = "io.github";
pub const ORG: &str = "cosmic_utils";
pub const APP: &str = "cosmic-ext-applet-clipboard-manager";
pub const APPID: &str = constcat::concat!(QUALIFIER, ".", ORG, ".", APP);

/// MIME type set by password managers (e.g. KeePassXC) to indicate sensitive clipboard content.
/// When present, the entry should not be stored in clipboard history and the clipboard should
/// not be restored when the source application clears it.
const PASSWORD_MANAGER_HINT_MIME: &str = "x-kde-passwordManagerHint";

static EDITOR_SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Spawn a background task to persist a DB operation to SQLite.
/// The in-memory state has already been updated; this just writes to disk.
fn spawn_db_persist(db_path: &str, op: crate::db::DbPersistOp) -> Task<AppMsg> {
    let db_path = db_path.to_string();
    Task::perform(
        async move {
            persist_op(&db_path, op)
                .await
                .map_err(|e| e.to_string())
        },
        |result| cosmic::action::app(AppMsg::DbPersistComplete(result)),
    )
}

/// Copy data to the Wayland clipboard via the wlr-data-control protocol.
///
/// Uses `wl-clipboard-rs` to write directly to the compositor, avoiding the
/// need for an external `wl-copy` process. A background thread is spawned
/// internally to serve paste requests — this is expected Wayland behavior.
fn wl_copy(mime: &str, data: &[u8]) -> Result<(), wl_clipboard_rs::copy::Error> {
    use wl_clipboard_rs::copy::{MimeType, Options, Source};
    Options::new().copy(
        Source::Bytes(data.into()),
        MimeType::Specific(mime.to_string()),
    )
}

/// Tracks the editor subprocess spawned by the applet.
pub struct EditorProcess {
    pub entry_id: EntryId,
    pub mime: String,
    pub stdin_handle: std::process::ChildStdin,
    pub child: std::process::Child,
    pub stdout_rx: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<EditorToApp>>>>,
    pub session_id: u64,
}

impl Drop for EditorProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct AppState<Db: DbTrait> {
    core: Core,
    config_handler: cosmic_config::Config,
    popup: Option<Popup>,
    pub config: Config,
    pub db: Db,
    pub clipboard_state: ClipboardState,
    pub focused: usize,
    pub page: usize,
    pub qr_code: Option<Result<qr_code::Data, ()>>,
    last_quit: Option<(i64, PopupKind)>,
    pub preferred_mime_types_regex: Vec<Regex>,
    /// Tracks whether the last clipboard entry was sensitive (e.g. from a password manager).
    /// When true, the clipboard will not be restored on clear.
    last_entry_sensitive: bool,
    pub editor: Option<EditorProcess>,
    pub favoriting_state: Option<FavoritingState>,
    pub show_favorites_only: bool,
    pub current_entry_id: Option<EntryId>,
    pub selection_buffer: SelectionBuffer,
    /// Temporary overlay for capturing cursor position before opening a positioned popup.
    cursor_capture: Option<CursorCapture>,
    /// Suppress LayerEvent::Unfocused while a layer surface popup is open.
    /// Unfocused fires spuriously when the layer surface is created/configured.
    suppress_unfocus: bool,
    /// Cursor position for layer surface popups (content is positioned here
    /// within the fullscreen overlay surface).
    popup_position: Option<cosmic::iced_core::Point>,
    /// Logical screen size from the most recently seen wayland output, used
    /// to clamp layer surface popup position to keep it fully on-screen.
    screen_size: Option<(f32, f32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardState {
    Init,
    Connected,
    Error(ErrorState),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorState {
    MissingDataControlProtocol,
    Other(String),
}

impl ClipboardState {
    pub fn is_error(&self) -> bool {
        matches!(self, ClipboardState::Error(..))
    }
}

#[derive(Clone, Debug)]
pub struct Flags {
    pub config_handler: cosmic_config::Config,
    pub config: Config,
}

#[derive(Debug, Clone)]
struct Popup {
    pub kind: PopupKind,
    pub id: window::Id,
    /// Whether this popup was opened as a layer surface (for --toggle)
    /// vs an XDG popup (for icon click).
    pub is_layer_surface: bool,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PopupKind {
    Popup,
    QuickSettings,
    Favorites,
    Selections,
}

/// Tracks the temporary overlay surface used to capture cursor position.
#[derive(Debug, Clone, Copy)]
struct CursorCapture {
    overlay_id: Id,
    target_popup: PopupKind,
}

/// Phase of the inline "add favorite" title input flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FavoritingPhase {
    /// Waiting for an AI title suggestion.
    Suggesting,
    /// User is editing the title text.
    Editing,
}

/// State for the inline "add favorite" title input flow.
#[derive(Debug, Clone)]
pub struct FavoritingState {
    pub entry_id: EntryId,
    pub title_input: String,
    pub phase: FavoritingPhase,
}

impl<Db: DbTrait> AppState<Db> {
    fn focus_next(&mut self) -> Task<AppMsg> {
        if self.db.len() > 0 {
            self.focused = (self.focused + 1) % self.db.len();
            self.page = self.focused / self.config.maximum_entries_by_page.get() as usize;

            debug!("");
            debug!("len = {}", self.db.len());
            debug!("focused = {}", self.focused);
            debug!(
                "maximum_entries_by_page = {}",
                self.config.maximum_entries_by_page.get() as usize
            );
            debug!("page = {}", self.page);

            // will not work with last page but it is not used anyway because have bug
            let delta_y = (self.focused % self.config.maximum_entries_by_page.get() as usize)
                as f32
                / self.config.maximum_entries_by_page.get() as f32;

            debug!("delta_y = {}", delta_y);

            iced_runtime::task::widget(operation::scrollable::snap_to(
                SCROLLABLE_ID.clone(),
                RelativeOffset {
                    x: 0.,
                    y: delta_y.clamp(0.0, 1.0),
                },
            ))
        } else {
            Task::none()
        }
    }

    fn focus_previous(&mut self) -> Task<AppMsg> {
        if self.db.len() > 0 {
            self.focused = (self.focused + self.db.len() - 1) % self.db.len();
            self.page = self.focused / self.config.maximum_entries_by_page.get() as usize;

            debug!("");
            debug!("len = {}", self.db.len());
            debug!("focused = {}", self.focused);
            debug!(
                "maximum_entries_by_page = {}",
                self.config.maximum_entries_by_page.get() as usize
            );
            debug!("page = {}", self.page);

            let delta_y = (self.focused % self.config.maximum_entries_by_page.get() as usize)
                as f32
                / self.config.maximum_entries_by_page.get() as f32;

            debug!("delta_y = {}", delta_y);
            iced_runtime::task::widget(operation::scrollable::snap_to(
                SCROLLABLE_ID.clone(),
                RelativeOffset {
                    x: 0.,
                    y: delta_y.clamp(0.0, 1.0),
                },
            ))
        } else {
            Task::none()
        }
    }

    fn toggle_popup(&mut self, kind: PopupKind) -> Task<AppMsg> {
        self.toggle_popup_ext(kind, false)
    }

    fn toggle_popup_ext(&mut self, kind: PopupKind, force_layer_surface: bool) -> Task<AppMsg> {
        self.qr_code.take();

        // If cursor capture is in progress, cancel it
        if self.cursor_capture.is_some() {
            return self.close_popup();
        }

        match &self.popup {
            Some(popup) => {
                if popup.kind == kind {
                    self.close_popup()
                } else {
                    Task::batch(vec![self.close_popup(), self.open_popup(kind, force_layer_surface)])
                }
            }
            None => self.open_popup(kind, force_layer_surface),
        }
    }

    fn close_popup(&mut self) -> Task<AppMsg> {
        self.focused = 0;
        self.page = 0;
        self.db.set_query_and_search("".into());
        self.selection_buffer.search(String::new());
        self.favoriting_state = None;
        self.show_favorites_only = false;
        self.suppress_unfocus = false;
        self.popup_position = None;

        // Clean up cursor capture overlay if it's active
        if let Some(capture) = self.cursor_capture.take() {
            return destroy_layer_surface(capture.overlay_id);
        }

        if let Some(popup) = self.popup.take() {
            self.last_quit = Some((Utc::now().timestamp_millis(), popup.kind));

            if popup.is_layer_surface {
                destroy_layer_surface(popup.id)
            } else {
                destroy_popup(popup.id)
            }
        } else {
            Task::none()
        }
    }

    fn open_popup(&mut self, kind: PopupKind, force_layer_surface: bool) -> Task<AppMsg> {
        // handle the case where the popup was closed by clicking the icon
        if self
            .last_quit
            .map(|(t, k)| (Utc::now().timestamp_millis() - t) < 200 && k == kind)
            .unwrap_or(false)
        {
            return Task::none();
        }

        let use_layer_surface = force_layer_surface || self.config.horizontal;

        // For D-Bus triggered popups (force_layer_surface), create a fullscreen
        // transparent overlay to capture the cursor position, then render the
        // popup at that position within the same surface.
        //
        // NOTE: cosmic-comp doesn't send wl_pointer.enter when a layer surface
        // is created under the stationary cursor, so the first physical mouse
        // movement is required to capture the position.
        if use_layer_surface && matches!(kind, PopupKind::Popup | PopupKind::Favorites | PopupKind::Selections) {
            let overlay_id = Id::unique();
            self.cursor_capture = Some(CursorCapture {
                overlay_id,
                target_popup: kind,
            });
            self.suppress_unfocus = true;

            return get_layer_surface(SctkLayerSurfaceSettings {
                id: overlay_id,
                layer: layer_surface::Layer::Overlay,
                keyboard_interactivity: KeyboardInteractivity::Exclusive,
                anchor: layer_surface::Anchor::TOP
                    | layer_surface::Anchor::BOTTOM
                    | layer_surface::Anchor::LEFT
                    | layer_surface::Anchor::RIGHT,
                namespace: "clipboard manager".into(),
                size: None,
                size_limits: Limits::NONE.min_width(10000.0).min_height(10000.0),
                ..Default::default()
            });
        }

        // Normal popup (icon click) — use XDG popup anchored to applet
        let new_id = Id::unique();
        let popup = Popup {
            kind,
            id: new_id,
            is_layer_surface: false,
        };
        self.popup.replace(popup);

        match kind {
            PopupKind::Popup => {
                let mut popup_settings = self.core.applet.get_popup_settings(
                    self.core.main_window_id().unwrap(),
                    new_id,
                    None,
                    None,
                    None,
                );

                popup_settings.positioner.size_limits = Limits::NONE
                    .min_width(300.0)
                    .max_width(400.0)
                    .min_height(200.0)
                    .max_height(500.0);
                get_popup(popup_settings)
            }
            PopupKind::Favorites => {
                let mut popup_settings = self.core.applet.get_popup_settings(
                    self.core.main_window_id().unwrap(),
                    new_id,
                    None,
                    None,
                    None,
                );

                popup_settings.positioner.size_limits = Limits::NONE
                    .min_width(400.0)
                    .max_width(1200.0)
                    .min_height(200.0)
                    .max_height(500.0);
                get_popup(popup_settings)
            }
            PopupKind::Selections => {
                let mut popup_settings = self.core.applet.get_popup_settings(
                    self.core.main_window_id().unwrap(),
                    new_id,
                    None,
                    None,
                    None,
                );

                popup_settings.positioner.size_limits = Limits::NONE
                    .min_width(300.0)
                    .max_width(400.0)
                    .min_height(200.0)
                    .max_height(500.0);
                get_popup(popup_settings)
            }
            PopupKind::QuickSettings => {
                let mut popup_settings = self.core.applet.get_popup_settings(
                    self.core.main_window_id().unwrap(),
                    new_id,
                    None,
                    None,
                    None,
                );

                popup_settings.positioner.size_limits = Limits::NONE
                    .min_width(200.0)
                    .max_width(250.0)
                    .min_height(200.0)
                    .max_height(550.0);

                get_popup(popup_settings)
            }
        }
    }

    /// Spawn the editor as a separate process.
    ///
    /// IPC channels:
    /// - Applet → Editor: child's stdin pipe (length-prefixed JSON frames)
    /// - Editor → Applet: dedicated pipe on FD 3 (avoids stdout, which COSMIC writes to)
    fn open_editor_process(
        &mut self,
        entry_id: EntryId,
        text: &str,
        mime: String,
    ) -> Task<AppMsg> {
        use std::os::unix::io::{AsRawFd, IntoRawFd};
        use std::os::unix::process::CommandExt;
        use nix::unistd::{close, dup2, pipe};

        /// The file descriptor number the child uses for IPC writes back to the applet.
        const IPC_FD: i32 = 3;

        // Close existing editor if open
        if let Some(mut old_editor) = self.editor.take() {
            let _ = editor_ipc::write_frame(
                &mut old_editor.stdin_handle,
                &editor_ipc::AppToEditor::CloseRequested,
            );
            // Reap in background to avoid zombies
            std::thread::spawn(move || {
                let _ = old_editor.child.wait();
            });
        }

        let exe = std::env::current_exe()
            .unwrap_or_else(|_| "cosmic-ext-applet-clipboard-manager".into());

        // Create a dedicated pipe for Editor → Applet IPC.
        // The child writes to IPC_FD (3), the parent reads from ipc_read_fd.
        let (ipc_read_owned, ipc_write_owned) = match pipe() {
            Ok(fds) => fds,
            Err(e) => {
                error!("failed to create IPC pipe: {e}");
                return Task::none();
            }
        };

        // Extract raw fd values for the pre_exec closure (runs post-fork in the child,
        // so we pass plain integers — OwnedFds must not be moved into pre_exec because
        // they would be dropped in the child while the parent still owns them).
        let ipc_read_raw = ipc_read_owned.as_raw_fd();
        let ipc_write_raw = ipc_write_owned.as_raw_fd();

        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--editor-window")
            .stdin(std::process::Stdio::piped())
            // stdout/stderr are inherited — COSMIC can write freely without corrupting IPC
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());

        // SAFETY: pre_exec runs between fork() and exec() in the child process.
        // The closure only calls async-signal-safe functions (dup2, close) and
        // only touches file descriptors that are valid at this point:
        //   - ipc_write_raw: write end of the pipe, duplicated to IPC_FD then closed
        //   - ipc_read_raw: read end of the pipe, closed (only parent needs it)
        // The raw fd values are Copy integers captured by value — no ownership issues.
        unsafe {
            cmd.pre_exec(move || {
                // Move write end to the well-known IPC_FD
                dup2(ipc_write_raw, IPC_FD)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                // Close the original write-end fd (now duplicated at IPC_FD)
                if ipc_write_raw != IPC_FD {
                    close(ipc_write_raw).ok();
                }
                // Close the read end in the child — only the parent reads from it
                close(ipc_read_raw).ok();
                Ok(())
            });
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                error!("failed to spawn editor: {e}");
                // OwnedFds drop here and close automatically on failure
                drop(ipc_read_owned);
                drop(ipc_write_owned);
                return Task::none();
            }
        };

        // Parent: close the write end (only the child writes).
        // into_raw_fd() relinquishes ownership so close() is the manual cleanup.
        close(ipc_write_owned.into_raw_fd()).ok();

        // Convert the read end to a File via OwnedFd (safe ownership transfer).
        let ipc_read_file = std::fs::File::from(ipc_read_owned);

        let mut stdin_handle = child.stdin.take().unwrap();

        // Check if entry is a favorite (for in-place save behavior)
        let is_favorite = self.db.get_from_id(entry_id).map_or(false, |e| e.is_favorite());

        // Send Init message
        if let Err(e) = editor_ipc::write_frame(
            &mut stdin_handle,
            &editor_ipc::AppToEditor::Init {
                entry_id: entry_id.as_i64(),
                mime: mime.clone(),
                content: text.to_string(),
                is_favorite,
            },
        ) {
            error!("failed to send Init to editor: {e}");
            return Task::none();
        }

        // Spawn reader thread for the dedicated IPC pipe (not stdout)
        let (tx, rx) = tokio::sync::mpsc::channel::<EditorToApp>(8);
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(ipc_read_file);
            loop {
                match editor_ipc::read_frame::<EditorToApp>(&mut reader) {
                    Ok(msg) => {
                        eprintln!("[applet-reader] Read frame: {msg:?}");
                        if tx.blocking_send(msg).is_err() {
                            eprintln!("[applet-reader] mpsc send failed, breaking");
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("[applet-reader] read_frame error: {e}");
                        break;
                    }
                }
            }
            eprintln!("[applet-reader] Reader thread exiting");
        });

        let session_id = EDITOR_SESSION_COUNTER.fetch_add(1, atomic::Ordering::Relaxed);

        self.editor = Some(EditorProcess {
            entry_id,
            mime,
            stdin_handle,
            child,
            stdout_rx: Arc::new(Mutex::new(Some(rx))),
            session_id,
        });

        Task::none()
    }

    /// Send a message to the editor process. Returns false if send failed.
    fn send_to_editor(&mut self, msg: &editor_ipc::AppToEditor) -> bool {
        if let Some(editor) = &mut self.editor {
            if editor_ipc::write_frame(&mut editor.stdin_handle, msg).is_err() {
                warn!("failed to send to editor (pipe broken)");
                return false;
            }
            true
        } else {
            false
        }
    }
}

impl<Db: DbTrait + 'static> cosmic::Application for AppState<Db> {
    type Executor = cosmic::executor::Default;
    type Flags = Flags;
    type Message = AppMsg;
    const APP_ID: &'static str = APPID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, flags: Self::Flags) -> (Self, Task<Self::Message>) {
        let config = flags.config;
        PRIVATE_MODE.store(config.private_mode, atomic::Ordering::Relaxed);
        SELECTION_BUFFER_ENABLED.store(config.selection_buffer_enabled, atomic::Ordering::Relaxed);

        let db = block_on(async { Db::new(&config).await.unwrap() });

        let state = AppState {
            core,
            config_handler: flags.config_handler,
            popup: None,
            db,
            clipboard_state: ClipboardState::Init,
            focused: 0,
            qr_code: None,
            last_quit: None,
            page: 0,
            last_entry_sensitive: false,
            editor: None,
            favoriting_state: None,
            show_favorites_only: false,
            current_entry_id: None,
            selection_buffer: SelectionBuffer::new(config.selection_buffer_max_entries),
            cursor_capture: None,
            suppress_unfocus: false,
            popup_position: None,
            screen_size: None,
            preferred_mime_types_regex: config
                .preferred_mime_types
                .iter()
                .filter_map(|r| match Regex::new(r) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        error!("regex {e}");
                        None
                    }
                })
                .collect(),
            config,
        };

        #[cfg(debug_assertions)]
        let command = task_message(AppMsg::TogglePopup);

        #[cfg(not(debug_assertions))]
        let command = Task::none();

        (state, command)
    }

    fn on_close_requested(&self, id: window::Id) -> Option<AppMsg> {
        info!("on_close_requested");

        // If the cursor capture overlay is dismissed (e.g. Escape), close it
        if let Some(capture) = &self.cursor_capture {
            if capture.overlay_id == id {
                return Some(AppMsg::ClosePopup);
            }
        }

        if let Some(popup) = &self.popup
            && popup.id == id
        {
            return Some(AppMsg::ClosePopup);
        }
        None
    }

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        macro_rules! config_set {
            ($name: ident, $value: expr) => {
                match paste::paste! { self.config.[<set_ $name>](&self.config_handler, $value) } {
                    Ok(_) => {}
                    Err(err) => {
                        error!("failed to save config {:?}: {}", stringify!($name), err);
                    }
                }
            };
        }

        match message {
            AppMsg::Noop => {}
            AppMsg::DbPersistComplete(result) => {
                if let Err(e) = result {
                    error!("background DB persist failed: {e}");
                }
            }
            AppMsg::DbusToggle => {
                self.last_quit = None;
                return self.toggle_popup_ext(PopupKind::Popup, true);
            }
            AppMsg::RawCursorMoved(_) | AppMsg::Noop => {}
            AppMsg::CursorCaptured { position, target } => {
                if let Some(capture) = self.cursor_capture.take() {
                    // Fallback: MouseArea on_move transitions the overlay
                    self.popup = Some(Popup {
                        kind: target,
                        id: capture.overlay_id,
                        is_layer_surface: true,
                    });
                    self.popup_position = Some(position);
                    self.suppress_unfocus = true;
                }
            }
            AppMsg::OutputSize(w, h) => {
                self.screen_size = Some((w, h));
            }
            AppMsg::OpenPositionedPopup { .. } => {
                // Unused — kept for message enum compatibility.
                // Cursor-positioned popups now use the single-surface
                // transition in CursorCaptured instead.
            }
            AppMsg::DbusListEntries { reply } => {
                let summaries: Vec<EntrySummary> = self
                    .db
                    .iter()
                    .map(|entry| {
                        let preview = match entry
                            .preferred_content(&self.preferred_mime_types_regex)
                        {
                            Some((_, Content::Text(text))) => {
                                sanitize_preview(text, 100)
                            }
                            Some((_, Content::UriList(uris))) => {
                                sanitize_preview(&uris.join(" "), 100)
                            }
                            Some((_, Content::Image(_))) => "[image]".to_string(),
                            None => "[unknown]".to_string(),
                        };
                        EntrySummary {
                            id: entry.id().as_i64(),
                            is_favorite: entry.is_favorite(),
                            preview,
                        }
                    })
                    .collect();
                reply.reply(summaries);
            }
            AppMsg::DbusCopyEntry { id, reply } => {
                match self.db.get_from_id(id) {
                    Some(entry) => {
                        // Use wl_copy (zwlr_data_control) instead of copy_iced (wl_data_device)
                        // because the applet may not have Wayland focus when called via D-Bus.
                        let raw = entry.raw_content();
                        // Prefer text/plain, fall back to first available MIME type
                        let (mime, data) = if let Some(d) = raw.get(&MimeType::new("text/plain".to_string())) {
                            ("text/plain".to_string(), d.clone())
                        } else if let Some((m, d)) = raw.iter().next() {
                            (m.as_str().to_string(), d.clone())
                        } else {
                            reply.reply(Err("entry has no content".to_string()));
                            return Task::none();
                        };
                        match wl_copy(&mime, &data) {
                            Ok(()) => {
                                reply.reply(Ok(()));
                            }
                            Err(e) => {
                                error!("clipboard copy failed: {e}");
                                reply.reply(Err(format!("clipboard copy failed: {e}")));
                            }
                        }
                    }
                    None => {
                        reply.reply(Err(format!("entry {id} not found")));
                    }
                };
            }
            AppMsg::DbusGetEntry { id, reply } => {
                let result = match self.db.get_from_id(id) {
                    Some(entry) => {
                        match entry.preferred_content(&self.preferred_mime_types_regex) {
                            Some(((mime, raw), _)) => {
                                Ok((mime.to_string(), raw.clone()))
                            }
                            None => Err(format!("entry {id} has no displayable content")),
                        }
                    }
                    None => Err(format!("entry {id} not found")),
                };
                reply.reply(result);
            }
            AppMsg::ChangeConfig(config) => {
                if config.private_mode != self.config.private_mode {
                    PRIVATE_MODE.store(config.private_mode, atomic::Ordering::Relaxed);
                }
                if config.selection_buffer_enabled != self.config.selection_buffer_enabled {
                    SELECTION_BUFFER_ENABLED.store(config.selection_buffer_enabled, atomic::Ordering::Relaxed);
                }
                if config.selection_buffer_max_entries != self.config.selection_buffer_max_entries {
                    self.selection_buffer.set_max(config.selection_buffer_max_entries);
                }
                if config.preferred_mime_types != self.config.preferred_mime_types {
                    self.preferred_mime_types_regex = config
                        .preferred_mime_types
                        .iter()
                        .filter_map(|r| match Regex::new(r) {
                            Ok(r) => Some(r),
                            Err(e) => {
                                error!("regex {e}");
                                None
                            }
                        })
                        .collect();
                }
                self.config = config;
            }
            AppMsg::ToggleQuickSettings => {
                return self.toggle_popup(PopupKind::QuickSettings);
            }
            AppMsg::TogglePopup => {
                return self.toggle_popup(PopupKind::Popup);
            }
            AppMsg::ClosePopup => {
                return self.close_popup();
            }
            AppMsg::Search(query) => {
                self.db.set_query_and_search(query);
            }
            AppMsg::ClipboardEvent(message) => match message {
                clipboard::ClipboardMessage::Connected => {
                    self.clipboard_state = ClipboardState::Connected;
                }
                clipboard::ClipboardMessage::Data(data) => {
                    // Check if this clipboard event came from primary selection sync.
                    // If so, skip DB insert — the text is already in the selection buffer.
                    if SKIP_NEXT_CLIPBOARD.compare_exchange(
                        true,
                        false,
                        atomic::Ordering::AcqRel,
                        atomic::Ordering::Relaxed,
                    ).is_ok() {
                        info!("skipping DB insert for primary selection sync");
                    } else if data.contains_key(&MimeType::new(PASSWORD_MANAGER_HINT_MIME.to_string())) {
                        info!("clipboard contains password manager hint, skipping storage");
                        self.last_entry_sensitive = true;
                    } else {
                        self.last_entry_sensitive = false;
                        // Update in-memory state synchronously (instant), persist to
                        // SQLite in the background so the UI thread never blocks on I/O.
                        if let Some(op) = self.db.insert_to_memory(data) {
                            // Track the newest entry as the current clipboard buffer
                            self.current_entry_id = self.db.chronological_iter().next().map(|e| e.id());
                            return spawn_db_persist(self.db.db_path(), op);
                        }
                    }
                }
                #[expect(irrefutable_let_patterns)]
                clipboard::ClipboardMessage::Error(e) => {
                    error!("clipboard: {e}");

                    self.clipboard_state = if let ClipboardError::Watch(ref e) = e
                        && let clipboard_watcher::Error::MissingProtocol { name, .. } = **e
                        && name == "zwlr_data_control_manager_v1"
                    {
                        ClipboardState::Error(ErrorState::MissingDataControlProtocol)
                    } else {
                        ClipboardState::Error(ErrorState::Other(e.to_string()))
                    };
                }
                clipboard::ClipboardMessage::EmptyKeyboard => {
                    if self.last_entry_sensitive {
                        info!("clipboard cleared by password manager, not restoring");
                        self.last_entry_sensitive = false;
                    } else if let Some(data) = self.db.get(0) {
                        return copy_iced(data.raw_content().clone());
                    }
                }
            },
            AppMsg::PrimaryClipboardEvent(message) => match message {
                clipboard::ClipboardMessage::Connected => {
                    info!("primary clipboard watcher connected");
                }
                clipboard::ClipboardMessage::Data(data) => {
                    if let Some(text_data) = data.get(&MimeType::new("text/plain".to_string())) {
                        if let Ok(text) = String::from_utf8(text_data.clone()) {
                            if self.config.selection_buffer_enabled {
                                // Always insert into the in-memory selection buffer
                                self.selection_buffer.push(text.clone());

                                if self.config.selection_buffer_sync_clipboard {
                                    // Set skip flag so the regular watcher doesn't persist to DB
                                    SKIP_NEXT_CLIPBOARD.store(true, atomic::Ordering::Release);

                                    // Copy to clipboard for immediate Ctrl+V
                                    if let Err(e) = wl_copy("text/plain", text_data) {
                                        // Clear skip flag on failure
                                        SKIP_NEXT_CLIPBOARD.store(false, atomic::Ordering::Release);
                                        error!("primary sync: clipboard copy failed: {e}");
                                    }
                                }
                                // If sync_clipboard is false, text only goes to buffer (no copy)
                            } else {
                                // Selection buffer disabled — old behavior: copy to clipboard
                                if let Err(e) = wl_copy("text/plain", text_data) {
                                    error!("primary sync: clipboard copy failed: {e}");
                                }
                            }
                        }
                    }
                }
                clipboard::ClipboardMessage::EmptyKeyboard => {
                    // Primary selection cleared — nothing to do
                }
                clipboard::ClipboardMessage::Error(e) => {
                    error!("primary clipboard: {e}");
                }
            },
            AppMsg::Copy(id) => {
                self.current_entry_id = Some(id);
                let task = match self.db.get_from_id(id) {
                    Some(data) => copy_iced(data.raw_content().clone()),
                    None => {
                        error!("id not found");
                        Task::none()
                    }
                };

                return Task::batch([task, self.close_popup()]);
            }

            AppMsg::CopySpecial(data) => {
                return copy_iced(data);
            }
            AppMsg::Clear => {
                if let Some(op) = self.db.clear_memory() {
                    return spawn_db_persist(self.db.db_path(), op);
                }
            }
            AppMsg::RetryConnectingClipboard => {
                self.clipboard_state = ClipboardState::Init;
            }
            AppMsg::Navigation(message) => {
                match message {
                EventMsg::Event(e) => {
                    let message = match e {
                        Named::Enter => EventMsg::Enter,
                        Named::Escape => EventMsg::Quit,
                        Named::ArrowDown if !self.config.horizontal => EventMsg::Next,
                        Named::ArrowUp if !self.config.horizontal => EventMsg::Previous,
                        Named::ArrowLeft if self.config.horizontal => EventMsg::Previous,
                        Named::ArrowRight if self.config.horizontal => EventMsg::Next,
                        _ => EventMsg::None,
                    };

                    return task_message(AppMsg::Navigation(message));
                }
                EventMsg::Next => {
                    return self.focus_next();
                }
                EventMsg::Previous => {
                    return self.focus_previous();
                }
                EventMsg::Enter => {
                    if matches!(
                        self.popup,
                        Some(Popup {
                            kind: PopupKind::Popup | PopupKind::Favorites,
                            ..
                        })
                    ) && let Some(data) = self.db.get(self.focused)
                    {
                        return Task::batch([
                            copy_iced(data.raw_content().clone()),
                            self.close_popup(),
                        ]);
                    }
                }
                EventMsg::Quit => {
                    // Layer surface popups handle dismissal via click-outside
                    // or Escape key, not Unfocused events (which fire spuriously
                    // when the layer surface is created/configured).
                    if self.suppress_unfocus {
                        // Don't consume the flag — keep suppressing while
                        // the layer surface popup is open.
                    } else {
                        return self.close_popup();
                    }
                }
                EventMsg::None => {}
            }
            }
            AppMsg::Db(inner) => {
                if let Err(err) = block_on(self.db.handle_message(inner)) {
                    error!("{err}");
                }
            }
            AppMsg::ReturnToClipboard => {
                self.qr_code.take();
            }
            AppMsg::Config(msg) => match msg {
                ConfigMsg::PrivateMode(private_mode) => {
                    config_set!(private_mode, private_mode);
                    PRIVATE_MODE.store(private_mode, atomic::Ordering::Relaxed);
                }
                ConfigMsg::Horizontal(horizontal) => {
                    config_set!(horizontal, horizontal);
                }
                ConfigMsg::UniqueSession(unique_session) => {
                    config_set!(unique_session, unique_session);
                }
                ConfigMsg::SelectionBufferEnabled(enabled) => {
                    config_set!(selection_buffer_enabled, enabled);
                    SELECTION_BUFFER_ENABLED.store(enabled, atomic::Ordering::Relaxed);
                }
                ConfigMsg::SelectionBufferSyncClipboard(sync) => {
                    config_set!(selection_buffer_sync_clipboard, sync);
                }
            },
            AppMsg::NextPage => {
                self.page += 1;
                self.focused = self.page * self.config.maximum_entries_by_page.get() as usize;
            }
            AppMsg::PreviousPage => {
                if self.page > 0 {
                    self.page -= 1;
                    self.focused = self.page * self.config.maximum_entries_by_page.get() as usize;
                }
            }
            AppMsg::ToggleFavoritesFilter => {
                self.show_favorites_only = !self.show_favorites_only;
                self.page = 0;
                self.focused = 0;
            }
            AppMsg::ContextMenu(msg) => match msg {
                ContextMenuMsg::RemoveFavorite(entry) => {
                    if let Err(err) = block_on(self.db.remove_favorite(entry)) {
                        error!("{err}");
                    }
                }
                ContextMenuMsg::AddFavorite(entry) => {
                    // If already a favorite, this is a "Rename" action
                    if let Some(e) = self.db.get_from_id(entry) {
                        if e.is_favorite() {
                            // Start rename flow: pre-fill with existing title
                            let existing_title = e.favorite_title().unwrap_or("").to_string();
                            let content_for_ai = e
                                .preferred_content(&self.preferred_mime_types_regex)
                                .and_then(|(_, c)| {
                                    if let Content::Text(text) = c {
                                        Some(text.to_string())
                                    } else {
                                        None
                                    }
                                });
                            self.favoriting_state = Some(FavoritingState {
                                entry_id: entry,
                                title_input: existing_title,
                                phase: FavoritingPhase::Editing,
                            });
                            // Don't auto-suggest for rename — user already has a title
                            let _ = content_for_ai;
                            return Task::none();
                        }
                    }
                    // Not a favorite — start the add-favorite flow with title input
                    return task_message(AppMsg::BeginFavorite(entry));
                }
                ContextMenuMsg::Edit(id) => {
                    let edit_info = self.db.get_from_id(id).and_then(|entry| {
                        if let Some(((mime, _), Content::Text(text))) =
                            entry.preferred_content(&self.preferred_mime_types_regex)
                        {
                            Some((text.to_string(), mime.to_string()))
                        } else {
                            None
                        }
                    });
                    if let Some((text, mime)) = edit_info {
                        return self.open_editor_process(id, &text, mime);
                    }
                }
                ContextMenuMsg::ShowQrCode(id) => {
                    match self.db.get_from_id(id) {
                        Some(entry) => {
                            if let Some(((_, content), _)) =
                                entry.preferred_content(&self.preferred_mime_types_regex)
                            {
                                // todo: handle better this error
                                if content.len() < 700 {
                                    match qr_code::Data::new(content) {
                                        Ok(s) => {
                                            self.qr_code.replace(Ok(s));
                                        }
                                        Err(e) => {
                                            error!("{e}");
                                            self.qr_code.replace(Err(()));
                                        }
                                    }
                                } else {
                                    error!("qr code to long: {}", content.len());
                                    self.qr_code.replace(Err(()));
                                }
                            }
                        }
                        None => error!("id not found"),
                    }
                }
                ContextMenuMsg::Delete(id) => {
                    // If editing this entry, tell editor to close without saving
                    if self
                        .editor
                        .as_ref()
                        .is_some_and(|e| e.entry_id == id)
                    {
                        self.send_to_editor(&editor_ipc::AppToEditor::EntryDeleted);
                    }
                    if let Some(op) = self.db.delete_from_memory(id) {
                        return spawn_db_persist(self.db.db_path(), op);
                    }
                }
            },
            AppMsg::LinkClicked(url) => {
                info!("open: {url}");
                if let Err(e) = open::that(url.as_str()) {
                    error!("{e}");
                }
            }
            AppMsg::EditLatest => {
                self.last_quit = None;
                // Find the most recent text entry (pure chronological, ignoring favorite order)
                let edit_info = self
                    .db
                    .chronological_iter()
                    .find(|e| {
                        matches!(
                            e.preferred_content(&self.preferred_mime_types_regex),
                            Some((_, Content::Text(_)))
                        )
                    })
                    .and_then(|entry| {
                        if let Some(((mime, _), Content::Text(text))) =
                            entry.preferred_content(&self.preferred_mime_types_regex)
                        {
                            Some((entry.id(), text.to_string(), mime.to_string()))
                        } else {
                            None
                        }
                    });
                if let Some((id, text, mime)) = edit_info {
                    return self.open_editor_process(id, &text, mime);
                }
            }
            AppMsg::EditorEvent(msg) => {
                eprintln!("[applet] EditorEvent: {msg:?}");
                match msg {
                    EditorToApp::Ready => {}
                    EditorToApp::SaveAsNew { content } => {
                        eprintln!("[applet] SaveAsNew content_len={}", content.len());
                        if content.trim().is_empty() {
                            // Empty content — delete the original entry
                            if let Some(editor) = &self.editor {
                                let entry_id = editor.entry_id;
                                eprintln!("[applet] SaveAsNew: empty content, deleting entry {entry_id}");
                                if let Some(op) = self.db.delete_from_memory(entry_id) {
                                    return spawn_db_persist(self.db.db_path(), op);
                                }
                            }
                        } else {
                            let mut data = MimeDataMap::new();
                            data.insert(MimeType::new("text/plain".to_string()), content.as_bytes().to_vec());
                            if let Some(op) = self.db.insert_to_memory(data.clone()) {
                                eprintln!("[applet] SaveAsNew: inserted as new entry");
                                let persist = spawn_db_persist(self.db.db_path(), op);
                                return Task::batch([copy_iced(data), persist]);
                            }
                        }
                    }
                    EditorToApp::UpdateExisting { content } => {
                        eprintln!("[applet] UpdateExisting content_len={}", content.len());
                        if let Some(editor) = &self.editor {
                            let entry_id = editor.entry_id;
                            if content.trim().is_empty() {
                                // Empty content — delete the entry
                                eprintln!("[applet] UpdateExisting: empty content, deleting entry {entry_id}");
                                if let Some(op) = self.db.delete_from_memory(entry_id) {
                                    return spawn_db_persist(self.db.db_path(), op);
                                }
                            } else {
                                let mut data = MimeDataMap::new();
                                data.insert(MimeType::new("text/plain".to_string()), content.as_bytes().to_vec());
                                match block_on(self.db.update_content(entry_id, data.clone())) {
                                    Ok(_new_id) => {
                                        eprintln!("[applet] UpdateExisting: updated entry {entry_id}");
                                        return copy_iced(data);
                                    }
                                    Err(e) => error!("failed to update entry: {e}"),
                                }
                            }
                        }
                    }
                    EditorToApp::Closed => {
                        eprintln!("[applet] Editor sent Closed (no changes)");
                    }
                }
            },
            AppMsg::EditorProcessExited => {
                eprintln!("[applet] EditorProcessExited");
                if let Some(mut editor) = self.editor.take() {
                    let _ = editor.child.wait();
                }
            }
            AppMsg::DbusFavorites => {
                self.last_quit = None;
                return self.toggle_popup_ext(PopupKind::Favorites, true);
            }
            AppMsg::DbusToggleSelections => {
                self.last_quit = None;
                return self.toggle_popup_ext(PopupKind::Selections, true);
            }
            AppMsg::SelectionSearch(query) => {
                self.selection_buffer.search(query);
            }
            AppMsg::SelectionCopy(id) => {
                if let Some(entry) = self.selection_buffer.get_by_id(id) {
                    let text_data = entry.text.as_bytes().to_vec();
                    if let Err(e) = wl_copy("text/plain", &text_data) {
                        error!("selection copy: clipboard copy failed: {e}");
                    }
                }
                // Do NOT close popup — multi-grab workflow
            }
            AppMsg::SelectionToFavorite(id) => {
                if let Some(entry) = self.selection_buffer.get_by_id(id) {
                    let text = entry.text.clone();
                    let mut data = MimeDataMap::new();
                    data.insert(MimeType::new("text/plain".to_string()), text.as_bytes().to_vec());
                    if let Some(op) = self.db.insert_to_memory(data) {
                        // Get the newly inserted entry's ID (first in chronological order)
                        let new_id = self.db.chronological_iter().next().map(|e| e.id());
                        if let Some(new_id) = new_id {
                            if let Err(e) = block_on(self.db.add_favorite(new_id, None)) {
                                error!("failed to add favorite: {e}");
                            }
                        }
                        return spawn_db_persist(self.db.db_path(), op);
                    }
                }
            }
            AppMsg::ClearSelections => {
                self.selection_buffer.clear();
            }
            AppMsg::DbusListFavorites { reply } => {
                let summaries: Vec<FavoriteSummary> = self
                    .db
                    .iter()
                    .filter(|e| e.is_favorite())
                    .map(|entry| {
                        let preview = match entry
                            .preferred_content(&self.preferred_mime_types_regex)
                        {
                            Some((_, Content::Text(text))) => {
                                sanitize_preview(text, 100)
                            }
                            Some((_, Content::UriList(uris))) => {
                                sanitize_preview(&uris.join(" "), 100)
                            }
                            Some((_, Content::Image(_))) => "[image]".to_string(),
                            None => "[unknown]".to_string(),
                        };
                        FavoriteSummary {
                            id: entry.id().as_i64(),
                            title: entry.favorite_title().map(|s| s.to_string()),
                            preview,
                        }
                    })
                    .collect();
                reply.reply(summaries);
            }
            AppMsg::BeginFavorite(id) => {
                // Get entry content for AI suggestion
                let content_for_ai = self.db.get_from_id(id).and_then(|entry| {
                    if let Some((_, Content::Text(text))) =
                        entry.preferred_content(&self.preferred_mime_types_regex)
                    {
                        Some(text.to_string())
                    } else {
                        None
                    }
                });

                self.favoriting_state = Some(FavoritingState {
                    entry_id: id,
                    title_input: String::new(),
                    phase: if content_for_ai.is_some() && ai::get_api_key().is_some() {
                        FavoritingPhase::Suggesting
                    } else {
                        FavoritingPhase::Editing
                    },
                });

                // Kick off AI suggestion if we have text content and an API key
                if let Some(text) = content_for_ai {
                    if ai::get_api_key().is_some() {
                        return Task::perform(
                            async move { ai::suggest_title(&text).await },
                            move |suggestion| {
                                cosmic::action::app(AppMsg::TitleSuggested(id, suggestion))
                            },
                        );
                    }
                }
            }
            AppMsg::TitleSuggested(id, suggestion) => {
                if let Some(state) = &mut self.favoriting_state {
                    if state.entry_id == id {
                        state.title_input = suggestion.unwrap_or_default();
                        state.phase = FavoritingPhase::Editing;
                    }
                }
            }
            AppMsg::FavoriteTitleInput(text) => {
                if let Some(state) = &mut self.favoriting_state {
                    state.title_input = text;
                }
            }
            AppMsg::ConfirmFavorite(id, title) => {
                let already_fav = self.db.get_from_id(id).is_some_and(|e| e.is_favorite());
                if !already_fav {
                    if let Err(err) = block_on(self.db.add_favorite(id, None)) {
                        error!("{err}");
                    }
                }
                let title = title.filter(|t| !t.trim().is_empty());
                if let Err(err) = block_on(self.db.set_favorite_title(id, title)) {
                    error!("{err}");
                }
                self.favoriting_state = None;
            }
            AppMsg::CancelFavorite => {
                self.favoriting_state = None;
            }
            AppMsg::SetFavoriteTitle(id, title) => {
                let title = if title.trim().is_empty() { None } else { Some(title) };
                if let Err(err) = block_on(self.db.set_favorite_title(id, title)) {
                    error!("{err}");
                }
            }
            AppMsg::SuggestTitle(id) => {
                // Re-trigger AI suggestion for an existing favorite
                let content_for_ai = self.db.get_from_id(id).and_then(|entry| {
                    if let Some((_, Content::Text(text))) =
                        entry.preferred_content(&self.preferred_mime_types_regex)
                    {
                        Some(text.to_string())
                    } else {
                        None
                    }
                });
                if let Some(text) = content_for_ai {
                    if let Some(state) = &mut self.favoriting_state {
                        state.phase = FavoritingPhase::Suggesting;
                    }
                    return Task::perform(
                        async move { ai::suggest_title(&text).await },
                        move |suggestion| {
                            cosmic::action::app(AppMsg::TitleSuggested(id, suggestion))
                        },
                    );
                }
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let icon = self
            .core
            .applet
            .icon_button(constcat::concat!(APPID, "-symbolic"))
            .on_press(AppMsg::TogglePopup);

        MouseArea::new(icon)
            .on_right_release(AppMsg::ToggleQuickSettings)
            .into()
    }

    fn view_window(&self, _id: Id) -> Element<'_, Self::Message> {
        // Render the cursor capture overlay as a transparent mouse_area
        if let Some(capture) = &self.cursor_capture {
            let target = capture.target_popup;
            return MouseArea::new(
                Space::new(cosmic::iced::Length::Fill, cosmic::iced::Length::Fill),
            )
            .on_move(move |pos| AppMsg::CursorCaptured {
                position: pos,
                target,
            })
            .into();
        }

        let Some(popup) = &self.popup else {
            return Space::new(0, 0).into();
        };

        let view = match &popup.kind {
            PopupKind::Popup => self.popup_view(),
            PopupKind::Favorites => self.favorites_view(),
            PopupKind::Selections => self.selections_view(),
            PopupKind::QuickSettings => self.quick_settings_view(),
        };

        // Layer surface popups are fullscreen overlays — position the content
        // at the cursor (if captured) or center it, and dismiss on click outside.
        // We skip popup_container() because its Autosize widget shrinks the
        // surface to content size (~360px).
        if popup.is_layer_surface {
            let cosmic_theme = self.core.system_theme().cosmic();
            let corners = cosmic_theme.corner_radii;
            let styled = cosmic::widget::container(view)
                .style(move |theme: &cosmic::Theme| {
                    let cosmic = theme.cosmic();
                    cosmic::iced_widget::container::Style {
                        text_color: Some(cosmic.background.on.into()),
                        background: Some(cosmic::iced::Color::from(cosmic.background.base).into()),
                        border: cosmic::iced::Border {
                            radius: corners.radius_m.into(),
                            width: 1.0,
                            color: cosmic.background.divider.into(),
                        },
                        shadow: Default::default(),
                        icon_color: Some(cosmic.background.on.into()),
                    }
                })
                .width(cosmic::iced::Length::Fixed(match popup.kind {
                    PopupKind::Favorites => 1200.0,
                    _ => 400.0,
                }))
                .height(cosmic::iced::Length::Shrink)
                .max_height(530.0);

            let outer = if let Some(pos) = self.popup_position {
                // Clamp so the popup stays fully on-screen when cursor is near an edge.
                let popup_w = match popup.kind {
                    PopupKind::Favorites => 1200.0,
                    _ => 400.0,
                };
                let popup_h = 530.0;
                let (left, top) = if let Some((sw, sh)) = self.screen_size {
                    (
                        pos.x.min(sw - popup_w).max(0.0),
                        pos.y.min(sh - popup_h).max(0.0),
                    )
                } else {
                    (pos.x, pos.y)
                };

                cosmic::widget::container(styled)
                    .width(cosmic::iced::Length::Fill)
                    .height(cosmic::iced::Length::Fill)
                    .align_x(cosmic::iced::Alignment::Start)
                    .align_y(cosmic::iced::Alignment::Start)
                    .padding(cosmic::iced::Padding {
                        top,
                        right: 0.0,
                        bottom: 0.0,
                        left,
                    })
            } else {
                // Center fallback (no cursor position captured)
                cosmic::widget::container(styled)
                    .width(cosmic::iced::Length::Fill)
                    .height(cosmic::iced::Length::Fill)
                    .align_x(cosmic::iced::Alignment::Center)
                    .align_y(cosmic::iced::Alignment::Center)
            };

            return MouseArea::new(outer)
                .on_press(AppMsg::ClosePopup)
                .into();
        }

        self.core.applet.popup_container(view).into()
    }
    fn subscription(&self) -> Subscription<Self::Message> {
        pub fn db_sub() -> Subscription<DbMessage> {
            cosmic::iced::time::every(Duration::from_millis(5000)).map(|_| DbMessage::CheckUpdate)
        }

        let mut subscriptions = vec![
            config::sub(),
            navigation::sub().map(AppMsg::Navigation),
            db_sub().map(AppMsg::Db),
            ipc::dbus_toggle_subscription(),
            cosmic::iced_futures::event::listen_with(|event, _, _| {
                use cosmic::iced::event::{PlatformSpecific, wayland};
                let cosmic::iced::Event::PlatformSpecific(PlatformSpecific::Wayland(
                    wayland::Event::Output(output_event, _),
                )) = event
                else {
                    return None;
                };
                let info = match output_event {
                    wayland::OutputEvent::Created(Some(info)) => info,
                    wayland::OutputEvent::InfoUpdate(info) => info,
                    _ => return None,
                };
                info.logical_size
                    .map(|(w, h)| AppMsg::OutputSize(w as f32, h as f32))
            }),
        ];

        // Editor process IPC subscription
        if let Some(editor) = &self.editor {
            let rx_arc = editor.stdout_rx.clone();
            let session_id = editor.session_id;
            subscriptions.push(Subscription::run_with_id(
                session_id,
                cosmic::iced::stream::channel(8, move |mut output| async move {
                    use cosmic::iced::futures::SinkExt;
                    let rx_opt = rx_arc.lock().unwrap().take();
                    let Some(mut rx) = rx_opt else {
                        futures::future::pending::<()>().await;
                        unreachable!();
                    };
                    loop {
                        match rx.recv().await {
                            Some(msg) => {
                                eprintln!("[applet-sub] Received from editor: {msg:?}");
                                output.send(AppMsg::EditorEvent(msg)).await.ok();
                            }
                            None => {
                                eprintln!("[applet-sub] Editor channel closed, sending EditorProcessExited");
                                output.send(AppMsg::EditorProcessExited).await.ok();
                                futures::future::pending::<()>().await;
                            }
                        }
                    }
                }),
            ));
        }

        if !self.clipboard_state.is_error() {
            subscriptions.push(Subscription::run(|| {
                clipboard::sub().map(AppMsg::ClipboardEvent)
            }));

            if self.config.selection_buffer_enabled {
                subscriptions.push(Subscription::run(|| {
                    clipboard::primary_sub().map(AppMsg::PrimaryClipboardEvent)
                }));
            }
        }

        Subscription::batch(subscriptions)
    }

    fn on_app_exit(&mut self) -> Option<Self::Message> {
        // Tell editor to close gracefully before applet exits
        if let Some(mut editor) = self.editor.take() {
            let _ = editor_ipc::write_frame(
                &mut editor.stdin_handle,
                &editor_ipc::AppToEditor::CloseRequested,
            );
            let _ = editor.child.wait();
        }

        if self.config.unique_session {
            // On close with unique_session, clear non-favorites synchronously
            // since we can't return a Task from dbus_activation.
            if let Err(err) = block_on(self.db.clear()) {
                error!("{err}");
            }
        }
        None
    }

    fn style(&self) -> Option<iced::runtime::Appearance> {
        Some(cosmic::applet::style())
    }
}

// used because wl_clipboard can't copy when zwlr_data_control_manager_v1 is not there
fn copy_iced(data: MimeDataMap) -> Task<AppMsg> {
    struct MimeDataMapN(MimeDataMap);

    impl AsMimeTypes for MimeDataMapN {
        fn available(&self) -> std::borrow::Cow<'static, [String]> {
            std::borrow::Cow::Owned(self.0.keys().map(|k| k.as_str().to_string()).collect())
        }

        fn as_bytes(&self, mime_type: &str) -> Option<std::borrow::Cow<'static, [u8]>> {
            self.0
                .get(&MimeType::new(mime_type.to_string()))
                .map(|d| std::borrow::Cow::Owned(d.clone()))
        }
    }

    cosmic::iced::clipboard::write_data(MimeDataMapN(data))
}
