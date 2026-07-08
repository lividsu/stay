mod shared;

#[cfg(unix)]
mod unix_app;

fn main() {
    #[cfg(unix)]
    {
        if let Err(err) = unix_app::run() {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }

    #[cfg(not(unix))]
    {
        eprintln!("Stay V1 only supports Linux.");
        std::process::exit(1);
    }
}
