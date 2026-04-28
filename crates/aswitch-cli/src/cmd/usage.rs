use chrono::{DateTime, Local};
use serde_json::{Map, Value};

pub(crate) fn format_quota(quota: &Map<String, Value>) -> String {
    if quota.is_empty() {
        return "-".to_string();
    }

    let mut items = Vec::new();

    if let Some(remaining) = quota.get("remaining_percent").and_then(Value::as_f64) {
        items.push(format!("remaining={}", display_percent(remaining)));
    } else if let Some(value) = quota.get("limit_tokens").and_then(simple_value_to_string) {
        items.push(value);
    }

    if let Some(reset_time) = quota.get("reset_time").and_then(simple_value_to_string) {
        items.push(format!("reset={}", format_local_time(&reset_time)));
    }

    if items.is_empty() {
        items = quota
            .iter()
            .take(3)
            .filter_map(|(key, value)| {
                simple_value_to_string(value).map(|value| format!("{key}={value}"))
            })
            .collect::<Vec<_>>();
    }

    items.join(", ")
}

fn display_percent(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}%", value as i64)
    } else {
        format!("{value:.1}%")
    }
}

fn simple_value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

pub(crate) fn format_local_time(value: &str) -> String {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return parsed
            .with_timezone(&Local)
            .format("%m-%d %H:%M")
            .to_string();
    }

    value.to_string()
}
