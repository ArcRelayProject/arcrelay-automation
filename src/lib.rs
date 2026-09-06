mod engine;
mod model;
mod shell;
mod store;
mod validation;

pub use engine::*;
pub use model::*;
pub use shell::{run_shell, run_shell_literal};
pub use store::AutomationStore;
pub use validation::*;

pub type Result<T> = std::result::Result<T, AutomationError>;

#[derive(Debug, thiserror::Error)]
pub enum AutomationError {
    #[error("invalid automation: {0}")]
    Invalid(String),
    #[error("the configuration was modified in another window; reopen it before saving")]
    Conflict,
    #[error("automation or activity not found")]
    NotFound,
    #[error("automation database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("automation serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

impl AutomationError {
    /// Stable machine-readable category for transport and UI adapters.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "automation.invalid_argument",
            Self::Conflict => "automation.conflict",
            Self::NotFound => "automation.not_found",
            Self::Database(_) | Self::Json(_) => "automation.internal",
        }
    }
}

#[cfg(test)]
mod tests;
