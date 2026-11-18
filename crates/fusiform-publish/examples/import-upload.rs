#[cfg(not(target_arch = "wasm32"))]
#[path = "import_upload/client.rs"]
mod client;

#[cfg(not(target_arch = "wasm32"))]
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    use std::process::ExitCode;

    match client::run(
        std::env::args_os().skip(1),
        std::env::var("MODELS_IMPORT_TOKEN"),
    )
    .await
    {
        Ok(reply) => {
            println!("{reply}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {}
