//! Typed image admission before persistence or provider I/O.
use super::*;

impl<P: Provider, T: ToolExecutor, S: Session> Agent<P, T, S> {
    /// Validates attachments without persisting content or contacting a provider.
    pub fn validate_images(&self, images: &[Block]) -> Result<(), AgentError> {
        for image in images {
            if image.block_type != crate::model::BlockType::Image {
                return Err(AgentError::Other(
                    "Attachment must be a typed image block; remove it and attach an image again."
                        .into(),
                ));
            }
            image.validate().map_err(|_| {
                AgentError::Other(
                    "Invalid image attachment; attach a supported PNG, JPEG, GIF or WebP again."
                        .into(),
                )
            })?;
        }
        if images.is_empty() {
            return Ok(());
        }
        match self.options.provider_name.as_str() {
            "openai-compatible" | "chatgpt" => {},
            "claude" => return Err(AgentError::Other("This Claude transport does not support image input yet; switch to an image-capable OpenAI-compatible or ChatGPT model.".into())),
            _ => return Err(AgentError::Other("This provider does not support local image input; switch to an image-capable OpenAI-compatible or ChatGPT model.".into())),
        }
        let model = self.options.model.to_ascii_lowercase();
        if model.starts_with("gpt-3.5")
            || model == "gpt-4"
            || model.starts_with("gpt-4-0")
            || model.starts_with("gpt-4-32k")
            || model.starts_with("o1-mini")
            || model.starts_with("o1-preview")
            || model.starts_with("o3-mini")
            || model.contains("embedding")
            || model.starts_with("text-")
            || model.starts_with("babbage")
            || model.starts_with("davinci")
        {
            return Err(AgentError::Other("The selected model is text-only; switch to an image-capable model before submitting images.".into()));
        }
        self.preflight_ordinary_turn("", images, "", false)
    }
}
