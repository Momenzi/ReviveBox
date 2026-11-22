//! Desktop launcher executable entrypoint for **ReviveBox** (*Old Games Second Chance*).

#![forbid(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    if let Some(arg) = args.next() {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("ReviveBox {} (ogsc-launcher)", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                println!(
                    "ReviveBox — Old Games Second Chance {}\n\nUsage: ogsc-launcher [--help | --version]",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            _ => {}
        }
    }

    ogsc_ui::run_desktop_app()?;
    Ok(())
}
