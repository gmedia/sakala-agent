use uuid::Uuid;

#[must_use]
pub fn new_correlation_id() -> Uuid {
    Uuid::new_v4()
}
