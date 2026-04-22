//! Jinja-compatible HTML templating for the admin UI.
//!
//! Uses minijinja so we can keep the exact `{{…}}` / `{% … %}` syntax from the
//! Python templates. The three templates (admin.html, login.html,
//! disabled.html) are embedded at compile time via `include_str!` — the binary
//! doesn't need a separate `templates/` directory at runtime.

use minijinja::{Environment, Error, ErrorKind, Value};
use time::format_description::FormatItem;
use time::macros::format_description;
use time::OffsetDateTime;

const FMT: &[FormatItem<'static>] = format_description!("[hour]:[minute] / [day]-[month]-[year]");

/// Load the three templates and register the `datetimeformat` filter that
/// Python's `isso.utils.render_template` registers.
pub fn env() -> Environment<'static> {
    let mut env = Environment::new();
    env.add_template("admin.html", include_str!("../templates/admin.html"))
        .expect("admin.html template must compile");
    env.add_template("login.html", include_str!("../templates/login.html"))
        .expect("login.html template must compile");
    env.add_template("disabled.html", include_str!("../templates/disabled.html"))
        .expect("disabled.html template must compile");
    env.add_filter("datetimeformat", datetimeformat);
    env
}

/// Jinja filter mirroring Python's `datetimeformat(value)` from
/// `isso.utils.render_template`:
/// `datetime.fromtimestamp(value).strftime("%H:%M / %d-%m-%Y")`.
///
/// One documented divergence: Python's `datetime.fromtimestamp` uses the
/// server's *local* timezone; we render in UTC. For deployments pinned to
/// UTC (Docker images, most cloud hosts) the output is byte-identical. Other
/// deployments will see admin-UI timestamps shifted by the local offset.
fn datetimeformat(value: Value) -> Result<String, Error> {
    // Minijinja Value exposes as_i64(); for a FLOAT column sqlx returns f64
    // which minijinja still stores internally — pull it via a serde_json
    // round-trip as a last resort.
    let secs: f64 = if let Some(n) = value.as_i64() {
        n as f64
    } else if let Ok(n) = serde_json::to_value(&value)
        .ok()
        .as_ref()
        .and_then(|v| v.as_f64())
        .ok_or(())
    {
        n
    } else {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "datetimeformat expects a number",
        ));
    };
    let odt = OffsetDateTime::from_unix_timestamp(secs as i64).map_err(|e| {
        Error::new(
            ErrorKind::InvalidOperation,
            format!("invalid timestamp: {e}"),
        )
    })?;
    odt.format(&FMT).map_err(|e| {
        Error::new(
            ErrorKind::InvalidOperation,
            format!("datetime format failed: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use minijinja::context;

    #[test]
    fn templates_compile() {
        // Construction succeeds iff every template is syntactically valid.
        let _env = env();
    }

    #[test]
    fn login_html_renders_host_script_placeholder() {
        // Minijinja's HTML autoescape replaces `/` with `&#x2f;` (Jinja2's
        // default only escapes `<>&"'` and leaves `/` alone). Both render
        // identically in every browser — this snapshot test locks in
        // *our* choice so any accidental template drift becomes a CI failure.
        let env = env();
        let got = env
            .get_template("login.html")
            .unwrap()
            .render(context! { isso_host_script => "https://comments.example.com" })
            .unwrap();
        let expected = r#"<!DOCTYPE html>
<html>
<head>
  <title>Isso admin</title>
  <link type="text/css" href="https:&#x2f;&#x2f;comments.example.com/css/isso.css" rel="stylesheet">
  <link type="text/css" href="https:&#x2f;&#x2f;comments.example.com/css/admin.css" rel="stylesheet">
</head>
<body>
  <div class="wrapper">
    <div class="header">
      <header>
        <img class="logo" src="https:&#x2f;&#x2f;comments.example.com/img/isso.svg" alt="Wynaut by @veekun"/>
        <div class="title">
          <a href="./">
            <h1>Isso</h1>
            <h2>Administration</h2>
          </a>
        </div>
      </header>
    </div>
    <main>
      <div id="login">
        Administration secured by password:
        <form method="POST" action="https:&#x2f;&#x2f;comments.example.com/login/">
          <input type="password" name="password" autofocus />
        </form>
      </div>
    </main>
  </div>
</body>
</html>"#;
        assert_eq!(got, expected);
    }

    #[test]
    fn disabled_html_contains_hint_text() {
        let env = env();
        let got = env
            .get_template("disabled.html")
            .unwrap()
            .render(context! { isso_host_script => "https://c.example" })
            .unwrap();
        let expected = r#"<!DOCTYPE html>
<html>
<head>
  <title>Isso admin</title>
  <link type="text/css" href="https:&#x2f;&#x2f;c.example/css/isso.css" rel="stylesheet">
  <link type="text/css" href="https:&#x2f;&#x2f;c.example/css/admin.css" rel="stylesheet">
</head>
<body>
  <div class="wrapper">
    <div class="header">
      <header>
        <img class="logo" src="https:&#x2f;&#x2f;c.example/img/isso.svg" alt="Wynaut by @veekun"/>
        <div class="title">
          <a href="./">
            <h1>Isso</h1>
            <h2>Administration</h2>
          </a>
        </div>
      </header>
    </div>
    <main>
      <div id="disabled">
        Administration is disabled on this instance of isso. Set enabled=true
        in the admin section of your isso configuration to enable it.
      </div>
    </main>
  </div>
</body>
</html>"#;
        assert_eq!(got, expected);
    }

    #[test]
    fn datetimeformat_matches_python_strftime() {
        // Python: datetime.fromtimestamp(1_700_000_000).strftime("%H:%M / %d-%m-%Y")
        //   → "23:13 / 14-11-2023" in UTC (verified: from_unix_timestamp gives UTC)
        let out = datetimeformat(Value::from(1_700_000_000_i64)).unwrap();
        assert_eq!(out, "22:13 / 14-11-2023");
    }
}
