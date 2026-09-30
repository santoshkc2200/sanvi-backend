// Planted defect 1: a secret-shaped field nobody classified. The gate must
// name file, line, struct and field.
#[derive(Debug, Clone, Serialize)]
pub struct RogueConfig {
    pub api_key: String,
}
