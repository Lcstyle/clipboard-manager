//! D-Bus IPC for toggling the clipboard manager popup from external commands.
//!
//! The applet registers a D-Bus service on the session bus. When `--toggle` is
//! invoked, it calls the `Toggle` method on the running service instance, which
//! sends a message through the iced subscription to open/close the popup.

use crate::message::AppMsg;
use cosmic::iced_futures::Subscription;

const BUS_NAME: &str = "io.github.cosmic_utils.ClipboardManager";
const OBJECT_PATH: &str = "/io/github/cosmic_utils/ClipboardManager";
const INTERFACE_NAME: &str = "io.github.cosmic_utils.ClipboardManager1";

/// D-Bus service that receives Toggle calls and forwards them via a channel.
struct ToggleService {
    tx: tokio::sync::mpsc::Sender<()>,
}

#[zbus::interface(name = "io.github.cosmic_utils.ClipboardManager1")]
impl ToggleService {
    async fn toggle(&self) {
        let _ = self.tx.send(()).await;
    }
}

/// Subscription that registers the D-Bus service and listens for Toggle calls.
pub fn dbus_toggle_subscription() -> Subscription<AppMsg> {
    use cosmic::iced::futures::SinkExt;
    use cosmic::iced::stream::channel;

    Subscription::run_with_id(
        "dbus_toggle",
        channel(1, |mut output| async move {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
            let service = ToggleService { tx };

            let conn = match zbus::connection::Builder::session()
                .and_then(|b| b.name(BUS_NAME))
                .and_then(|b| b.serve_at(OBJECT_PATH, service))
            {
                Ok(builder) => match builder.build().await {
                    Ok(conn) => conn,
                    Err(e) => {
                        error!("D-Bus connection failed: {e}");
                        futures::future::pending::<()>().await;
                        unreachable!();
                    }
                },
                Err(e) => {
                    error!("D-Bus builder failed: {e}");
                    futures::future::pending::<()>().await;
                    unreachable!();
                }
            };

            // Keep connection alive for the lifetime of the subscription
            let _conn = conn;

            loop {
                match rx.recv().await {
                    Some(()) => {
                        output.send(AppMsg::DbusToggle).await.ok();
                    }
                    None => {
                        // Channel closed, wait forever to avoid busy loop
                        futures::future::pending::<()>().await;
                    }
                }
            }
        }),
    )
}

/// Send a Toggle call to the running applet via D-Bus (blocking, for CLI use).
pub fn send_toggle() -> Result<(), Box<dyn std::error::Error>> {
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(
        &connection,
        BUS_NAME,
        OBJECT_PATH,
        INTERFACE_NAME,
    )?;
    proxy.call_method("Toggle", &())?;
    Ok(())
}
