//! Value conversion helpers: json_to_value, toml_to_value, current_system.

use crate::value::Value;

pub(crate) fn json_to_value(json: &serde_json::Value) -> Value {
    Value::from(json)
}

pub(crate) fn toml_to_value(v: &toml::Value) -> Value {
    Value::from(v)
}

pub(crate) fn current_system() -> &'static str {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "aarch64-darwin"
        } else {
            "x86_64-darwin"
        }
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-linux"
    } else {
        "x86_64-linux"
    }
}
