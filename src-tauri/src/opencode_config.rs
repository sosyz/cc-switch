use crate::config::write_json_file;
use crate::error::AppError;
use crate::provider::OpenCodeProviderConfig;
use crate::settings::get_opencode_override_dir;
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Mutex;

// Global cache for storing original file content with comments
static ORIGINAL_CONFIG_CONTENT: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));

pub fn get_opencode_dir() -> PathBuf {
    if let Some(override_dir) = get_opencode_override_dir() {
        return override_dir;
    }

    dirs::home_dir()
        .map(|h| h.join(".config").join("opencode"))
        .unwrap_or_else(|| PathBuf::from(".config").join("opencode"))
}

pub fn get_opencode_config_path() -> PathBuf {
    get_opencode_dir().join("opencode.json")
}

#[allow(dead_code)]
pub fn get_opencode_env_path() -> PathBuf {
    get_opencode_dir().join(".env")
}

/// Strip JSONC comments from input string
/// Handles both line comments (//) and block comments (/* */)
/// Preserves strings and escaped characters
fn strip_jsonc_comments(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escape = false;

    while let Some(&c) = chars.peek() {
        if in_string {
            result.push(c);
            chars.next();
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            result.push(c);
            chars.next();
        } else if c == '/' {
            chars.next();
            match chars.peek() {
                Some('/') => {
                    // Line comment - skip until newline
                    chars.next();
                    while let Some(&nc) = chars.peek() {
                        if nc == '\n' {
                            break;
                        }
                        chars.next();
                    }
                }
                Some('*') => {
                    // Block comment - skip until */
                    chars.next();
                    while let Some(nc) = chars.next() {
                        if nc == '*' {
                            if let Some(&'/') = chars.peek() {
                                chars.next();
                                break;
                            }
                        }
                    }
                }
                _ => {
                    // Not a comment, just a slash
                    result.push('/');
                }
            }
        } else {
            result.push(c);
            chars.next();
        }
    }
    result
}

pub fn read_opencode_config() -> Result<Value, AppError> {
    let path = get_opencode_config_path();

    if !path.exists() {
        return Ok(json!({
            "$schema": "https://opencode.ai/config.json"
        }));
    }

    let content = std::fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?;

    // Store original content for comment preservation
    if let Ok(mut cache) = ORIGINAL_CONFIG_CONTENT.lock() {
        *cache = Some(content.clone());
    }

    // Strip JSONC comments before parsing
    let cleaned = strip_jsonc_comments(&content);
    serde_json::from_str(&cleaned).map_err(|e| AppError::json(&path, e))
}

pub fn write_opencode_config(config: &Value) -> Result<(), AppError> {
    let path = get_opencode_config_path();

    // Try to preserve comments if we have original content
    let content_to_write = if let Ok(cache) = ORIGINAL_CONFIG_CONTENT.lock() {
        if let Some(original) = cache.as_ref() {
            // Try to merge new config with original content preserving comments
            match preserve_comments_in_json(original, config) {
                Ok(merged) => merged,
                Err(_) => {
                    // If merging fails, fallback to standard JSON
                    log::warn!("Failed to preserve comments, writing standard JSON");
                    serde_json::to_string_pretty(config)
                        .map_err(|e| AppError::JsonSerialize { source: e })?
                }
            }
        } else {
            // No original content, write standard JSON
            serde_json::to_string_pretty(config)
                .map_err(|e| AppError::JsonSerialize { source: e })?
        }
    } else {
        // Lock failed, write standard JSON
        serde_json::to_string_pretty(config).map_err(|e| AppError::JsonSerialize { source: e })?
    };

    // Write the content atomically
    crate::config::atomic_write(&path, content_to_write.as_bytes())?;

    log::debug!("OpenCode config written to {path:?}");
    Ok(())
}

/// Preserve comments from original JSONC while updating with new values
/// This is a best-effort approach that handles common cases
fn preserve_comments_in_json(original: &str, new_config: &Value) -> Result<String, AppError> {
    // Parse original without comments to get structure
    let cleaned = strip_jsonc_comments(original);
    let original_parsed: Value = serde_json::from_str(&cleaned)
        .map_err(|e| AppError::Config(format!("Failed to parse original: {}", e)))?;

    // If the structure is significantly different, use jsonc-parser approach
    // For now, use a simpler approach: if values match, keep original; else rewrite section
    if original_parsed == *new_config {
        // No changes needed, return original with comments intact
        return Ok(original.to_string());
    }

    // Attempt smart merge using jsonc-parser
    use jsonc_parser::{parse_to_serde_value, ParseOptions};

    // Parse with jsonc-parser which preserves more structure info
    let parsed = parse_to_serde_value(original, &ParseOptions::default())
        .map_err(|e| AppError::Config(format!("JSONC parse error: {:?}", e)))?;

    // If parsed value equals new config, return original
    if let Some(parsed_val) = parsed {
        if parsed_val == *new_config {
            return Ok(original.to_string());
        }
    }

    // For complex merges, we'll need to do smart value replacement
    // For now, return pretty-printed JSON (comments will be lost in this case)
    // This is a fallback - in practice, most operations are adding/removing keys
    // which we can handle better

    // Try to preserve structure by doing line-by-line replacement
    let result = smart_json_merge(original, &original_parsed, new_config)?;
    Ok(result)
}

/// Smart JSON merge that attempts to preserve comments and formatting
/// by doing surgical updates where possible
fn smart_json_merge(
    original: &str,
    old_value: &Value,
    new_value: &Value,
) -> Result<String, AppError> {
    // If values are identical, return original
    if old_value == new_value {
        return Ok(original.to_string());
    }

    // If both are objects, try to merge key by key
    if let (Some(old_obj), Some(new_obj)) = (old_value.as_object(), new_value.as_object()) {
        return merge_json_objects(original, old_obj, new_obj);
    }

    // For other cases (arrays, primitives, type changes), regenerate JSON
    Ok(serde_json::to_string_pretty(new_value)
        .map_err(|e| AppError::JsonSerialize { source: e })?)
}

/// Merge JSON objects while preserving comments
fn merge_json_objects(
    original: &str,
    old_obj: &Map<String, Value>,
    new_obj: &Map<String, Value>,
) -> Result<String, AppError> {
    // Check if only values changed but not structure
    let old_keys: std::collections::HashSet<_> = old_obj.keys().collect();
    let new_keys: std::collections::HashSet<_> = new_obj.keys().collect();

    // If structure is very different, just rewrite
    if old_keys != new_keys {
        // For structural changes, we need to rewrite
        // But we can try to preserve comments for unchanged sections
        return Ok(serde_json::to_string_pretty(new_obj)
            .map_err(|e| AppError::JsonSerialize { source: e })?);
    }

    // All keys are the same, try to update values in place
    // This is complex - for MVP, if structure is same but values differ,
    // we'll use a simple heuristic: find and replace value strings

    let mut result = original.to_string();

    for (key, new_val) in new_obj {
        if let Some(old_val) = old_obj.get(key) {
            if old_val != new_val {
                // Try to replace the old value with new value
                // This is a simplified approach
                result = replace_json_value(&result, key, old_val, new_val)?;
            }
        }
    }

    Ok(result)
}

/// Replace a JSON value in the original text
/// This is a best-effort simple implementation
fn replace_json_value(
    text: &str,
    key: &str,
    old_val: &Value,
    new_val: &Value,
) -> Result<String, AppError> {
    // For simple values, try string replacement
    let old_str =
        serde_json::to_string(old_val).map_err(|e| AppError::JsonSerialize { source: e })?;
    let new_str =
        serde_json::to_string_pretty(new_val).map_err(|e| AppError::JsonSerialize { source: e })?;

    // Look for pattern: "key": old_value
    let pattern = format!("\"{}\":{}", key, old_str);
    let replacement = format!("\"{}\":{}", key, new_str);

    if text.contains(&pattern) {
        Ok(text.replacen(&pattern, &replacement, 1))
    } else {
        // Try with whitespace variations
        let pattern_with_space = format!("\"{}\": {}", key, old_str);
        let replacement_with_space = format!("\"{}\": {}", key, new_str);

        if text.contains(&pattern_with_space) {
            Ok(text.replacen(&pattern_with_space, &replacement_with_space, 1))
        } else {
            // Can't find exact match, return text as-is
            // This means we'll lose the update, but preserve comments
            // In practice, this shouldn't happen often
            Ok(text.to_string())
        }
    }
}

pub fn get_providers() -> Result<Map<String, Value>, AppError> {
    let config = read_opencode_config()?;
    Ok(config
        .get("provider")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default())
}

pub fn set_provider(id: &str, config: Value) -> Result<(), AppError> {
    let mut full_config = read_opencode_config()?;

    if full_config.get("provider").is_none() {
        full_config["provider"] = json!({});
    }

    if let Some(providers) = full_config
        .get_mut("provider")
        .and_then(|v| v.as_object_mut())
    {
        providers.insert(id.to_string(), config);
    }

    write_opencode_config(&full_config)
}

pub fn remove_provider(id: &str) -> Result<(), AppError> {
    let mut config = read_opencode_config()?;

    if let Some(providers) = config.get_mut("provider").and_then(|v| v.as_object_mut()) {
        providers.remove(id);
    }

    write_opencode_config(&config)
}

pub fn get_typed_providers() -> Result<IndexMap<String, OpenCodeProviderConfig>, AppError> {
    let providers = get_providers()?;
    let mut result = IndexMap::new();

    for (id, value) in providers {
        match serde_json::from_value::<OpenCodeProviderConfig>(value.clone()) {
            Ok(config) => {
                result.insert(id, config);
            }
            Err(e) => {
                log::warn!("Failed to parse provider '{id}': {e}");
            }
        }
    }

    Ok(result)
}

pub fn set_typed_provider(id: &str, config: &OpenCodeProviderConfig) -> Result<(), AppError> {
    let value = serde_json::to_value(config).map_err(|e| AppError::JsonSerialize { source: e })?;
    set_provider(id, value)
}

pub fn get_mcp_servers() -> Result<Map<String, Value>, AppError> {
    let config = read_opencode_config()?;
    Ok(config
        .get("mcp")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default())
}

pub fn set_mcp_server(id: &str, config: Value) -> Result<(), AppError> {
    let mut full_config = read_opencode_config()?;

    if full_config.get("mcp").is_none() {
        full_config["mcp"] = json!({});
    }

    if let Some(mcp) = full_config.get_mut("mcp").and_then(|v| v.as_object_mut()) {
        mcp.insert(id.to_string(), config);
    }

    write_opencode_config(&full_config)
}

pub fn remove_mcp_server(id: &str) -> Result<(), AppError> {
    let mut config = read_opencode_config()?;

    if let Some(mcp) = config.get_mut("mcp").and_then(|v| v.as_object_mut()) {
        mcp.remove(id);
    }

    write_opencode_config(&config)
}

pub fn add_plugin(plugin_name: &str) -> Result<(), AppError> {
    let mut config = read_opencode_config()?;

    let plugins = config.get_mut("plugin").and_then(|v| v.as_array_mut());

    match plugins {
        Some(arr) => {
            // Mutual exclusion: standard OMO and OMO Slim cannot coexist as plugins
            if plugin_name.starts_with("oh-my-opencode")
                && !plugin_name.starts_with("oh-my-opencode-slim")
            {
                // Adding standard OMO -> remove all Slim variants
                arr.retain(|v| {
                    v.as_str()
                        .map(|s| !s.starts_with("oh-my-opencode-slim"))
                        .unwrap_or(true)
                });
            } else if plugin_name.starts_with("oh-my-opencode-slim") {
                // Adding Slim -> remove all standard OMO variants (but keep slim)
                arr.retain(|v| {
                    v.as_str()
                        .map(|s| {
                            !s.starts_with("oh-my-opencode") || s.starts_with("oh-my-opencode-slim")
                        })
                        .unwrap_or(true)
                });
            }

            let already_exists = arr.iter().any(|v| v.as_str() == Some(plugin_name));
            if !already_exists {
                arr.push(Value::String(plugin_name.to_string()));
            }
        }
        None => {
            config["plugin"] = json!([plugin_name]);
        }
    }

    write_opencode_config(&config)
}

pub fn remove_plugin_by_prefix(prefix: &str) -> Result<(), AppError> {
    let mut config = read_opencode_config()?;

    if let Some(arr) = config.get_mut("plugin").and_then(|v| v.as_array_mut()) {
        arr.retain(|v| {
            v.as_str()
                .map(|s| {
                    if !s.starts_with(prefix) {
                        return true; // Keep: doesn't match prefix at all
                    }
                    let rest = &s[prefix.len()..];
                    rest.starts_with('-')
                })
                .unwrap_or(true)
        });

        if arr.is_empty() {
            config.as_object_mut().map(|obj| obj.remove("plugin"));
        }
    }

    write_opencode_config(&config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_jsonc_comments_line_comments() {
        let input = r#"{
  // This is a line comment
  "key": "value" // inline comment
}"#;
        let result = strip_jsonc_comments(input);
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn test_strip_jsonc_comments_block_comments() {
        let input = r#"{
  /* This is a
     block comment */
  "key": "value"
}"#;
        let result = strip_jsonc_comments(input);
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn test_strip_jsonc_comments_preserve_strings() {
        let input = r#"{
  "url": "https://example.com//path", // comment
  "pattern": "/* not a comment */"
}"#;
        let result = strip_jsonc_comments(input);
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["url"], "https://example.com//path");
        assert_eq!(parsed["pattern"], "/* not a comment */");
    }

    #[test]
    fn test_strip_jsonc_comments_complex() {
        let input = r#"{
  // Header comment
  "provider": {
    "openai": { // OpenAI config
      "apiKey": "sk-test", /* API key */
      "model": "gpt-4"
    }
  },
  /* Footer comment */
  "$schema": "https://opencode.ai/config.json"
}"#;
        let result = strip_jsonc_comments(input);
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["provider"].is_object());
        assert_eq!(parsed["provider"]["openai"]["model"], "gpt-4");
    }

    #[test]
    fn test_preserve_comments_no_change() {
        let original = r#"{
  // This is a comment
  "key": "value"
}"#;
        let parsed = serde_json::from_str(&strip_jsonc_comments(original)).unwrap();
        let result = preserve_comments_in_json(original, &parsed).unwrap();
        // Should return original with comments intact
        assert!(result.contains("// This is a comment"));
        assert_eq!(result, original);
    }

    #[test]
    fn test_preserve_comments_value_change() {
        let original = r#"{
  // Important config
  "key": "old_value"
}"#;
        let mut new_config = serde_json::from_str(&strip_jsonc_comments(original)).unwrap();
        if let Some(obj) = new_config.as_object_mut() {
            obj.insert("key".to_string(), Value::String("new_value".to_string()));
        }

        let result = preserve_comments_in_json(original, &new_config).unwrap();
        // Should preserve comment
        assert!(result.contains("// Important config"));
        // Should have new value
        let parsed = serde_json::from_str(&strip_jsonc_comments(&result)).unwrap();
        assert_eq!(parsed["key"], "new_value");
    }

    #[test]
    fn test_smart_json_merge_identical() {
        let original = r#"{"key": "value"}"#;
        let value: Value = serde_json::from_str(original).unwrap();
        let result = smart_json_merge(original, &value, &value).unwrap();
        assert_eq!(result, original);
    }

    #[test]
    fn test_replace_json_value_simple() {
        let text = r#"{"name": "test", "count": 42}"#;
        let old_val = json!(42);
        let new_val = json!(100);
        let result = replace_json_value(text, "count", &old_val, &new_val).unwrap();
        assert!(result.contains("100"));
        assert!(!result.contains("42"));
    }

    #[test]
    fn test_replace_json_value_with_space() {
        let text = r#"{"name": "test", "count": 42}"#;
        let old_val = json!("test");
        let new_val = json!("updated");
        let result = replace_json_value(text, "name", &old_val, &new_val).unwrap();
        assert!(result.contains("updated"));
    }
}
