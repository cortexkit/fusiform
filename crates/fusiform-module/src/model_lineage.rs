//! The embedded model lineage document and its local adapter.

pub use fusiform_curate::model_lineage::*;

const DOCUMENT: &str = include_str!("../data/model-lineage.json");

pub fn classifications() -> &'static ModelLineage {
    #[cfg(test)]
    if let Some(rows) = PLANTED.with(|planted| planted.get()) {
        return rows;
    }
    static ROWS: std::sync::OnceLock<ModelLineage> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

#[cfg(test)]
thread_local! {
    static PLANTED: std::cell::Cell<Option<&'static ModelLineage>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
#[must_use]
pub struct Planted {
    previous: Option<&'static ModelLineage>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

/// Use fixture classifications on the calling thread until the guard is dropped.
#[cfg(test)]
pub fn plant(doc: &str) -> Planted {
    let (rows, rejects) = load_reporting_rejects(doc);
    assert!(rejects.is_empty(), "{rejects:?}");
    let rows = Box::leak(Box::new(rows));
    Planted {
        previous: PLANTED.with(|planted| planted.replace(Some(rows))),
        _thread_bound: std::marker::PhantomData,
    }
}

#[cfg(test)]
impl Drop for Planted {
    fn drop(&mut self) {
        PLANTED.with(|planted| planted.set(self.previous));
    }
}
