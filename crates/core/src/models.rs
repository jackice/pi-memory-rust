//! Data models: match the TS `src/core/types.ts`, same fields and semantics.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: String,
    pub content: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    Working,
    Episodic,
    Fact,
    Preference,
    Instruction,
    Timeline,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Episodic => "episodic",
            Self::Fact => "fact",
            Self::Preference => "preference",
            Self::Instruction => "instruction",
            Self::Timeline => "timeline",
        }
    }
}

/// working_memory record (matches WorkingMemoryRecord)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkingMemoryRecord {
    pub id: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embed_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub session_id: String,
    pub project_scope: String,
    pub importance: f64,
    pub memory_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consolidated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
}

/// episodic_memory record (matches EpisodicMemoryRecord)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpisodicMemoryRecord {
    pub id: String,
    pub content: String,
    pub summary_of: String,
    pub session_id: String,
    pub project_scope: String,
    pub importance: f64,
    pub tier: i64,
    pub memory_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
}

/// facts record (matches FactRecord)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactRecord {
    pub fact_id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_memory_id: Option<String>,
    pub session_id: String,
    /// Project scope (empty for legacy migrated data; queries treat empty as visible for compatibility)
    pub project_scope: String,
    pub timestamp: String,
}

/// preferences record (matches PreferenceRecord)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreferenceRecord {
    pub id: String,
    pub preference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_snippet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_memory_id: Option<String>,
    pub session_id: String,
    /// Project scope (empty for legacy migrated data; queries treat empty as visible for compatibility)
    pub project_scope: String,
    pub created_at: String,
}

/// instructions record (matches InstructionRecord)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstructionRecord {
    pub id: String,
    pub instruction: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_snippet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_memory_id: Option<String>,
    pub session_id: String,
    /// Project scope (empty for legacy migrated data; queries treat empty as visible for compatibility)
    pub project_scope: String,
    pub created_at: String,
}

/// timeline record (matches TimelineEventRecord)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineEventRecord {
    pub event_id: String,
    pub date: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_memory_id: Option<String>,
    pub session_id: String,
    /// Project scope (empty for legacy migrated data; queries treat empty as visible for compatibility)
    pub project_scope: String,
    pub created_at: String,
}

/// memory_embeddings record (matches MemoryEmbeddingRecord)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEmbeddingRecord {
    pub memory_kind: MemoryKind,
    pub memory_id: String,
    pub embedding: Vec<f64>,
    pub model: String,
}

/// session_state record (matches SessionStateRecord)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStateRecord {
    pub session_id: String,
    pub project_scope: String,
    pub last_retained_user_turn: i64,
    pub has_recalled_for_first_turn: bool,
}

