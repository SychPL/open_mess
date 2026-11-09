//! The modules' own tools (manifest `extra_tools`, spec 2026-10-28-meetings-transcripts 5.1): which
//! of them both front ends list, as `<module>_<tool>`, and the arguments the core checks before the
//! module gets them. The module checks the types and ranges of its own arguments. A write tool
//! names the conversation it writes to and the account that writes (spec 2026-10-29-live-captions,
//! 5.1); its calls become pending actions.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};

use super::{ToolError, ToolInfo, recordings, shared};
use crate::modules::{ModuleInfo, ModuleState};

/// The largest arguments object, as JSON.
const MAX_ARGS: usize = 16 * 1024;
/// The largest result a module's own tool may return.
pub const MAX_RESULT: usize = 1024 * 1024;
const MAX_PROPERTIES: usize = 32;

/// The chat threads of the captions sessions agents read, by account and conversation, while the
/// session lasts (spec 2026-11-04-captions-archive, 4.2): a session's buffer holds what was said
/// in each of them, so an agent reading or leaving it must be allowed to read every one, checked
/// before the module is called and again on its answer. Only in memory.
#[derive(Default)]
pub(crate) struct Threads(Mutex<HashMap<(String, String), Known>>);

/// The threads a session named, and its token.
struct Known {
    session: Option<String>,
    threads: Vec<String>,
}

/// The sessions remembered; past it the map starts over (an agent reading more at once is unlikely).
const MAX_KNOWN: usize = 256;
const MAX_THREADS: usize = 16;

impl Threads {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), Known>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The threads known of the account's session in `conversation`.
    pub(crate) fn known(&self, account: &str, conversation: &str) -> Vec<String> {
        self.map().get(&(account.to_owned(), conversation.to_owned())).map(|known| known.threads.clone()).unwrap_or_default()
    }

    /// Takes in the threads a module's `result` names, and returns every thread known of the
    /// session. A result of another session forgets the earlier one's.
    pub(crate) fn learn(&self, account: &str, conversation: &str, result: &Value) -> Vec<String> {
        let session = result.get("session").and_then(Value::as_str).map(str::to_owned);
        let named = result.get("threads").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
        let named: Vec<String> = named.filter(|thread| thread.len() <= 256).take(MAX_THREADS).map(str::to_owned).collect();
        let mut map = self.map();
        let key = (account.to_owned(), conversation.to_owned());
        if map.get(&key).is_some_and(|known| session.is_some() && known.session != session) {
            map.remove(&key);
        }
        if named.is_empty() {
            return map.get(&key).map(|known| known.threads.clone()).unwrap_or_default();
        }
        if map.len() >= MAX_KNOWN && !map.contains_key(&key) {
            map.clear();
        }
        let known = map.entry(key).or_insert_with(|| Known { session: session.clone(), threads: Vec::new() });
        known.session = session.or(known.session.take());
        for thread in named {
            if !known.threads.contains(&thread) && known.threads.len() < MAX_THREADS {
                known.threads.push(thread);
            }
        }
        known.threads.clone()
    }
}

/// An extra tool of a module file that is the module (not one waiting to replace it).
pub(crate) struct Declared {
    pub module: String,
    pub state: ModuleState,
    /// As the manifest declares it; the module gets its name in `call`.
    pub decl: open_mess_host::ToolDecl,
    /// What the front ends list, or why it is not listed.
    pub listed: Result<Listed, String>,
}

pub(crate) struct Listed {
    pub info: ToolInfo,
    properties: Vec<String>,
    required: Vec<String>,
}

/// The name both front ends use: `-` in the module name becomes `_`, as model APIs take only
/// `[a-zA-Z0-9_-]{1,64}` and `.` is not among them.
pub(crate) fn full_name(module: &str, tool: &str) -> String {
    format!("{}_{tool}", module.replace('-', "_"))
}

/// Every extra tool of the modules, by its full name, each listed or with the reason it is not.
/// A name declared twice (in one module or two) or taken by a core tool is listed for neither.
pub(crate) fn declared(modules: &[ModuleInfo]) -> BTreeMap<String, Vec<Declared>> {
    let mut all: BTreeMap<String, Vec<Declared>> = BTreeMap::new();
    for info in modules.iter().filter(|info| info.state != ModuleState::PendingReplacement) {
        for tool in info.manifest.iter().flat_map(|manifest| &manifest.extra_tools) {
            let name = full_name(&info.name, &tool.name);
            let listed = check(&name, tool);
            all.entry(name).or_default().push(Declared { module: info.name.clone(), state: info.state.clone(), decl: tool.clone(), listed });
        }
    }
    for (name, tools) in &mut all {
        let taken = if core(name) {
            Some("its name is a core tool's")
        } else if tools.len() > 1 {
            Some("its name is declared more than once")
        } else {
            None
        };
        if let Some(why) = taken {
            for tool in tools.iter_mut() {
                tool.listed = Err(why.to_string());
            }
        }
    }
    all
}

/// The declarations of a module tool named `name`; None for a core tool's name, which a module
/// that declares it never shadows.
pub(crate) fn find(modules: &[ModuleInfo], name: &str) -> Option<Vec<Declared>> {
    if core(name) {
        return None;
    }
    declared(modules).remove(name)
}

/// Whether a core tool (`status`, a shared one or a recording one) has this name.
fn core(name: &str) -> bool {
    name == "status" || shared::tools().into_iter().chain(recordings::tools()).any(|tool| tool.name == name)
}

/// The tools of active modules the front ends list.
pub(crate) fn listed(modules: &[ModuleInfo]) -> Vec<ToolInfo> {
    let declared = declared(modules);
    let active = declared.into_values().flatten().filter(|tool| tool.state == ModuleState::Active);
    active.filter_map(|tool| tool.listed.ok()).map(|listed| listed.info).collect()
}

/// Why each of a module's tools is not listed, for `status`.
pub(crate) fn not_listed(modules: &[ModuleInfo], module: &str) -> Vec<Value> {
    let declared = declared(modules);
    let refused = declared.iter().flat_map(|(name, tools)| tools.iter().map(move |tool| (name, tool)));
    refused
        .filter(|(_, tool)| tool.module == module)
        .filter_map(|(name, tool)| tool.listed.as_ref().err().map(|why| json!({"tool": name, "reason": why})))
        .collect()
}

fn plain_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A declared tool as the front ends list it: plain names, a schema the core can check arguments
/// against, and a conversation required of a write tool; `account` is the core's to add.
fn check(name: &str, tool: &open_mess_host::ToolDecl) -> Result<Listed, String> {
    if !plain_name(&tool.name) {
        return Err("the name must be 1-32 characters of a-z, 0-9 and _, starting with a letter".to_string());
    }
    if name.len() > 64 {
        return Err("the name with the module's is over 64 characters".to_string());
    }
    let mut schema = match serde_json::from_str::<Value>(&tool.input_schema) {
        Ok(Value::Object(schema)) if schema.get("type") == Some(&json!("object")) => schema,
        _ => return Err("the input schema is not a JSON object schema".to_string()),
    };
    if schema.get("additionalProperties").is_some_and(|additional| additional != &json!(false)) {
        return Err("the input schema takes other properties".to_string());
    }
    let properties: Vec<String> = match schema.get("properties") {
        Some(Value::Object(properties)) if properties.len() <= MAX_PROPERTIES => properties.keys().cloned().collect(),
        _ => return Err(format!("the input schema needs properties, at most {MAX_PROPERTIES}")),
    };
    if let Some(bad) = properties.iter().find(|key| !plain_name(key)) {
        return Err(format!("the property {bad:?} is not a plain name"));
    }
    if properties.iter().any(|key| key == "account") {
        return Err("the input schema declares account, which the core adds".to_string());
    }
    let required: Vec<String> = match schema.get("required") {
        None => Vec::new(),
        Some(Value::Array(keys)) => {
            match keys.iter().map(|key| key.as_str().filter(|key| properties.iter().any(|p| p == key)).map(str::to_string)).collect() {
                Some(keys) => keys,
                None => return Err("required names a property the schema does not have".to_string()),
            }
        }
        Some(_) => return Err("required is not a list".to_string()),
    };
    if tool.writes && !required.iter().any(|key| key == "conversation") {
        return Err("a module write tool must name its conversation".to_string());
    }
    let account = if tool.writes {
        // As for `send`: the core never guesses the account that writes.
        json!({"type": "string", "description": "Account id from the accounts tool: the account that writes."})
    } else {
        json!({"type": "string", "description": "Account id from the accounts tool; left out, the module's only account the agent may read."})
    };
    if let Some(Value::Object(published)) = schema.get_mut("properties") {
        published.insert("account".to_string(), account);
    }
    if tool.writes
        && let Some(Value::Array(keys)) = schema.get_mut("required")
    {
        keys.push(json!("account"));
    }
    schema.insert("additionalProperties".to_string(), json!(false));
    let info = ToolInfo { name: name.to_string(), description: tool.description.clone(), input_schema: Value::Object(schema) };
    Ok(Listed { info, properties, required })
}

/// The arguments of a call checked against the tool's schema: an object, within the size limit,
/// with no unknown key, every required one, and `account` and `conversation` as text. Returns the
/// account asked for and what the module gets (the rest).
pub(crate) fn arguments(tool: &Listed, args: Value) -> Result<(Option<String>, Value), ToolError> {
    let invalid = |why: String| Err(ToolError::InvalidArgs(why));
    let Value::Object(mut args) = args else { return invalid("arguments must be a JSON object".into()) };
    if Value::Object(args.clone()).to_string().len() > MAX_ARGS {
        return invalid(format!("the arguments are over {} KiB", MAX_ARGS / 1024));
    }
    if let Some(key) = args.keys().find(|key| *key != "account" && !tool.properties.contains(key)) {
        return invalid(format!("unknown parameter {key:?}"));
    }
    if let Some(key) = tool.required.iter().find(|key| !args.contains_key(*key)) {
        return invalid(format!("{key} is required"));
    }
    for key in ["account", "conversation"] {
        if args.get(key).is_some_and(|value| !value.is_string()) {
            return invalid(format!("{key} must be text"));
        }
    }
    let account = args.remove("account").and_then(|account| account.as_str().map(str::to_string));
    Ok((account, Value::Object(args)))
}

#[cfg(test)]
mod tests {
    use open_mess_host::{Manifest, ToolDecl};

    use super::*;

    fn module(name: &str, state: ModuleState, tools: Vec<ToolDecl>) -> ModuleInfo {
        let manifest = Manifest {
            name: name.into(),
            version: "1".into(),
            platform: "test".into(),
            hosts: Vec::new(),
            sign_in_hosts: Vec::new(),
            wam: None,
            extra_tools: tools,
            media: None,
        };
        ModuleInfo {
            name: name.into(),
            file: format!("{name}.wasm"),
            version: "1".into(),
            platform: "test".into(),
            hosts: Vec::new(),
            state,
            file_sha256: String::new(),
            manifest_sha256: String::new(),
            manifest: Some(manifest),
        }
    }

    fn tool(name: &str, schema: Value) -> ToolDecl {
        ToolDecl { name: name.into(), description: format!("{name} tool"), input_schema: schema.to_string(), writes: false }
    }

    fn schema() -> Value {
        json!({"type": "object", "properties": {"conversation": {"type": "string"}, "from": {"type": "string"}}, "required": ["conversation"]})
    }

    fn reasons(modules: &[ModuleInfo], module: &str) -> Vec<(String, String)> {
        let reason = |v: &Value| (v["tool"].as_str().unwrap().to_string(), v["reason"].as_str().unwrap().to_string());
        not_listed(modules, module).iter().map(reason).collect()
    }

    #[test]
    fn only_fit_tools_of_active_modules_are_listed() {
        let writes = ToolDecl { writes: true, ..tool("poke", schema()) };
        let anywhere = ToolDecl { writes: true, ..tool("nudge", json!({"type": "object", "properties": {"conversation": {}}})) };
        let modules = [
            module(
                "my-mod",
                ModuleState::Active,
                vec![tool("peek", schema()), writes, anywhere, tool("Bad", schema()), tool("x".repeat(33).as_str(), schema())],
            ),
            module("off", ModuleState::Disabled, vec![tool("peek", schema())]),
        ];
        let listed = listed(&modules);
        assert_eq!(listed.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["my_mod_peek", "my_mod_poke"]);
        // `account` is added, other properties refused; a write tool requires it.
        assert_eq!(listed[0].input_schema["properties"]["account"]["type"], "string");
        assert_eq!(listed[0].input_schema["additionalProperties"], false);
        assert_eq!(listed[0].input_schema["required"], json!(["conversation"]));
        assert_eq!(listed[1].input_schema["required"], json!(["conversation", "account"]));
        let reasons = reasons(&modules, "my-mod");
        assert_eq!(reasons.len(), 3, "{reasons:?}");
        assert!(reasons.iter().any(|(name, why)| name == "my_mod_nudge" && why == "a module write tool must name its conversation"));
        assert!(reasons.iter().any(|(name, why)| name == "my_mod_Bad" && why.contains("a-z")));
    }

    #[test]
    fn schemas_the_core_cannot_check_are_refused() {
        for bad in [
            json!({"type": "array"}),
            json!({"type": "object"}),
            json!({"type": "object", "properties": {"account": {"type": "string"}}}),
            json!({"type": "object", "properties": {"Big": {}}}),
            json!({"type": "object", "properties": {"a": {}}, "required": ["b"]}),
            json!({"type": "object", "properties": {"a": {}}, "required": "a"}),
            json!({"type": "object", "properties": {"a": {}}, "additionalProperties": true}),
            json!({"type": "object", "properties": {"a": {}}, "additionalProperties": {"type": "string"}}),
            json!({"type": "object", "properties": (0..33).map(|n| (format!("p{n}"), json!({}))).collect::<serde_json::Map<_, _>>()}),
        ] {
            let modules = [module("m", ModuleState::Active, vec![tool("t", bad.clone())])];
            assert!(listed(&modules).is_empty(), "{bad}");
            assert_eq!(not_listed(&modules, "m").len(), 1, "{bad}");
        }
        let modules = [module("m", ModuleState::Active, vec![ToolDecl { input_schema: "{not json".into(), ..tool("t", json!({})) }])];
        assert!(listed(&modules).is_empty());
    }

    #[test]
    fn names_taken_twice_or_by_the_core_are_listed_for_neither() {
        let modules = [
            // `list` + `conversations` would shadow the shared tool, `list` + `recordings` a recording one.
            module(
                "list",
                ModuleState::Active,
                vec![tool("conversations", schema()), tool("recordings", schema()), tool("twin", schema()), tool("twin", schema())],
            ),
            // `a-b` + `c` and `a` + `b_c` are both `a_b_c`.
            module("a-b", ModuleState::Active, vec![tool("c", schema())]),
            module("a", ModuleState::Active, vec![tool("b_c", schema()), tool("d", schema())]),
        ];
        assert_eq!(listed(&modules).iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["a_d"]);
        // A call of the shared tool never reaches the module; the others find their declarations.
        assert!(find(&modules, "list_conversations").is_none() && find(&modules, "list_recordings").is_none());
        assert_eq!(find(&modules, "a_b_c").map(|tools| tools.len()), Some(2));
        assert_eq!(reasons(&modules, "list").len(), 4);
        assert_eq!(reasons(&modules, "a-b"), [("a_b_c".to_string(), "its name is declared more than once".to_string())]);
    }

    #[test]
    fn arguments_are_checked_against_the_schema() {
        let modules = [module("m", ModuleState::Active, vec![tool("t", schema())])];
        let declared = declared(&modules);
        let listed = declared["m_t"][0].listed.as_ref().unwrap();
        let (account, args) = arguments(listed, json!({"account": "m:a", "conversation": "c", "from": 5})).unwrap();
        // The module checks the types of its own arguments; `account` is not among them.
        assert_eq!((account.as_deref(), args), (Some("m:a"), json!({"conversation": "c", "from": 5})));
        for (bad, why) in [
            (json!({"conversation": "c", "to": "x"}), "unknown parameter"),
            (json!({"from": "x"}), "conversation is required"),
            (json!({"conversation": 5}), "conversation must be text"),
            (json!({"conversation": "c", "account": 1}), "account must be text"),
            (json!(["c"]), "JSON object"),
            (json!({"conversation": "x".repeat(MAX_ARGS)}), "KiB"),
        ] {
            let result = arguments(listed, bad.clone());
            assert!(matches!(&result, Err(ToolError::InvalidArgs(message)) if message.contains(why)), "{bad}: {:?}", result.map(|_| ()));
        }
    }
}
