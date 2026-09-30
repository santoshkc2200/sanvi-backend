// Control: a reasoned exemption whose class rule holds (no Debug, no
// Serialize derives). Must stay completely silent.
#[derive(Clone)]
pub struct FineConfig {
    pub internal_secret: String,
}
