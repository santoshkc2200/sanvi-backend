// Planted defect 2: the field IS exempted, but as a class whose mechanical
// rule its owning struct violates — config-carrier forbids Debug/Serialize,
// and this struct derives both. The exemption must not launder the field.
#[derive(Debug, Clone, Serialize)]
pub struct LeakyCarrier {
    pub client_secret: String,
}
