//! Translation of `shell/api/main/api.go`.
//!
//! Go used reflection to enumerate plugger methods at runtime; the Rust
//! port enumerates them explicitly: each action module's `install`
//! function registers its handlers with the actor.

use std::collections::HashMap;
use std::sync::Arc;

use crate::models::action::ExtendedField;
use crate::models::core::ICore;

use super::actions;

fn clone_model_extender(
    model_extender: &HashMap<String, HashMap<String, ExtendedField>>,
) -> HashMap<String, HashMap<String, ExtendedField>> {
    model_extender.clone()
}

/// Mirrors `PlugAll(core, modelExtender)`.
pub fn plug_all(
    app: Arc<dyn ICore>,
    model_extender: &HashMap<String, HashMap<String, ExtendedField>>,
) {
    actions::auth::install(app.clone());
    actions::creature::install(app.clone(), clone_model_extender(model_extender));
    actions::dummy::install(app.clone());
    actions::gateway::install(app.clone());
    actions::program::install(app.clone());
    actions::store::install(app.clone());
}
