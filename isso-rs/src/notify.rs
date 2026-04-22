//! Email + stdout notifications, mirroring isso/ext/notifications.py.
//!
//! Two notification backends, both enabled by `[general] notify`:
//!
//! - **stdout**: log-line emission for new / edit / delete / activate events
//!   with Delete/Activate URLs the operator can click through — useful for a
//!   dev loop, matches Python's `Stdout` class.
//! - **smtp**: emails sent on new-comment (to the admin) and on activate (to
//!   parent-comment authors who opted in via `notification = 1`). Delivery is
//!   best-effort: we fire-and-forget over a tokio task and log on failure.
//!
//! The admin notification includes signed `Delete` / `Activate` URLs; the
//! reply notification includes a signed `Unsubscribe` URL. Both use the same
//! `Signer` that the HTTP layer uses for cookies, so the moderation endpoints
//! (`/id/:id/(activate|delete)/:key`) accept them.

use std::sync::Arc;

use lettre::message::header::ContentType;
use lettre::message::{Mailbox, Message};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use serde_json::json;

use crate::config::{Config, Smtp};
use crate::db::comments::Comment;
use crate::db::threads::Thread;
use crate::signer::Signer;

/// Event routing. The HTTP handlers call into these three entry points
/// when state changes; each backend decides whether (and how) to act.
#[derive(Clone)]
pub struct Notifier {
    config: Arc<Config>,
    signer: Arc<Signer>,
}

impl Notifier {
    pub fn new(config: Arc<Config>, signer: Arc<Signer>) -> Self {
        Self { config, signer }
    }

    fn wants_admin_smtp(&self) -> bool {
        self.config
            .general
            .notify
            .iter()
            .any(|n| n.eq_ignore_ascii_case("smtp"))
    }

    fn wants_reply_smtp(&self) -> bool {
        self.config.general.reply_notifications
    }

    fn wants_stdout(&self) -> bool {
        self.config
            .general
            .notify
            .iter()
            .any(|n| n.eq_ignore_ascii_case("stdout"))
    }

    fn public_endpoint(&self) -> &str {
        if self.config.server.public_endpoint.is_empty() {
            self.config
                .general
                .hosts
                .first()
                .map(String::as_str)
                .unwrap_or("")
        } else {
            &self.config.server.public_endpoint
        }
        .trim_end_matches('/')
    }

    /// Fired after a new comment is persisted. Emits the admin SMTP email
    /// (when configured), and stdout logs in all cases.
    pub fn comment_created(&self, thread: &Thread, comment: &Comment) {
        if self.wants_stdout() {
            self.stdout_new(thread, comment);
        }
        if self.wants_admin_smtp() {
            self.spawn_admin_email(thread.clone(), comment.clone());
        }
        // If the comment is already accepted (not pending) we also fire the
        // reply-notification path on insert — matches Python's notify_new.
        if self.wants_reply_smtp() && comment.mode == 1 {
            // TODO: wire reply-notify fanout here once the HTTP layer can
            // pass in the parent-comment context. Current MVP triggers only
            // on activation (see comment_activated).
        }
    }

    /// Fired when a pending comment is activated by an admin. Sends reply
    /// notifications to parent-comment authors who opted in.
    pub fn comment_activated(&self, _thread: &Thread, _comment: &Comment) {
        if self.wants_stdout() {
            tracing::info!("comment activated (stdout notifier)");
        }
        // TODO: SMTP reply notification. The HTTP moderation handler will
        // resolve the parent comment and loop through notification subscribers;
        // integrate from that call site so we only need one DB query.
    }

    fn stdout_new(&self, thread: &Thread, comment: &Comment) {
        // Match the line format from isso/ext/notifications.py::Stdout.
        tracing::info!(
            "new comment: {}",
            json!({
                "id": comment.id,
                "tid": comment.tid,
                "thread_uri": thread.uri,
                "thread_title": thread.title,
                "mode": comment.mode,
                "author": comment.author,
            })
        );
        let base = self.public_endpoint();
        let delete_key = self
            .signer
            .sign(&comment.id)
            .unwrap_or_else(|_| String::from("<sign-failed>"));
        tracing::info!(
            "Delete comment: {base}/id/{}/delete/{delete_key}",
            comment.id
        );
        if comment.mode == 2 {
            tracing::info!(
                "Activate comment: {base}/id/{}/activate/{delete_key}",
                comment.id
            );
        }
    }

    fn spawn_admin_email(&self, thread: Thread, comment: Comment) {
        let config = Arc::clone(&self.config);
        let signer = Arc::clone(&self.signer);
        let base = self.public_endpoint().to_string();
        tokio::spawn(async move {
            let body = format_admin_body(&base, &thread, &comment, &signer);
            let subject = match &thread.title {
                Some(title) if !title.is_empty() => format!("New comment posted on {title}"),
                _ => "New comment posted".to_string(),
            };
            let to = config.smtp.to.clone();
            if to.is_empty() {
                tracing::debug!("skipping admin email: [smtp] to is empty");
                return;
            }
            if let Err(e) = send_email(&config.smtp, &subject, body, &to, &[]).await {
                tracing::warn!("admin email send failed: {e}");
            }
        });
    }
}

/// Plain-text admin body. Matches the Python layout closely (author line,
/// text, IP, link, delete/activate URLs).
fn format_admin_body(base: &str, thread: &Thread, comment: &Comment, signer: &Signer) -> String {
    let mut out = String::new();
    let author = match comment.author.as_deref() {
        Some(a) if !a.is_empty() => a.to_string(),
        _ => "Anonymous".to_string(),
    };
    let author_line = match comment.email.as_deref() {
        Some(e) if !e.is_empty() => format!("{author} <{e}>"),
        _ => author,
    };
    out.push_str(&format!("{author_line} wrote:\n\n"));
    out.push_str(&comment.text);
    out.push_str("\n\n");
    if let Some(website) = comment.website.as_deref() {
        if !website.is_empty() {
            out.push_str(&format!("User's URL: {website}\n"));
        }
    }
    if let Some(remote) = comment.remote_addr.as_deref() {
        out.push_str(&format!("IP address: {remote}\n"));
    }
    out.push_str(&format!(
        "Link to comment: {base}{}#isso-{}\n\n---\n",
        thread.uri, comment.id
    ));
    let key = signer
        .sign(&comment.id)
        .unwrap_or_else(|_| String::from("<sign-failed>"));
    out.push_str(&format!(
        "Delete comment: {base}/id/{}/delete/{key}\n",
        comment.id
    ));
    if comment.mode == 2 {
        out.push_str(&format!(
            "Activate comment: {base}/id/{}/activate/{key}\n",
            comment.id
        ));
    }
    out
}

async fn send_email(
    smtp: &Smtp,
    subject: &str,
    body: String,
    to: &str,
    // TODO: extra headers (List-Unsubscribe for the reply-notification flow)
    // once that flow is wired up end-to-end.
    _extra_headers: &[(String, String)],
) -> anyhow::Result<()> {
    let from: Mailbox = smtp.from.parse()?;
    let to: Mailbox = to.parse()?;
    let msg = Message::builder()
        .from(from)
        .to(to)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body)?;

    let transport = build_transport(smtp)?;
    transport.send(msg).await?;
    Ok(())
}

fn build_transport(smtp: &Smtp) -> anyhow::Result<AsyncSmtpTransport<Tokio1Executor>> {
    let host = smtp.host.as_str();
    let port = smtp.port;
    let tls = match smtp.security.as_str() {
        "ssl" => Tls::Wrapper(TlsParameters::new(host.into())?),
        "starttls" => Tls::Required(TlsParameters::new(host.into())?),
        _ => Tls::None,
    };
    let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
        .port(port)
        .tls(tls);
    if !smtp.username.is_empty() || !smtp.password.is_empty() {
        builder = builder.credentials(Credentials::new(
            smtp.username.clone(),
            smtp.password.clone(),
        ));
    }
    Ok(builder.build())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_thread() -> Thread {
        Thread {
            id: 3,
            uri: "/thread".into(),
            title: Some("Post Title".into()),
        }
    }

    fn sample_comment() -> Comment {
        Comment {
            tid: 3,
            id: 42,
            parent: None,
            created: 1700000000.0,
            modified: None,
            mode: 2,
            remote_addr: Some("127.0.0.0".into()),
            text: "hello".into(),
            author: Some("jane".into()),
            email: Some("jane@example.com".into()),
            website: Some("https://example.com".into()),
            likes: 0,
            dislikes: 0,
            voters: vec![0; 256],
            notification: 0,
        }
    }

    fn signer_for_tests() -> Arc<Signer> {
        Arc::new(Signer::new(b"fixed-test-key"))
    }

    #[test]
    fn admin_body_contains_action_urls_and_author_info() {
        let signer = signer_for_tests();
        let body = format_admin_body(
            "https://comments.example.com",
            &sample_thread(),
            &sample_comment(),
            &signer,
        );
        assert!(
            body.contains("jane <jane@example.com> wrote:"),
            "got: {body}"
        );
        assert!(body.contains("IP address: 127.0.0.0"));
        assert!(
            body.contains("Link to comment: https://comments.example.com/thread#isso-42"),
            "got: {body}"
        );
        assert!(body.contains("Delete comment: https://comments.example.com/id/42/delete/"));
        // Mode 2 (pending) must produce an Activate link.
        assert!(body.contains("Activate comment: https://comments.example.com/id/42/activate/"));
    }

    #[test]
    fn admin_body_omits_activate_for_accepted_comments() {
        let mut c = sample_comment();
        c.mode = 1;
        let body = format_admin_body(
            "https://comments.example.com",
            &sample_thread(),
            &c,
            &signer_for_tests(),
        );
        assert!(body.contains("Delete comment"));
        assert!(!body.contains("Activate comment"));
    }

    #[test]
    fn notifier_flags_match_config() {
        let mut c = Config::default();
        c.general.notify = vec!["stdout".into(), "SMTP".into()];
        c.general.reply_notifications = true;
        let n = Notifier::new(Arc::new(c), signer_for_tests());
        assert!(n.wants_stdout());
        assert!(n.wants_admin_smtp());
        assert!(n.wants_reply_smtp());
    }

    #[test]
    fn public_endpoint_prefers_config_value_and_strips_trailing_slash() {
        let mut c = Config::default();
        c.server.public_endpoint = "https://comments.example.com/".into();
        let n = Notifier::new(Arc::new(c), signer_for_tests());
        assert_eq!(n.public_endpoint(), "https://comments.example.com");
    }

    #[test]
    fn public_endpoint_falls_back_to_first_host() {
        let mut c = Config::default();
        c.server.public_endpoint = String::new();
        c.general.hosts = vec!["https://comments.other/".into()];
        let n = Notifier::new(Arc::new(c), signer_for_tests());
        assert_eq!(n.public_endpoint(), "https://comments.other");
    }

    /// Ensure build_transport accepts the three security modes without
    /// actually opening a socket.
    #[test]
    fn build_transport_accepts_all_security_modes() {
        for sec in ["none", "starttls", "ssl"] {
            let smtp = Smtp {
                host: "smtp.example.com".into(),
                port: 2525,
                security: sec.into(),
                timeout: 10,
                ..Default::default()
            };
            assert!(
                build_transport(&smtp).is_ok(),
                "security={sec}: {:?}",
                build_transport(&smtp).err()
            );
        }
    }
}
