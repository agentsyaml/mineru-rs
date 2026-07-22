use mineru::command::{RunContext, run_cli};

#[tokio::main]
async fn main() {
    let context = match std::env::current_exe()
        .map(|path| {
            path.with_file_name(if cfg!(windows) {
                "mineru-office-convert.exe"
            } else {
                "mineru-office-convert"
            })
        })
        .map_err(|error| error.to_string())
        .and_then(|path| {
            RunContext::with_office_executable(path).map_err(|error| error.to_string())
        }) {
        Ok(context) => context,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    let code = run_cli(std::env::args_os().skip(1).collect(), context).await;
    if code != 0 {
        std::process::exit(code);
    }
}
