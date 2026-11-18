#[cfg(not(target_arch = "wasm32"))]
#[path = "import_release/build.rs"]
mod build;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::Path;

    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [backup, output, published_at_ms] = args.as_slice() else {
        return Err(
            "usage: import-release <sqlite-backup> <new-output-dir> <published-at-ms>".into(),
        );
    };
    let at = published_at_ms
        .to_str()
        .ok_or("invalid publication timestamp")?
        .parse()?;
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fusiform-module/data");
    for (table, rows) in build::import(Path::new(backup), &data, Path::new(output), at)? {
        println!("{table}: {rows}");
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn main() {}
