use std::collections::HashMap;
use std::env;

/// Build child process environment per SPEC §1.6.
pub fn child_env(allow_api: bool) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = env::vars().collect();
    out.remove("CLAUDECODE");
    out.remove("OPENROUTER_API_KEY");
    out.remove("MAX_THINKING_TOKENS");

    let keys: Vec<String> = out.keys().cloned().collect();
    for key in keys {
        if key.starts_with("ANTHROPIC_") && !allow_api {
            out.remove(&key);
        }
        if key.starts_with("OPENROUTER_") && !allow_api {
            out.remove(&key);
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn strips_secrets_without_allow_api() {
        let _guard = env_lock().lock().expect("lock");
        unsafe {
            env::set_var("ANTHROPIC_API_KEY", "secret");
            env::set_var("OPENROUTER_API_KEY", "or-secret");
            env::set_var("OPENROUTER_BASE_URL", "https://example.com");
            env::set_var("CLAUDECODE", "1");
            env::set_var("MAX_THINKING_TOKENS", "999");
            env::set_var("THICKAPI_TEST_KEEP", "yes");
        }
        let out = child_env(false);
        assert!(!out.contains_key("ANTHROPIC_API_KEY"));
        assert!(!out.contains_key("OPENROUTER_API_KEY"));
        assert!(!out.contains_key("OPENROUTER_BASE_URL"));
        assert!(!out.contains_key("CLAUDECODE"));
        assert!(!out.contains_key("MAX_THINKING_TOKENS"));
        assert_eq!(out.get("THICKAPI_TEST_KEEP"), Some(&"yes".to_string()));
        unsafe {
            env::remove_var("ANTHROPIC_API_KEY");
            env::remove_var("OPENROUTER_API_KEY");
            env::remove_var("OPENROUTER_BASE_URL");
            env::remove_var("CLAUDECODE");
            env::remove_var("MAX_THINKING_TOKENS");
            env::remove_var("THICKAPI_TEST_KEEP");
        }
    }

    #[test]
    fn preserves_anthropic_when_allow_api() {
        let _guard = env_lock().lock().expect("lock");
        unsafe {
            env::set_var("ANTHROPIC_API_KEY", "secret");
            env::set_var("OPENROUTER_API_KEY", "or-secret");
        }
        let out = child_env(true);
        assert_eq!(out.get("ANTHROPIC_API_KEY"), Some(&"secret".to_string()));
        assert!(!out.contains_key("OPENROUTER_API_KEY"));
        unsafe {
            env::remove_var("ANTHROPIC_API_KEY");
            env::remove_var("OPENROUTER_API_KEY");
        }
    }
}
