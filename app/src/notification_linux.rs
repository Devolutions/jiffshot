//! Desktop notifications for capture results while the editor is hidden.

use std::collections::HashMap;
use std::time::Duration;

const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy)]
pub(super) enum Urgency {
    Normal,
    Critical,
}

pub(super) fn send(summary: &'static str, body: String, urgency: Urgency) {
    if let Err(error) = std::thread::Builder::new()
        .name("jiffshot-notification".to_owned())
        .spawn(move || {
            if let Err(error) = futures_lite::future::block_on(send_async(summary, &body, urgency))
            {
                eprintln!("Could not deliver desktop notification ({summary}): {error}");
            }
        })
    {
        eprintln!("Could not start desktop notification ({summary}): {error}");
    }
}

async fn send_async(summary: &str, body: &str, urgency: Urgency) -> Result<(), String> {
    futures_lite::future::or(
        async {
            let connection = zbus::Connection::session()
                .await
                .map_err(|error| format!("could not connect to the session bus: {error}"))?;
            let proxy = zbus::Proxy::new(
                &connection,
                "org.freedesktop.Notifications",
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
            )
            .await
            .map_err(|error| format!("could not connect to the notification service: {error}"))?;
            let hints: HashMap<&str, zbus::zvariant::Value<'_>> = [(
                "urgency",
                zbus::zvariant::Value::from(match urgency {
                    Urgency::Normal => 1_u8,
                    Urgency::Critical => 2_u8,
                }),
            )]
            .into_iter()
            .collect();
            let body = escape_markup(body);
            let _: u32 = proxy
                .call(
                    "Notify",
                    &(
                        "Jiffshot",
                        0_u32,
                        "com.devolutions.Jiffshot",
                        summary,
                        body.as_str(),
                        Vec::<&str>::new(),
                        hints,
                        -1_i32,
                    ),
                )
                .await
                .map_err(|error| {
                    format!("the notification service rejected the message: {error}")
                })?;
            Ok(())
        },
        async {
            async_io::Timer::after(NOTIFICATION_TIMEOUT).await;
            Err("the notification service did not respond".to_owned())
        },
    )
    .await
}

fn escape_markup(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_text_cannot_inject_markup() {
        assert_eq!(
            escape_markup("Saved <image> & \"notes\" in 'captures'."),
            "Saved &lt;image&gt; &amp; &quot;notes&quot; in &apos;captures&apos;."
        );
        assert_eq!(escape_markup("plain\ntext"), "plain\ntext");
    }
}
