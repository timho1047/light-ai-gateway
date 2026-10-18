//! The scalar field descriptor shared by the configuration page.
//!
//! One description owns a field's document path, label and current value. The
//! editable policy owns how it renders: a boolean becomes a select, a read-only
//! path becomes text with the read-only marker, and everything else is a text
//! input. Call sites list fields; they do not re-spell the controls.

use super::copy::Strings;
use super::layout::esc;
use crate::editable::{self, FieldClass, ValueKind};
use std::fmt::Write;

pub struct Field<'a> {
    path: Vec<&'a str>,
    label: String,
    value: String,
}

impl<'a> Field<'a> {
    pub fn new(path: &[&'a str], label: impl Into<String>, value: impl Into<String>) -> Self {
        Field {
            path: path.to_vec(),
            label: label.into(),
            value: value.into(),
        }
    }

    /// A field whose label is its document path, used by the server/policy
    /// sections where the full address is the operator-facing name.
    pub fn path(path: &[&'a str], value: impl Into<String>) -> Self {
        Field {
            path: path.to_vec(),
            label: path.join("."),
            value: value.into(),
        }
    }
}

pub fn flag(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

pub fn render_all(out: &mut String, fields: &[Field<'_>], s: &Strings) {
    for field in fields {
        render(out, field, s);
    }
}

pub fn render(out: &mut String, field: &Field<'_>, s: &Strings) {
    let policy = editable::field(
        &field
            .path
            .iter()
            .map(|part| part.to_string())
            .collect::<Vec<_>>(),
    );
    let class = policy
        .map(|policy| policy.class)
        .unwrap_or(FieldClass::ReadOnly);
    let marker = marker(class, s);
    let label = esc(&field.label);
    let value = esc(&field.value);

    if class != FieldClass::Live {
        let _ = write!(
            out,
            r#"<div class="field"><span class="field-label mono">{label}</span><span class="mono">{value}</span><span class="hint">{marker}</span></div>"#,
        );
        return;
    }

    if policy.is_some_and(|policy| policy.kind == ValueKind::Boolean) {
        let (true_selected, false_selected) = if field.value == "true" {
            (" selected", "")
        } else {
            ("", " selected")
        };
        let _ = write!(
            out,
            r#"<div class="field"><span class="field-label mono">{label}</span><select data-path="{}" data-initial="{}"><option value="true"{true_selected}>true</option><option value="false"{false_selected}>false</option></select><span class="hint">{marker}</span></div>"#,
            esc(&data_path(&field.path)),
            if field.value == "true" {
                "true"
            } else {
                "false"
            },
        );
        return;
    }

    let _ = write!(
        out,
        r#"<div class="field"><span class="field-label mono">{label}</span><input type="text" value="{value}" data-path="{}" data-initial="{value}" spellcheck="false" autocomplete="off"><span class="hint">{marker}</span></div>"#,
        esc(&data_path(&field.path)),
    );
}

fn marker(class: FieldClass, s: &Strings) -> String {
    let (class, text) = match class {
        FieldClass::Live => ("live", s.config_live),
        FieldClass::RestartOnly => ("restart", s.config_restart),
        FieldClass::ReadOnly => ("readonly", s.config_readonly),
    };
    format!(r#"<span class="tag {class}">{}</span>"#, esc(text))
}

pub(crate) fn data_path(path: &[&str]) -> String {
    serde_json::to_string(path).unwrap_or_else(|_| "[]".to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::copy::Lang;

    #[test]
    fn the_policy_decides_the_control_and_marker() {
        let s = Lang::En.strings();
        let mut out = String::new();
        render_all(
            &mut out,
            &[
                Field::new(&["policy", "allow_metered"], "allow_metered", "true"),
                Field::path(&["server", "listen"], "127.0.0.1:8787"),
                Field::path(&["server", "local_key"], "env:LGW_KEY"),
            ],
            s,
        );
        assert!(out.contains("<select"), "a boolean policy leaf is a select");
        assert_eq!(out.matches("<input").count(), 0);
        assert!(
            out.contains("tag restart"),
            "a restart-only field is marked"
        );
        assert!(out.contains("tag readonly"), "a secret is marked read-only");
        assert!(
            out.contains("server.listen"),
            "path labels are the full address"
        );
    }

    #[test]
    fn an_editable_text_field_carries_its_document_path() {
        let s = Lang::En.strings();
        let mut out = String::new();
        render(
            &mut out,
            &Field::new(&["policy", "max_retries"], "max_retries", "2"),
            s,
        );
        assert!(out.contains("data-path=\"[&quot;policy&quot;,&quot;max_retries&quot;]\""));
        assert!(out.contains("tag live"));
    }
}
