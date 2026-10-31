use std::collections::BTreeMap;

/// Rendered plain-text and HTML versions of one transactional message.
#[derive(Clone, PartialEq, Eq)]
pub struct RenderedMail {
    pub subject: String,
    pub text: String,
    pub html: String,
}

#[derive(Clone, Copy)]
struct Source {
    subject: &'static str,
    text: &'static str,
    html: &'static str,
}

fn source(template: &str, language: &str) -> Option<Source> {
    let de =
        language.eq_ignore_ascii_case("de") || language.to_ascii_lowercase().starts_with("de-");
    Some(match (template, de) {
        ("invitation", true) => Source {
            subject: "Einladung zu {{organization_name}}",
            text: include_str!("../templates/de/invitation.txt"),
            html: include_str!("../templates/de/invitation.html"),
        },
        ("invitation", false) => Source {
            subject: "Invitation to {{organization_name}}",
            text: include_str!("../templates/en/invitation.txt"),
            html: include_str!("../templates/en/invitation.html"),
        },
        ("password_reset", true) => Source {
            subject: "CraftCloud Passwort zurücksetzen",
            text: include_str!("../templates/de/password_reset.txt"),
            html: include_str!("../templates/de/password_reset.html"),
        },
        ("password_reset", false) => Source {
            subject: "Reset your CraftCloud password",
            text: include_str!("../templates/en/password_reset.txt"),
            html: include_str!("../templates/en/password_reset.html"),
        },
        ("email_verification", true) => Source {
            subject: "E-Mail-Adresse bestätigen",
            text: include_str!("../templates/de/email_verification.txt"),
            html: include_str!("../templates/de/email_verification.html"),
        },
        ("email_verification", false) => Source {
            subject: "Verify your email address",
            text: include_str!("../templates/en/email_verification.txt"),
            html: include_str!("../templates/en/email_verification.html"),
        },
        ("email_change", true) => Source {
            subject: "E-Mail-Adresse ändern",
            text: include_str!("../templates/de/email_change.txt"),
            html: include_str!("../templates/de/email_change.html"),
        },
        ("email_change", false) => Source {
            subject: "Change your email address",
            text: include_str!("../templates/en/email_change.txt"),
            html: include_str!("../templates/en/email_change.html"),
        },
        ("security_notice", true) => Source {
            subject: "Sicherheitshinweis für dein CraftCloud-Konto",
            text: include_str!("../templates/de/security_notice.txt"),
            html: include_str!("../templates/de/security_notice.html"),
        },
        ("security_notice", false) => Source {
            subject: "CraftCloud account security notice",
            text: include_str!("../templates/en/security_notice.txt"),
            html: include_str!("../templates/en/security_notice.html"),
        },
        ("test", true) => Source {
            subject: "CraftCloud Testnachricht",
            text: include_str!("../templates/de/test.txt"),
            html: include_str!("../templates/de/test.html"),
        },
        ("test", false) => Source {
            subject: "CraftCloud test message",
            text: include_str!("../templates/en/test.txt"),
            html: include_str!("../templates/en/test.html"),
        },
        _ => return None,
    })
}

/// Render a known template. Unknown placeholders fail; inserted values are not
/// scanned again, and values are escaped in HTML output.
pub fn render(
    template: &str,
    language: &str,
    params: &BTreeMap<String, String>,
) -> Option<RenderedMail> {
    let source = source(template, language)?;
    let mut values = BTreeMap::from([
        ("recipient_name".to_owned(), String::new()),
        ("inviter_name".to_owned(), String::new()),
        ("organization_name".to_owned(), String::new()),
        ("org_role".to_owned(), String::new()),
        ("workspace_name".to_owned(), String::new()),
        ("action_url".to_owned(), String::new()),
        ("expires_at".to_owned(), String::new()),
        ("old_email".to_owned(), String::new()),
        ("new_email".to_owned(), String::new()),
        ("security_event".to_owned(), String::new()),
        ("support_email".to_owned(), String::new()),
    ]);
    values.extend(
        params
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    Some(RenderedMail {
        subject: substitute(source.subject, &values, false)?,
        text: substitute(source.text, &values, false)?,
        html: substitute(source.html, &values, true)?,
    })
}

fn substitute(input: &str, values: &BTreeMap<String, String>, html: bool) -> Option<String> {
    let mut output = String::with_capacity(input.len());
    let mut remaining = input;
    while let Some(start) = remaining.find("{{") {
        output.push_str(remaining.get(..start)?);
        let placeholder = remaining.get(start + 2..)?;
        let end = placeholder.find("}}")?;
        let key = placeholder.get(..end)?;
        let value = values.get(key)?;
        if html {
            output.push_str(&escape_html(value));
        } else {
            output.push_str(value);
        }
        remaining = placeholder.get(end + 2..)?;
    }
    output.push_str(remaining);
    Some(output)
}

fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn language_falls_back_to_english_and_unknown_template_fails() {
        let params = BTreeMap::from([("organization_name".into(), "Studio".into())]);
        let rendered = render("invitation", "fr", &params).unwrap();
        assert_eq!(rendered.subject, "Invitation to Studio");
        assert!(render("unknown", "en", &params).is_none());
    }

    #[test]
    fn html_values_are_escaped_and_text_values_are_preserved() {
        let params = BTreeMap::from([
            (
                "recipient_name".into(),
                "<img src=x onerror=alert(1)> & \"x\"".into(),
            ),
            (
                "action_url".into(),
                "https://example.invalid/?a=1&b=2".into(),
            ),
            ("organization_name".into(), "Studio".into()),
        ]);
        let rendered = render("invitation", "en", &params).unwrap();
        assert!(
            rendered
                .html
                .contains("&lt;img src=x onerror=alert(1)&gt; &amp; &quot;x&quot;")
        );
        assert!(rendered.html.contains("a=1&amp;b=2"));
        assert!(rendered.text.contains("<img src=x onerror=alert(1)>"));
    }

    #[test]
    fn inserted_values_are_not_scanned_as_template_placeholders() {
        let params = BTreeMap::from([
            ("recipient_name".into(), "{{action_url}}".into()),
            ("action_url".into(), "https://cloud.example/token".into()),
            ("organization_name".into(), "Studio".into()),
        ]);
        let rendered = render("invitation", "en", &params).unwrap();
        assert!(rendered.text.contains("Hello {{action_url}},"));
        assert!(rendered.html.contains("Hello {{action_url}},"));
        assert!(
            rendered
                .html
                .contains("href=\"https://cloud.example/token\"")
        );
    }

    #[test]
    fn every_template_has_plain_and_html_content_in_both_languages() {
        for template in [
            "invitation",
            "password_reset",
            "email_verification",
            "email_change",
            "security_notice",
            "test",
        ] {
            for language in ["en", "de"] {
                let mail = render(template, language, &BTreeMap::new()).unwrap();
                assert!(!mail.text.is_empty());
                assert!(mail.html.contains("<!doctype html>"));
            }
        }
    }

    #[test]
    fn security_and_test_templates_have_no_action_link_or_external_assets() {
        for template in ["security_notice", "test"] {
            let mail = render(template, "en", &BTreeMap::new()).unwrap();
            assert!(!mail.html.contains("href="));
            assert!(!mail.html.contains("<img"));
            assert!(!mail.html.contains("http://"));
            assert!(!mail.html.contains("https://"));
        }
    }
}
