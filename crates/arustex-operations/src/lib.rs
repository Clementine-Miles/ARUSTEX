//! The static operation registry.
//!
//! Workflow agents can only act through the operations compiled into this crate.
//! There is no shell, PTY, generic HTTP client, filesystem access, dynamic loading or
//! plugin protocol. Every operation has a typed input (unknown fields are rejected), a
//! declared capability and effect class checked by [`arustex_policy::Policy`], an
//! execution timeout and an output size limit. Anything that fails a check is denied.

mod ops;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use arustex_core::domain::agent::SessionId;
use arustex_core::ports::{RecordSource, ReviewSink};
use arustex_core::{CoreError, CoreResult};
use arustex_policy::{Capability, Decision, EffectClass, OperationClaim, Policy, PolicyContext};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use ops::*;

/// Maximum accepted size of a serialized operation input.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Everything an operation may use. It holds no network or filesystem handles.
#[derive(Clone)]
pub struct OperationContext {
    pub session_id: SessionId,
    pub policy: PolicyContext,
    pub records: Arc<dyn RecordSource>,
    pub review: Arc<dyn ReviewSink>,
}

impl std::fmt::Debug for OperationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperationContext")
            .field("session_id", &self.session_id)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

/// Static metadata of an operation.
#[derive(Debug, Clone, Serialize)]
pub struct OperationDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub capability: Capability,
    pub effect: EffectClass,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl OperationDescriptor {
    fn claim(&self) -> OperationClaim<'_> {
        OperationClaim {
            name: self.name,
            capability: self.capability,
            effect: self.effect,
        }
    }
}

/// A tool definition handed to a model provider.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// An operation with a typed input.
#[async_trait]
pub trait TypedOperation: Send + Sync + 'static {
    type Input: DeserializeOwned + JsonSchema + Send;
    fn descriptor(&self) -> &OperationDescriptor;
    async fn call(
        &self,
        ctx: &OperationContext,
        input: Self::Input,
    ) -> CoreResult<serde_json::Value>;
}

#[async_trait]
trait ErasedOperation: Send + Sync {
    fn descriptor(&self) -> &OperationDescriptor;
    fn input_schema(&self) -> serde_json::Value;
    async fn call_json(
        &self,
        ctx: &OperationContext,
        input: serde_json::Value,
    ) -> CoreResult<serde_json::Value>;
}

#[async_trait]
impl<T: TypedOperation> ErasedOperation for T {
    fn descriptor(&self) -> &OperationDescriptor {
        TypedOperation::descriptor(self)
    }

    fn input_schema(&self) -> serde_json::Value {
        let schema = schemars::schema_for!(T::Input);
        let mut value = serde_json::to_value(schema).unwrap_or_default();
        if let Some(map) = value.as_object_mut() {
            map.remove("$schema");
            map.remove("title");
        }
        value
    }

    async fn call_json(
        &self,
        ctx: &OperationContext,
        input: serde_json::Value,
    ) -> CoreResult<serde_json::Value> {
        let typed: T::Input = serde_json::from_value(input).map_err(|e| {
            CoreError::invalid(format!("invalid input for {}: {e}", self.descriptor().name))
        })?;
        self.call(ctx, typed).await
    }
}

/// The result of an invocation as recorded in the transcript.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Invocation {
    pub operation: String,
    pub effect: EffectClass,
    pub output: serde_json::Value,
    pub truncated: bool,
}

/// The compiled-in set of operations plus the policy that guards them.
pub struct Registry {
    ops: BTreeMap<&'static str, Box<dyn ErasedOperation>>,
    policy: Policy,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("ops", &self.ops.keys().collect::<Vec<_>>())
            .field("policy", &self.policy)
            .finish()
    }
}

impl Registry {
    /// The standard registry: every operation this build ships.
    pub fn standard(policy: Policy) -> Self {
        let mut r = Self {
            ops: BTreeMap::new(),
            policy,
        };
        r.add(SearchRecords::new());
        r.add(ReadRecords::new());
        r.add(SummarizeScope::new());
        r.add(FieldStatistics::new());
        r.add(ValidateGraph::new());
        r.add(CompareOutputs::new());
        r.add(RecordFinding::new());
        r.add(ProposeReportDraft::new());
        r.add(ProposeStatusUpdate::new());
        r
    }

    fn add<T: TypedOperation>(&mut self, op: T) {
        let name = TypedOperation::descriptor(&op).name;
        let previous = self.ops.insert(name, Box::new(op));
        assert!(previous.is_none(), "duplicate operation {name}");
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn descriptors(&self) -> impl Iterator<Item = &OperationDescriptor> {
        self.ops.values().map(|op| op.descriptor())
    }

    /// Tool definitions visible to a role, in a stable order.
    pub fn tools_for(&self, role: arustex_core::domain::agent::AgentRole) -> Vec<ToolSpec> {
        self.ops
            .values()
            .filter(|op| self.policy.visible(role, &op.descriptor().claim()))
            .map(|op| ToolSpec {
                name: op.descriptor().name.to_owned(),
                description: op.descriptor().description.to_owned(),
                input_schema: op.input_schema(),
            })
            .collect()
    }

    /// Checks policy, validates input, runs the operation under its timeout and caps
    /// the output. `calls_so_far` is the number of earlier calls in this session.
    pub async fn invoke(
        &self,
        ctx: &OperationContext,
        name: &str,
        input: serde_json::Value,
        calls_so_far: u32,
    ) -> CoreResult<Invocation> {
        let Some(op) = self.ops.get(name) else {
            return Err(CoreError::denied(format!("unknown operation {name:?}")));
        };
        let desc = op.descriptor();
        match self
            .policy
            .authorize(&ctx.policy, &desc.claim(), calls_so_far)
        {
            Decision::Deny { reason } => return Err(CoreError::denied(reason)),
            Decision::Allow | Decision::AllowAsApprovalRequest => {}
        }
        if input.to_string().len() > MAX_INPUT_BYTES {
            return Err(CoreError::LimitExceeded(format!(
                "input for {name} exceeds {MAX_INPUT_BYTES} bytes"
            )));
        }
        let output = tokio::time::timeout(desc.timeout, op.call_json(ctx, input))
            .await
            .map_err(|_| CoreError::Timeout(format!("operation {name} timed out")))??;
        let text = output.to_string();
        let (output, truncated) = if text.len() > desc.max_output_bytes {
            let mut end = desc.max_output_bytes;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            (
                serde_json::json!({ "truncated": true, "partial_json": &text[..end] }),
                true,
            )
        } else {
            (output, false)
        };
        tracing::debug!(operation = name, session = %ctx.session_id, truncated, "operation completed");
        Ok(Invocation {
            operation: name.to_owned(),
            effect: desc.effect,
            output,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests;
