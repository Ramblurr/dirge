#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plugin_manager_new() {
        let mgr = PluginManager::new();
        assert!(mgr.hooks.is_empty());
    }

    #[test]
    fn test_register_hook() {
        let mut mgr = PluginManager::new();
        mgr.register("on-init", "test-init");
        assert_eq!(mgr.hooks.len(), 1);
        assert!(mgr.hooks.contains_key("on-init"));
    }

    #[test]
    fn test_register_multiple_hooks() {
        let mut mgr = PluginManager::new();
        mgr.register("on-init", "test-init");
        mgr.register("on-prompt", "test-prompt");
        mgr.register("on-response", "test-response");
        assert_eq!(mgr.hooks.len(), 3);
    }

    #[test]
    fn test_load_and_eval_janet() {
        let mut mgr = PluginManager::new();
        let result = mgr.eval("(+ 1 2)");
        assert_eq!(result, Ok("3".to_string()));
    }

    #[test]
    fn test_load_and_eval_janet_error() {
        let mut mgr = PluginManager::new();
        let result = mgr.eval("(undefined-fn 1)");
        assert!(result.is_err());
    }

    #[test]
    fn test_dispatch_hook() {
        let mut mgr = PluginManager::new();
        mgr.eval("(defn on-init [ctx] (string \"loaded with model: \" (ctx :model)))")
            .unwrap();
        mgr.register("on-init", "on-init");
        let result = mgr.dispatch("on-init", "@{:model \"gpt-4\"}");
        assert!(result.is_ok());
        assert!(result.unwrap().contains("loaded with model: gpt-4"));
    }

    #[test]
    fn test_harness_log() {
        let mut mgr = PluginManager::new();
        let result = mgr.eval("(harness/log \"hello from plugin\")");
        assert!(result.is_ok());
    }

    #[test]
    fn test_load_file() {
        let mut mgr = PluginManager::new();
        let fixtures = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("plugins")
            .join("test_plugin.janet");
        mgr.load_file(&fixtures).unwrap();
        mgr.register("on-init", "on-init");
        let result = mgr.dispatch("on-init", "@{:model \"test\"}");
        assert!(result.is_ok());
        assert!(result.unwrap().contains("loaded with test"));
    }

    #[test]
    fn test_auto_discover_hooks() {
        let mut mgr = PluginManager::new();
        let fixtures = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("plugins")
            .join("test_plugin.janet");
        mgr.load_file(&fixtures).unwrap();

        // Simulate auto-discovery: check each hook and register if found.
        // The {stem}-{hook} naming convention means a plugin file named
        // "workflow.janet" exports functions like "workflow-on-init".
        // Here we test a file where functions are just "on-init" (no prefix).
        let hook_names = [
            "on-init", "on-prompt", "on-response",
            "on-tool-start", "on-tool-end", "on-error", "on-complete",
        ];
        let mut found = 0;
        for hook in &hook_names {
            // Try to eval the function name directly
            if mgr.eval(hook).is_ok() {
                mgr.register(hook, hook);
                found += 1;
            }
        }
        assert_eq!(found, 1, "should find exactly on-init function");

        let result = mgr.dispatch("on-init", "@{:model \"test\"}");
        assert!(result.is_ok());
        assert!(result.unwrap().contains("loaded with test"));

        // Verify unknown hooks gracefully return empty
        let result = mgr.dispatch("on-prompt", "@{:prompt \"hello\"}");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "");
    }
}

use std::collections::HashMap;

#[cfg(feature = "plugin")]
use janetrs::client::{Error as JanetError, JanetClient};

pub struct PluginManager {
    hooks: HashMap<String, Vec<String>>,
    #[cfg(feature = "plugin")]
    client: JanetClient,
    pub phase: String,
    pub pending_prompt: Option<String>,
    pub last_response: Option<String>,
}

impl PluginManager {
    pub fn new() -> Self {
        #[cfg(feature = "plugin")]
        let client = {
            let c = JanetClient::init_with_default_env().expect("Failed to initialize Janet VM");

            // Define harness API functions in Janet
            let _ = c.run(
                r#"
                (var harness-phase :idle)
                (var harness-pending nil)
                (var harness-response nil)

                (defn harness/log [msg] (print "[plugin] " msg))
                (defn harness/get-cwd [] (os/cwd))
                (defn harness/set-phase [p] (set harness-phase p))
                (defn harness/request-prompt [prompt]
                  (set harness-pending prompt))
                (defn harness/store-response [resp]
                  (set harness-response resp))
            "#,
            );

            c
        };

        PluginManager {
            hooks: HashMap::new(),
            #[cfg(feature = "plugin")]
            client,
            phase: String::from("idle"),
            pending_prompt: None,
            last_response: None,
        }
    }

    #[cfg(feature = "plugin")]
    pub fn load_file(&mut self, path: &std::path::Path) -> Result<(), String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("Failed to read plugin: {e}"))?;
        self.eval(&content)?;
        Ok(())
    }

    #[cfg(not(feature = "plugin"))]
    pub fn load_file(&mut self, _path: &std::path::Path) -> Result<(), String> {
        Ok(())
    }

    pub fn register(&mut self, hook: &str, script: &str) {
        self.hooks
            .entry(hook.to_string())
            .or_default()
            .push(script.to_string());
    }

    #[cfg(feature = "plugin")]
    pub fn sync_phase(&mut self) {
        if let Ok(val) = self.client.run("harness-phase") {
            self.phase = val.to_string();
        }
    }

    #[cfg(not(feature = "plugin"))]
    pub fn sync_phase(&mut self) {}

    #[cfg(feature = "plugin")]
    pub fn take_pending_prompt(&mut self) -> Option<String> {
        if let Ok(val) = self.client.run("harness-pending") {
            let s = val.to_string();
            if !s.is_empty() && s != "nil" {
                let _ = self.client.run("(set harness-pending nil)");
                return Some(s);
            }
        }
        None
    }

    #[cfg(not(feature = "plugin"))]
    pub fn take_pending_prompt(&mut self) -> Option<String> {
        None
    }

    #[cfg(feature = "plugin")]
    pub fn store_response(&mut self, response: &str) {
        let escaped = response.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = self
            .client
            .run(&format!(r#"(set harness-response "{}")"#, escaped));
    }

    #[cfg(not(feature = "plugin"))]
    pub fn store_response(&mut self, _response: &str) {}

    #[cfg(feature = "plugin")]
    pub fn eval(&mut self, code: &str) -> Result<String, String> {
        self.client
            .run(code)
            .map(|val| val.to_string())
            .map_err(|e: JanetError| format!("Janet error: {e}"))
    }

    #[cfg(not(feature = "plugin"))]
    pub fn eval(&mut self, _code: &str) -> Result<String, String> {
        Err("plugin feature not enabled".to_string())
    }

    #[cfg(feature = "plugin")]
    pub fn dispatch(&mut self, hook: &str, context_janet: &str) -> Result<String, String> {
        let names = match self.hooks.get(hook) {
            Some(names) => names.clone(),
            None => return Ok(String::new()),
        };

        let mut results = Vec::new();
        for name in &names {
            let code = format!(r#"(do (def ctx {}) ({} ctx))"#, context_janet, name);
            let result = self.eval(&code)?;
            if !result.is_empty() {
                results.push(result);
            }
        }

        Ok(results.join("\n"))
    }

    #[cfg(not(feature = "plugin"))]
    pub fn dispatch(&mut self, _hook: &str, _context_janet: &str) -> Result<String, String> {
        Ok(String::new())
    }
}
