// Control: the approved shape. Must stay completely silent.
#[derive(Debug, Clone, Serialize)]
pub struct WrappedConfig {
    pub api_key: sanvi_telemetry::Redacted<String>,
}
