//! Email, stdout and webhook notifications, mirroring
//! isso/ext/notifications.py.
//!
//! Three notification backends, all enabled by `[general] notify`:
//!
//! - **stdout**: log-line emission for new / edit / delete / activate events
//!   with Delete/Activate URLs the operator can click through — useful for a
//!   dev loop, matches Python's `Stdout` class.
//! - **smtp**: admin email on new-comment; reply email to parent-comment
//!   subscribers on activation. Delivery is best-effort: we fire-and-forget
//!   over a tokio task and log on failure.
//! - **webhook**: POST a `comment.created` JSON event to every `[webhook] url`
//!   for each saved comment, signed with HMAC-SHA256 when `secret` is set.
//!
//! The admin notification includes signed `Delete` / `Activate` URLs; the
//! reply notification includes a signed `Unsubscribe` URL *and* a
//! `List-Unsubscribe` header carrying the same URL so modern mail clients
//! expose a one-click unsubscribe.

use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use lettre::message::header::{ContentType, HeaderName, HeaderValue};
use lettre::message::{Mailbox, Message};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use serde_json::json;
use sha2::Sha256;
use sqlx::SqlitePool;

use crate::config::{Config, Smtp, Webhook};
use crate::db::comments::{self as cmt, Comment};
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
        let notifier = Self { config, signer };
        if notifier.wants_webhook() && notifier.config.webhook.urls.is_empty() {
            tracing::warn!("webhook notifications are enabled without webhook URLs");
        }
        notifier
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

    fn wants_webhook(&self) -> bool {
        self.config.general.notify.iter().any(|n| n == "webhook")
    }

    fn wants_stdout(&self) -> bool {
        self.config
            .general
            .notify
            .iter()
            .any(|n| n.eq_ignore_ascii_case("stdout"))
    }

    fn public_endpoint(&self) -> String {
        if self.config.server.public_endpoint.is_empty() {
            self.config
                .general
                .hosts
                .first()
                .cloned()
                .unwrap_or_default()
        } else {
            self.config.server.public_endpoint.clone()
        }
        .trim_end_matches('/')
        .to_string()
    }

    /// Fired after a new comment is persisted. Emits the admin SMTP email
    /// (when configured), and stdout logs in all cases. When the comment is
    /// *already accepted* (mode=1) we also fan out reply-notifications here,
    /// matching Python's `notify_new → notify_users if mode == 1`.
    pub fn comment_created(&self, pool: &SqlitePool, thread: &Thread, comment: &Comment) {
        if self.wants_stdout() {
            self.stdout_new(thread, comment);
        }
        if self.wants_admin_smtp() {
            self.spawn_admin_email(thread.clone(), comment.clone());
        }
        if self.wants_reply_smtp() && comment.mode == 1 {
            self.spawn_reply_fanout(pool.clone(), thread.clone(), comment.clone());
        }
        if self.wants_webhook() && !self.config.webhook.urls.is_empty() {
            let config = Arc::clone(&self.config);
            let body = webhook_body(thread, comment);
            tokio::spawn(async move { send_webhook(&config.webhook, &body).await });
        }
    }

    /// Fired when a pending comment is activated by an admin. Sends reply
    /// notifications unconditionally (the comment just became visible to
    /// the world) when `[general] reply-notifications` is on.
    pub fn comment_activated(&self, pool: &SqlitePool, thread: &Thread, comment: &Comment) {
        if self.wants_stdout() {
            tracing::info!("comment {} activated", comment.id);
        }
        if self.wants_reply_smtp() {
            self.spawn_reply_fanout(pool.clone(), thread.clone(), comment.clone());
        }
    }

    fn stdout_new(&self, thread: &Thread, comment: &Comment) {
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
        let base = self.public_endpoint();
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

    /// Resolve subscribers for a reply and send one email to each unique
    /// recipient. Runs on a tokio task so the HTTP response isn't blocked
    /// waiting on SMTP.
    fn spawn_reply_fanout(&self, pool: SqlitePool, thread: Thread, comment: Comment) {
        // Nothing to fan out if the comment isn't a reply or has no email
        // (we'd have nothing to exclude from the notified set in that case,
        // but we still want fanout when the author didn't leave an email —
        // they just don't self-filter).
        let Some(parent_id) = comment.parent else {
            return;
        };
        let config = Arc::clone(&self.config);
        let signer = Arc::clone(&self.signer);
        let base = self.public_endpoint();
        tokio::spawn(async move {
            let subscribers = match cmt::fetch_reply_subscribers(&pool, parent_id).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("reply fanout lookup failed: {e}");
                    return;
                }
            };
            let parent_comment = match cmt::get(&pool, parent_id).await {
                Ok(Some(c)) => c,
                Ok(None) => {
                    tracing::warn!("reply fanout: parent {parent_id} missing");
                    return;
                }
                Err(e) => {
                    tracing::warn!("reply fanout parent lookup failed: {e}");
                    return;
                }
            };

            let author_email = comment.email.as_deref().unwrap_or("");
            let mut notified: Vec<String> = Vec::new();
            for sub in &subscribers {
                let Some(email) = sub.email.as_deref() else {
                    continue;
                };
                if email.is_empty()
                    || sub.id == comment.id
                    || email == author_email
                    || notified.iter().any(|e| e == email)
                {
                    continue;
                }

                let body =
                    format_reply_body(&base, &thread, &comment, &parent_comment, email, &signer);
                let subject = match &thread.title {
                    Some(title) if !title.is_empty() => {
                        format!("Re: New comment posted on {title}")
                    }
                    _ => "Re: New comment posted".to_string(),
                };
                let list_unsub = list_unsubscribe_url(&base, parent_id, email, &signer);
                let extra = [("List-Unsubscribe".to_string(), format!("<{list_unsub}>"))];
                if let Err(e) = send_email(&config.smtp, &subject, body, email, &extra).await {
                    tracing::warn!("reply email to {email} failed: {e}");
                }
                notified.push(email.to_string());
            }
        });
    }
}

fn list_unsubscribe_url(base: &str, parent_id: i64, recipient: &str, signer: &Signer) -> String {
    let key = signer
        .sign(&("unsubscribe", recipient))
        .unwrap_or_else(|_| String::from("<sign-failed>"));
    format!(
        "{base}/id/{parent_id}/unsubscribe/{}/{key}",
        urlencoding::encode(recipient)
    )
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

/// Plain-text reply-notification body. Matches Python's `format(admin=False)`.
fn format_reply_body(
    base: &str,
    thread: &Thread,
    comment: &Comment,
    parent: &Comment,
    recipient: &str,
    signer: &Signer,
) -> String {
    let mut out = String::new();
    let author = match comment.author.as_deref() {
        Some(a) if !a.is_empty() => a.to_string(),
        _ => "Anonymous".to_string(),
    };
    out.push_str(&format!("{author} wrote:\n\n"));
    out.push_str(&comment.text);
    out.push_str("\n\n");
    out.push_str(&format!(
        "Link to comment: {base}{}#isso-{}\n\n---\n",
        thread.uri, comment.id
    ));
    out.push_str(&format!(
        "Unsubscribe from this conversation: {}\n",
        list_unsubscribe_url(base, parent.id, recipient, signer)
    ));
    out
}

async fn send_email(
    smtp: &Smtp,
    subject: &str,
    body: String,
    to: &str,
    extra_headers: &[(String, String)],
) -> anyhow::Result<()> {
    let from: Mailbox = smtp.from.parse()?;
    let to: Mailbox = to.parse()?;
    let mut builder = Message::builder()
        .from(from)
        .to(to)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN);
    for (name, value) in extra_headers {
        let header_name = HeaderName::new_from_ascii(name.clone())
            .map_err(|e| anyhow::anyhow!("invalid header name {name:?}: {e}"))?;
        builder = builder.raw_header(HeaderValue::new(header_name, value.clone()));
    }
    let msg = builder.body(body)?;

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
    if let Some(credentials) = credentials(smtp) {
        builder = builder.credentials(credentials);
    }
    Ok(builder.build())
}

/// Like isso, only log in when both a username and a password are set.
fn credentials(smtp: &Smtp) -> Option<Credentials> {
    (!smtp.username.is_empty() && !smtp.password.is_empty())
        .then(|| Credentials::new(smtp.username.clone(), smtp.password.clone()))
}

#[derive(serde::Serialize)]
struct WebhookPayload<'a> {
    event: &'static str,
    thread: WebhookThread<'a>,
    comment: WebhookComment<'a>,
}

#[derive(serde::Serialize)]
struct WebhookThread<'a> {
    id: i64,
    uri: &'a str,
    title: Option<&'a str>,
}

#[derive(serde::Serialize)]
struct WebhookComment<'a> {
    id: i64,
    parent: Option<i64>,
    created: f64,
    modified: Option<f64>,
    mode: i64,
    text: &'a str,
    author: Option<&'a str>,
    website: Option<&'a str>,
}

/// Payload of the `comment.created` webhook event. Email, IP address and
/// voters are deliberately left out.
///
/// The body is signed, so it is produced byte-for-byte like Python's
/// `json.dumps(payload, separators=(",", ":"))`: keys in insertion order
/// and non-ASCII characters escaped.
pub fn webhook_body(thread: &Thread, comment: &Comment) -> Vec<u8> {
    let payload = WebhookPayload {
        event: "comment.created",
        thread: WebhookThread {
            id: thread.id,
            uri: &thread.uri,
            title: thread.title.as_deref(),
        },
        comment: WebhookComment {
            id: comment.id,
            parent: comment.parent,
            created: comment.created,
            modified: comment.modified,
            mode: comment.mode,
            text: &comment.text,
            author: comment.author.as_deref(),
            website: comment.website.as_deref(),
        },
    };
    let json = serde_json::to_string(&payload).expect("payload serializes");
    ensure_ascii(&json).into_bytes()
}

/// Escape non-ASCII characters as `\uXXXX` (surrogate pairs outside the
/// BMP), like Python's `json.dumps` default `ensure_ascii=True`. Non-ASCII
/// can only occur inside JSON strings, so this is safe on serialized output.
fn ensure_ascii(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for ch in json.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else {
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

/// `sha256=` followed by the hex HMAC-SHA256 of `body`.
pub fn webhook_signature(secret: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// POST `body` to every configured URL. Failures are logged per URL and
/// don't stop delivery to the others.
pub async fn send_webhook(cfg: &Webhook, body: &[u8]) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(cfg.timeout))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("unable to build webhook client: {e}");
            return;
        }
    };
    for url in &cfg.urls {
        let mut req = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("X-Isso-Event", "comment.created")
            .body(body.to_vec());
        if !cfg.secret.is_empty() {
            req = req.header("X-Isso-Signature", webhook_signature(&cfg.secret, body));
        }
        if let Err(e) = req.send().await.and_then(|r| r.error_for_status()) {
            tracing::warn!("unable to deliver comment webhook to {url}: {e}");
        }
    }
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
    fn reply_body_is_recipient_specific() {
        // Each recipient gets their own signed unsubscribe URL.
        let signer = signer_for_tests();
        let parent = Comment {
            id: 10,
            ..sample_comment()
        };
        let body_alice = format_reply_body(
            "https://c.example",
            &sample_thread(),
            &sample_comment(),
            &parent,
            "alice@example.com",
            &signer,
        );
        let body_bob = format_reply_body(
            "https://c.example",
            &sample_thread(),
            &sample_comment(),
            &parent,
            "bob@example.com",
            &signer,
        );
        assert!(body_alice.contains("alice%40example.com"));
        assert!(body_bob.contains("bob%40example.com"));
        assert!(body_alice
            .contains("Unsubscribe from this conversation: https://c.example/id/10/unsubscribe/"));
        assert_ne!(body_alice, body_bob);
    }

    #[test]
    fn list_unsubscribe_url_is_signed_per_recipient() {
        let signer = signer_for_tests();
        let a = list_unsubscribe_url("https://c.example", 5, "a@b.com", &signer);
        let b = list_unsubscribe_url("https://c.example", 5, "c@d.com", &signer);
        assert!(a.starts_with("https://c.example/id/5/unsubscribe/a%40b.com/"));
        assert!(b.starts_with("https://c.example/id/5/unsubscribe/c%40d.com/"));
        assert_ne!(a, b);
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

    #[test]
    fn credentials_need_both_username_and_password() {
        let smtp = |username: &str, password: &str| Smtp {
            username: username.into(),
            password: password.into(),
            ..Default::default()
        };
        assert!(credentials(&smtp("", "")).is_none());
        assert!(credentials(&smtp("user", "")).is_none());
        assert!(credentials(&smtp("", "secret")).is_none());
        assert!(credentials(&smtp("user", "secret")).is_some());
    }

    #[test]
    fn webhook_body_omits_private_fields() {
        let body = webhook_body(&sample_thread(), &sample_comment());
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            payload,
            json!({
                "event": "comment.created",
                "thread": {"id": 3, "uri": "/thread", "title": "Post Title"},
                "comment": {
                    "id": 42,
                    "parent": null,
                    "created": 1700000000.0,
                    "modified": null,
                    "mode": 2,
                    "text": "hello",
                    "author": "jane",
                    "website": "https://example.com",
                },
            })
        );
    }

    #[test]
    fn webhook_body_matches_python_json_dumps() {
        let mut comment = sample_comment();
        comment.text = "caf\u{e9} \u{1f600}".into();
        // json.dumps(payload, separators=(",", ":")) for the same payload.
        let expected = concat!(
            r#"{"event":"comment.created","#,
            r#""thread":{"id":3,"uri":"/thread","title":"Post Title"},"#,
            r#""comment":{"id":42,"parent":null,"created":1700000000.0,"modified":null,"#,
            r#""mode":2,"text":"caf\u00e9 \ud83d\ude00","author":"jane","#,
            r#""website":"https://example.com"}}"#,
        );
        assert_eq!(
            String::from_utf8(webhook_body(&sample_thread(), &comment)).unwrap(),
            expected
        );
    }

    #[test]
    fn webhook_signature_matches_hmac_sha256() {
        // python3 -c 'import hmac,hashlib;print(hmac.new(b"key",b"body",hashlib.sha256).hexdigest())'
        assert_eq!(
            webhook_signature("key", b"body"),
            "sha256=515aae133b435d4000956731f68ae5cf5eb85d4f0dc6a546d2bfcd3595ec1ae1"
        );
    }

    #[tokio::test]
    async fn send_webhook_posts_to_every_url() {
        use axum::extract::State;
        use axum::http::HeaderMap;
        use tokio::sync::mpsc;

        type Received = (String, HeaderMap, Vec<u8>);
        let (tx, mut rx) = mpsc::unbounded_channel::<Received>();
        let app = axum::Router::new()
            .route(
                "/:name",
                axum::routing::post(
                    |State(tx): State<mpsc::UnboundedSender<Received>>,
                     axum::extract::Path(name): axum::extract::Path<String>,
                     headers: HeaderMap,
                     body: axum::body::Bytes| async move {
                        tx.send((name, headers, body.to_vec())).unwrap();
                    },
                ),
            )
            .with_state(tx);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let cfg = Webhook {
            urls: vec![
                format!("http://{addr}/first"),
                format!("http://{addr}/second"),
            ],
            secret: "test-secret".into(),
            timeout: 5,
        };
        let body = webhook_body(&sample_thread(), &sample_comment());
        send_webhook(&cfg, &body).await;

        for expected in ["first", "second"] {
            let (name, headers, received) = rx.recv().await.unwrap();
            assert_eq!(name, expected);
            assert_eq!(headers["content-type"], "application/json");
            assert_eq!(headers["x-isso-event"], "comment.created");
            assert_eq!(
                headers["x-isso-signature"],
                webhook_signature("test-secret", &body).as_str()
            );
            assert_eq!(received, body);
        }
    }
}
