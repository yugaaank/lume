mod bar;

use gpui::App;

fn main() -> anyhow::Result<()> {
    // Relative Exec= lines in .desktop files resolve against cwd. Set this
    // before you have a launcher so you never debug it later.
    std::env::set_current_dir(std::env::var("HOME").unwrap_or_else(|_| ".".into())).ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let app = gpui_platform::application();
    // QuitMode variants are Default | LastWindowClosed | Explicit.
    // There is no OnLastWindowClosed.
    app.with_quit_mode(gpui::QuitMode::LastWindowClosed)
        .run(|cx: &mut App| {
            bar::open_when_ready(cx);
        });

    Ok(())
}