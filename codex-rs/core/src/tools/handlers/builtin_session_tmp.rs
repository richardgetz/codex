use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::builtin_session_tmp_spec::TOOL_CREATE;
use crate::tools::handlers::builtin_session_tmp_spec::TOOL_LIST;
use crate::tools::handlers::builtin_session_tmp_spec::TOOL_REGISTER;
use crate::tools::handlers::builtin_session_tmp_spec::TOOL_RETAIN;
use crate::tools::handlers::builtin_session_tmp_spec::TOOL_SCHEMA;
use crate::tools::handlers::builtin_session_tmp_spec::session_tmp_namespace_spec;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_session_tmp::Retention;
use codex_session_tmp::TempKind;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde_json::Value;
use std::path::PathBuf;

const SESSION_TMP_NAMESPACE: &str = "session_tmp";
const MAX_ARGUMENT_BYTES: usize = 32 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

pub(crate) struct BuiltinSessionTmpHandler {
    tool_name: &'static str,
}

impl BuiltinSessionTmpHandler {
    pub(crate) const fn new(tool_name: &'static str) -> Self {
        Self { tool_name }
    }
}

impl ToolExecutor<ToolInvocation> for BuiltinSessionTmpHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(SESSION_TMP_NAMESPACE, self.tool_name)
    }

    fn spec(&self) -> ToolSpec {
        session_tmp_namespace_spec()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl BuiltinSessionTmpHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            payload,
            tool_name,
            ..
        } = invocation;
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "session_tmp handler received unsupported payload".to_string(),
            ));
        };
        if arguments.len() > MAX_ARGUMENT_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "session_tmp arguments exceed the {MAX_ARGUMENT_BYTES}-byte limit"
            )));
        }
        let args: Value = parse_arguments(&arguments)?;
        let Some(manager) = session.session_tmp.as_ref() else {
            return Err(FunctionCallError::RespondToModel(
                "session temporary storage is disabled; enable [session_tmp].enabled = true in config.toml"
                    .to_string(),
            ));
        };
        let tool_name = tool_name.name.as_str();
        let result = match tool_name {
            TOOL_CREATE => create(manager, &args).and_then(json_value),
            TOOL_REGISTER => register(manager, &args).and_then(json_value),
            TOOL_LIST => manager
                .list()
                .map_err(|error| error.to_string())
                .and_then(json_value),
            TOOL_RETAIN => retain(manager, &args).and_then(json_value),
            TOOL_SCHEMA => Ok(serde_json::json!({
                "namespace": SESSION_TMP_NAMESPACE,
                "retention": {
                    "session": "remove when the owning session ends",
                    "manual": "keep until explicit cleanup",
                    "ttl": "ttl:<seconds>, with <seconds> replaced by a non-negative integer",
                },
                "lineage": "Paths under the returned agent_root belong to the current session and thread.",
                "cleanup": "Session retention is removed when the root session ends; manual retention requires user cleanup or stale reap.",
            })),
            _ => Err(format!("unknown session_tmp tool: {tool_name}")),
        }
        .map_err(FunctionCallError::RespondToModel)?;
        let result_text = serde_json::to_string_pretty(&result)
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        if result_text.len() > MAX_OUTPUT_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "session_tmp output exceeds the {MAX_OUTPUT_BYTES}-byte limit; narrow the listing"
            )));
        }
        Ok(boxed_tool_output(FunctionToolOutput::from_text(
            result_text,
            Some(true),
        )))
    }
}

fn json_value<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}

impl CoreToolRuntime for BuiltinSessionTmpHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

fn create(
    manager: &codex_session_tmp::SessionTmpManager,
    args: &Value,
) -> Result<codex_session_tmp::TempEntry, String> {
    let purpose = args
        .get("purpose")
        .and_then(Value::as_str)
        .filter(|purpose| !purpose.trim().is_empty())
        .ok_or_else(|| "session_tmp.create requires a non-empty purpose".to_string())?;
    let name = args.get("name").and_then(Value::as_str);
    let retention = retention_arg(args)?;
    let kind = match args
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("directory")
    {
        "file" => TempKind::File,
        "directory" => TempKind::Directory,
        value => return Err(format!("unsupported session_tmp kind: {value}")),
    };
    manager
        .create(name, purpose, retention, kind)
        .map_err(|error| error.to_string())
}

fn register(
    manager: &codex_session_tmp::SessionTmpManager,
    args: &Value,
) -> Result<codex_session_tmp::TempEntry, String> {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "session_tmp.register requires path".to_string())?;
    let purpose = args
        .get("purpose")
        .and_then(Value::as_str)
        .filter(|purpose| !purpose.trim().is_empty())
        .ok_or_else(|| "session_tmp.register requires a non-empty purpose".to_string())?;
    manager
        .register(&PathBuf::from(path), purpose, retention_arg(args)?)
        .map_err(|error| error.to_string())
}

fn retain(
    manager: &codex_session_tmp::SessionTmpManager,
    args: &Value,
) -> Result<codex_session_tmp::TempEntry, String> {
    let entry_id = args
        .get("entry_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "session_tmp.retain requires entry_id".to_string())?;
    manager
        .set_retention(entry_id, retention_arg(args)?)
        .map_err(|error| error.to_string())
}

fn retention_arg(args: &Value) -> Result<Retention, String> {
    Retention::parse(
        args.get("retention")
            .and_then(Value::as_str)
            .unwrap_or("session"),
    )
    .map_err(|error| error.to_string())
}
