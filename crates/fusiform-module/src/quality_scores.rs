//! Operator judgments joined to the current served identity, never stored facts.

use std::collections::BTreeMap;

const DOCUMENT: &str = include_str!("../data/quality-scores.json");

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityScore {
    pub model: String,
    pub elo: Option<serde_json::Number>,
    pub eq: Option<serde_json::Number>,
}

#[derive(Debug, Default)]
pub struct QualityScores {
    pub graded_at_ms: i64,
    pub rows: BTreeMap<String, QualityScore>,
}

/// Count malformed rows without discarding independent, valid grades.
pub fn load_reporting_rejects(doc: &str) -> (QualityScores, Vec<String>) {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Doc {
        graded_by: String,
        graded_at_ms: i64,
        purpose: String,
        rows: Vec<serde_json::Value>,
    }
    let doc: Doc = match serde_json::from_str(doc) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                QualityScores::default(),
                vec![format!("quality scores unreadable: {error}")],
            )
        }
    };
    if doc.graded_by != "operator" || doc.graded_at_ms < 0 || doc.purpose.trim().is_empty() {
        return (
            QualityScores::default(),
            vec![
                "quality scores require graded_by operator, nonnegative graded_at_ms and a purpose"
                    .into(),
            ],
        );
    }
    let mut scores = QualityScores {
        graded_at_ms: doc.graded_at_ms,
        ..QualityScores::default()
    };
    let mut rejects = Vec::new();
    for (index, raw) in doc.rows.into_iter().enumerate() {
        let row: QualityScore = match serde_json::from_value(raw) {
            Ok(row) => row,
            Err(error) => {
                rejects.push(format!(
                    "quality score row {} did not parse: {error}",
                    index + 1
                ));
                continue;
            }
        };
        let valid_key = row
            .model
            .split_once('/')
            .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
            && !row.model.chars().any(char::is_whitespace);
        let reason = if !valid_key {
            Some("model must be provider/model without whitespace")
        } else if row.elo.is_none() && row.eq.is_none() {
            Some("both scores are null")
        } else if scores.rows.contains_key(&row.model) {
            Some("duplicate model key")
        } else {
            None
        };
        if let Some(reason) = reason {
            rejects.push(format!("quality score {}: {reason}", row.model));
        } else {
            scores.rows.insert(row.model.clone(), row);
        }
    }
    (scores, rejects)
}

pub fn scores() -> &'static QualityScores {
    if let Some(scores) = PLANTED.with(|planted| planted.get()) {
        return scores;
    }
    static SCORES: std::sync::OnceLock<QualityScores> = std::sync::OnceLock::new();
    SCORES.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

thread_local! {
    static PLANTED: std::cell::Cell<Option<&'static QualityScores>> = const { std::cell::Cell::new(None) };
}

#[doc(hidden)]
#[must_use]
pub struct Planted {
    previous: Option<&'static QualityScores>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

/// Use fixture grades on the calling thread until the guard is dropped.
/// Normal serving uses the embedded file; only tests call this fixture seam.
#[doc(hidden)]
pub fn plant(doc: &str) -> Planted {
    let (scores, rejects) = load_reporting_rejects(doc);
    assert!(rejects.is_empty(), "{rejects:?}");
    let scores = Box::leak(Box::new(scores));
    Planted {
        previous: PLANTED.with(|planted| planted.replace(Some(scores))),
        _thread_bound: std::marker::PhantomData,
    }
}

impl Drop for Planted {
    fn drop(&mut self) {
        PLANTED.with(|planted| planted.set(self.previous));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_scores_load_without_rejects() {
        let (_, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
    }

    #[test]
    fn decimal_tokens_round_trip_without_float_conversion() {
        let doc = r#"{"graded_by":"operator","graded_at_ms":1,"purpose":"routing","rows":[{"model":"a/m","elo":1284,"eq":1599.800}]}"#;
        let (scores, rejects) = load_reporting_rejects(doc);
        assert!(rejects.is_empty());
        assert_eq!(
            serde_json::to_string(scores.rows["a/m"].eq.as_ref().unwrap()).unwrap(),
            "1599.800"
        );
        let previous = self::scores();
        let guard = plant(doc);
        assert_eq!(
            self::scores().rows["a/m"].elo.as_ref().unwrap().to_string(),
            "1284"
        );
        drop(guard);
        assert!(std::ptr::eq(self::scores(), previous));
    }

    #[test]
    fn malformed_and_duplicate_rows_are_rejected() {
        let (scores, rejects) = load_reporting_rejects(
            r#"{"graded_by":"operator","graded_at_ms":1,"purpose":"routing","rows":[{"model":"a/m","elo":1,"eq":null},{"model":"a/m","elo":2,"eq":3},{"model":"a/n","elo":null,"eq":null},{"model":"bad","elo":1},{"model":"b/m","elo":"1"},false]}"#,
        );
        assert_eq!(scores.rows.len(), 1);
        assert_eq!(rejects.len(), 5);
        assert!(rejects[0].contains("duplicate"));
        for doc in [
            "not json",
            r#"{"graded_by":"upstream","graded_at_ms":1,"purpose":"routing","rows":[]}"#,
        ] {
            let (scores, rejects) = load_reporting_rejects(doc);
            assert!(scores.rows.is_empty());
            assert_eq!(rejects.len(), 1);
        }
    }
}
