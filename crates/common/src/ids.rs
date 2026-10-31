/// A fresh ULID as a string. Lexically sortable by creation time.
pub fn new_id() -> String {
    ulid::Ulid::generate().to_string()
}
