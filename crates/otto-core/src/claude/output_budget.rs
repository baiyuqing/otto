//! Claude output budgets reuse the canonical model catalog; unknown IDs never
//! inherit a guessed family limit.
use crate::config::ModelLimits;

const INITIAL_TOKENS: u32 = 16_384;
const UNKNOWN_TOKENS: u32 = 4_096;

#[derive(Debug, Clone, Copy)]
pub struct OutputBudget {
    pub initial: u32,
    pub ceiling: u32,
}
impl OutputBudget {
    pub fn for_model(model: &str) -> Self {
        let model = model.strip_prefix("anthropic/").unwrap_or(model);
        let limits = ModelLimits::for_model(&format!("anthropic/{model}"));
        let ceiling = u32::try_from(limits.max_output_tokens)
            .ok()
            .filter(|limit| limits.known && *limit > 0)
            .unwrap_or(UNKNOWN_TOKENS);
        Self {
            initial: INITIAL_TOKENS.min(ceiling),
            ceiling,
        }
    }
    /// Every recovery grows strictly, at most doubling, never past the known
    /// model ceiling. An unknown model's fallback is also its ceiling.
    pub fn grow(self, current: u32) -> Option<u32> {
        (current > 0 && current < self.ceiling).then(|| current.saturating_mul(2).min(self.ceiling))
    }
}
