// TODO: are these too trivial to keep?

pub fn to_str(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

pub fn to_string(bytes: &[u8]) -> String {
    String::from(self::to_str(bytes))
}
