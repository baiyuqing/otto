use std::collections::BTreeMap;
use std::path::PathBuf;

use super::*;
use axum::body::Body;
use axum::http::Request;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::StreamExt;
use otto::fixtures;
use otto::runtime::builder::{Builder, Runner, RuntimeInfo, SharedSession};
use otto_core::model::{Block, BlockType, Message, Role, Usage};
use otto_core::provider::{
    Provider, ProviderError, Request as ProviderRequest, Response as ProviderResponse, StreamEvent,
    StreamSink,
};
use otto_core::session::{CURRENT_VERSION, Header, ListResult, Session, SessionInfo};
use otto_core::wire::sse::{Frame, parse_frames};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::watch;
use tower::ServiceExt;

mod approval_cases;
mod doubles;
mod endpoint_cases;
mod harness;
mod local_command_cases;
mod memory_route_cases;
mod model_config_cases;
mod observer_cases;
mod permanent_delete_cases;
mod queue_cases;
mod queue_provenance_cases;
mod remote_listener_cases;
mod scoped_route_cases;
mod session_lifecycle_cases;
mod sessions_cases;
mod skills_cases;
mod status_cases;
mod steer_interrupt_cases;
mod turns_cases;
mod wake_cases;
mod workspace_cases;
mod workspace_diff_cases;

use approval_cases::*;
use doubles::*;
use harness::*;
use queue_cases::*;
use status_cases::*;
use wake_cases::*;
