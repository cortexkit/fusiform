//! The embedded creators document and its local adapter.

pub use fusiform_curate::creators::*;

pub fn load_reporting_rejects() -> (Creators, Vec<String>) {
    parse_document(CREATORS)
}

const CREATORS: &str = include_str!("../data/model-creators.json");

/// Load the table, discarding the reject report.
pub fn load() -> Creators {
    load_reporting_rejects().0
}

/// The table, parsed once.
///
/// Per process rather than per request: the file is embedded, so a re-parse on
/// every catalog read would cost the same answer repeatedly.
pub fn creators() -> &'static Creators {
    static PARSED: std::sync::OnceLock<Creators> = std::sync::OnceLock::new();
    PARSED.get_or_init(load)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_table_loads_without_rejects() {
        let (creators, rejects) = load_reporting_rejects();
        assert!(
            rejects.is_empty(),
            "the shipped creator table must parse cleanly: {rejects:?}"
        );
        assert!(
            !creators.is_empty(),
            "an empty table would make every assertion below vacuous"
        );
        assert_eq!(
            creators.get("glm").map(String::as_str),
            Some("zai"),
            "the family ALF's routing defect turns on must resolve"
        );
    }
}
