use std::{
    sync::{
        Arc,
        atomic::{self},
    },
    time::Duration,
};

use cosmic::iced::{futures::SinkExt, stream::channel};
use futures::{Stream, future::join_all};
use itertools::Itertools;
use tokio::{io::AsyncReadExt, sync::mpsc};

use crate::{clipboard_watcher, config::{PRIVATE_MODE, SELECTION_BUFFER_ENABLED}, db::{MimeDataMap, MimeType}};

#[derive(Debug, Clone)]
pub enum ClipboardMessage {
    Connected,
    Data(MimeDataMap),
    /// Means that the source was closed, or the compurer just started
    /// This means the clipboard manager must become the source, by providing the last entry
    EmptyKeyboard,
    Error(ClipboardError),
}

#[non_exhaustive]
#[derive(Debug, Clone, thiserror::Error)]
pub enum ClipboardError {
    #[error(transparent)]
    Watch(Arc<clipboard_watcher::Error>),
}

enum WatchRes<I> {
    Some(I),
    None,
    Err(clipboard_watcher::Error),
}

/// Read all offered MIME types from clipboard pipes with a 500ms timeout each.
async fn read_pipes(
    pipes: impl IntoIterator<Item = (String, impl tokio::io::AsyncRead + Unpin)>,
    label: &str,
) -> MimeDataMap {
    let reads = pipes.into_iter().map(|(mime_type, mut pipe)| {
        let label = label.to_string();
        async move {
            let mut contents = Vec::new();
            match tokio::time::timeout(
                Duration::from_millis(500),
                pipe.read_to_end(&mut contents),
            )
            .await
            {
                Ok(Ok(len)) if len > 0 => Some((MimeType::new(mime_type), contents)),
                Ok(Ok(_)) => {
                    debug!("{label}data is empty: {mime_type}");
                    None
                }
                Ok(Err(e)) => {
                    warn!("{label}read error on pipe: {mime_type} {e}");
                    None
                }
                Err(e) => {
                    warn!("{label}read timeout on pipe: {mime_type} {e}");
                    None
                }
            }
        }
    });
    join_all(reads).await.into_iter().flatten().collect()
}

/// Shared recv-loop for both clipboard and primary-selection watchers.
async fn clipboard_recv_loop(
    mut rx: mpsc::Receiver<WatchRes<impl IntoIterator<Item = (String, impl tokio::io::AsyncRead + Unpin)>>>,
    mut output: impl SinkExt<ClipboardMessage> + Unpin,
    label: &str,
) {
    loop {
        match rx.recv().await {
            Some(WatchRes::Some(res)) => {
                let data = read_pipes(res, label).await;
                if !data.is_empty() {
                    if label.is_empty() {
                        let mimes = data
                            .iter()
                            .map(|(m, d)| (m.to_string(), d.len()))
                            .collect_vec();
                        debug!("send mime types to db: {mimes:?}");
                    }
                    let _ = output.send(ClipboardMessage::Data(data)).await;
                }
            }
            Some(WatchRes::None) => {
                debug!("{label}empty clipboard");
                let _ = output.send(ClipboardMessage::EmptyKeyboard).await;
            }
            Some(WatchRes::Err(e)) => {
                let _ = output
                    .send(ClipboardMessage::Error(ClipboardError::Watch(e.into())))
                    .await;
                std::future::pending::<()>().await;
            }
            None => {
                std::future::pending::<()>().await;
            }
        }
    }
}

pub fn sub() -> impl Stream<Item = ClipboardMessage> {
    channel(50, move |mut output| {
        async move {
            match clipboard_watcher::Watcher::init(false) {
                Ok(mut clipboard_watcher) => {
                    let (tx, rx) = mpsc::channel(5);

                    // JoinHandle intentionally dropped: the blocking task communicates
                    // errors back through `tx` (as WatchRes::Err), and if it panics the
                    // channel closes, which the recv loop below handles as the None arm.
                    tokio::task::spawn_blocking(move || {
                        loop {
                            debug!("start watching");
                            match clipboard_watcher
                                .start_watching(clipboard_watcher::Seat::Unspecified)
                            {
                                Ok(res) => {
                                    if !PRIVATE_MODE.load(atomic::Ordering::Relaxed) {
                                        if tx.blocking_send(WatchRes::Some(res)).is_err() {
                                            break;
                                        }
                                    } else {
                                        info!("private mode")
                                    }
                                }
                                Err(e) => match e {
                                    clipboard_watcher::Error::ClipboardEmpty => {
                                        if tx.blocking_send(WatchRes::None).is_err() {
                                            break;
                                        }
                                    }
                                    _ => {
                                        let _ = tx.blocking_send(WatchRes::Err(e));
                                        break;
                                    }
                                },
                            }
                        }
                    });
                    let _ = output.send(ClipboardMessage::Connected).await;
                    clipboard_recv_loop(rx, output, "").await;
                }

                Err(e) => {
                    let _ = output
                        .send(ClipboardMessage::Error(ClipboardError::Watch(e.into())))
                        .await;
                    std::future::pending::<()>().await;
                }
            };
        }
    })
}

pub fn primary_sub() -> impl Stream<Item = ClipboardMessage> {
    channel(50, move |mut output| {
        async move {
            match clipboard_watcher::Watcher::init(true) {
                Ok(mut clipboard_watcher) => {
                    let (tx, rx) = mpsc::channel(5);

                    // JoinHandle intentionally dropped: see comment in sub() above.
                    tokio::task::spawn_blocking(move || {
                        loop {
                            debug!("primary: start watching");
                            match clipboard_watcher
                                .start_watching(clipboard_watcher::Seat::Unspecified)
                            {
                                Ok(res) => {
                                    if !SELECTION_BUFFER_ENABLED.load(atomic::Ordering::Relaxed) {
                                        info!("primary: selection buffer disabled");
                                    } else if PRIVATE_MODE.load(atomic::Ordering::Relaxed) {
                                        info!("primary: private mode");
                                    } else if tx.blocking_send(WatchRes::Some(res)).is_err() {
                                        break;
                                    }
                                }
                                Err(e) => match e {
                                    clipboard_watcher::Error::ClipboardEmpty => {
                                        if tx.blocking_send(WatchRes::None).is_err() {
                                            break;
                                        }
                                    }
                                    _ => {
                                        let _ = tx.blocking_send(WatchRes::Err(e));
                                        break;
                                    }
                                },
                            }
                        }
                    });
                    let _ = output.send(ClipboardMessage::Connected).await;
                    clipboard_recv_loop(rx, output, "primary: ").await;
                }

                Err(e) => {
                    let _ = output
                        .send(ClipboardMessage::Error(ClipboardError::Watch(e.into())))
                        .await;
                    std::future::pending::<()>().await;
                }
            };
        }
    })
}

// unfold experiment, doesn't work with channel, but better error management
/*

enum State {
    Init,
    Idle(paste_watch::Watcher),
    Error,
}

pub fn sub2() -> Subscription<Message> {
    struct Connect;

    subscription::unfold(
        std::any::TypeId::of::<Connect>(),
        State::Init,
        |state| {

            async move {
                match state {
                    State::Init => {
                        match paste_watch::Watcher::init(paste_watch::ClipboardType::Regular) {
                            Ok(watcher) => {
                                return (Message::Connected, State::Idle(watcher));
                            }
                            Err(e) => {
                                return (Message::Error(e), State::Error);
                            }
                        }
                    }
                    State::Idle(watcher) => {

                        let e = watcher.start_watching2(
                            paste_watch::Seat::Unspecified,
                            paste_watch::MimeType::Any,
                        );


                        todo!()
                    }

                    State::Error => {
                        // todo
                        todo!()
                    }
                }
            }
        },
    )
}

 */
