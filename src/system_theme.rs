//! The desktop's light/dark preference. winit reports it on Windows and
//! macOS, but on Linux `system_theme()` is always `None` and `ThemeChanged`
//! never fires, so egui's "System" theme would always be dark there. Read it
//! from the XDG desktop portal instead (GNOME, KDE and others implement it) and
//! follow its `SettingChanged` signal.

use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

const UNKNOWN: u8 = 0;
const DARK: u8 = 1;
const LIGHT: u8 = 2;

pub struct SystemTheme(Arc<AtomicU8>);

impl SystemTheme {
    pub fn watch(ctx: &egui::Context) -> Self {
        let theme = Arc::new(AtomicU8::new(UNKNOWN));
        #[cfg(target_os = "linux")]
        portal::spawn(ctx.clone(), theme.clone());
        #[cfg(not(target_os = "linux"))]
        let _ = ctx;
        Self(theme)
    }

    /// The desktop's theme, or `None` to keep what winit reports.
    pub fn get(&self) -> Option<egui::Theme> {
        match self.0.load(Ordering::Relaxed) {
            DARK => Some(egui::Theme::Dark),
            LIGHT => Some(egui::Theme::Light),
            _ => None,
        }
    }
}

#[cfg(target_os = "linux")]
mod portal {
    use std::{
        future::poll_fn,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicU8, Ordering},
            mpsc,
        },
        time::Duration,
    };

    use zbus::{Connection, OwnedVariant, Proxy, export::futures_core::Stream};

    use super::{DARK, LIGHT, UNKNOWN};

    const NAMESPACE: &str = "org.freedesktop.appearance";
    const KEY: &str = "color-scheme";
    /// How long startup waits for the first answer, so the first frame is
    /// already drawn in the right theme. A portal being started on demand
    /// can take longer; the theme then switches once it answers.
    const FIRST_ANSWER: Duration = Duration::from_millis(300);

    pub fn spawn(ctx: egui::Context, theme: Arc<AtomicU8>) {
        let (answered, first_answer) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("system-theme".into())
            .spawn(move || {
                let result = zbus::block_on(watch(&ctx, &theme, &answered));
                // Unblock startup even if the portal never answered.
                let _ = answered.send(());
                if let Err(err) = result {
                    eprintln!("system theme: {err}");
                }
            });
        if spawned.is_ok() {
            let _ = first_answer.recv_timeout(FIRST_ANSWER);
        }
    }

    async fn watch(
        ctx: &egui::Context,
        theme: &AtomicU8,
        answered: &mpsc::Sender<()>,
    ) -> zbus::Result<()> {
        let connection = Connection::session().await?;
        let settings = Proxy::new(
            &connection,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings",
        )
        .await?;
        // Subscribe before reading, so a change in between isn't missed.
        let mut changes = settings.receive_signal("SettingChanged").await?;

        // `ReadOne` is portal version 2; `Read` wraps the value in one more
        // variant, which `downcast_ref` unwraps.
        let value: zbus::Result<OwnedVariant> =
            match settings.call("ReadOne", &(NAMESPACE, KEY)).await {
                Ok(value) => Ok(value),
                Err(_) => settings.call("Read", &(NAMESPACE, KEY)).await,
            };
        if let Ok(value) = value {
            theme.store(scheme(&value), Ordering::Relaxed);
        }
        let _ = answered.send(());

        while let Some(message) = poll_fn(|cx| Pin::new(&mut changes).poll_next(cx)).await {
            let Ok((namespace, key, value)) = message
                .body()
                .deserialize::<(String, String, OwnedVariant)>()
            else {
                continue;
            };
            if namespace == NAMESPACE && key == KEY {
                theme.store(scheme(&value), Ordering::Relaxed);
                ctx.request_repaint();
            }
        }
        Ok(())
    }

    /// 1 is "prefer dark", 2 "prefer light" and 0 "no preference", which is
    /// what GNOME reports for its default (light) style.
    fn scheme(value: &OwnedVariant) -> u8 {
        match value.downcast_ref::<u32>() {
            Ok(1) => DARK,
            Ok(0 | 2) => LIGHT,
            _ => UNKNOWN,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Needs a session bus with a desktop portal.
    #[test]
    #[ignore]
    fn reads_the_portal() {
        let theme = SystemTheme::watch(&egui::Context::default());
        assert!(theme.get().is_some(), "the portal didn't answer");
    }
}
