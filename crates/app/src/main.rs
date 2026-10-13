mod fonts;
mod surfaces;
mod theme;

use gpui::App;
use services::{AppState, Command, decode, handle_default, send, serve};


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

    // Hyprland owns the keybinds, so the user's hyprland.conf invokes us:
    //     bind = $mod, 1, exec, lume workspace 1
    // That path is the CLIENT half of the IPC protocol. If a shell is already
    // running, forward and exit. If not, start one.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        let cmd = decode(&args.join(" "))?;
        return run_client(cmd);
    }

    run_shell()
}

/// Forward a command to a running shell, or report that there is none.
fn run_client(cmd: Command) -> anyhow::Result<()> {
    // Re-serialised from the enum itself, so the wire format and the parsed
    // type cannot drift. `Command::wire` is the single definition; the test
    // below checks it against `decode`.
    let wire = cmd.wire();

    match send(&wire)? {
        Some(reply) => {
            if reply.starts_with("error:") {
                eprintln!("lume: {reply}");
                std::process::exit(1);
            }
            println!("{reply}");
            Ok(())
        }
        // No auto-spawn. Launching a second shell over a live one would give
        // the user two bars fighting over the same exclusive zone.
        None => {
            eprintln!("lume is not running");
            std::process::exit(1);
        }
    }
}

fn run_shell() -> anyhow::Result<()> {
    // Enter the Tokio runtime for the whole process, not block_on it. GPUI's
    // executor threads do not run a Tokio reactor, so anything awaiting a
    // socket read in a bare GPUI task would hang.
    //
    // The guard must be HELD until exit — dropping it early is the bug.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    let _runtime_guard = runtime.enter();

    // Created before the app so the IPC thread can be handed a sender and the
    // UI thread the receiver. See services::intent for why intents exist at
    // all rather than a global registry of window handles.
    let (sender, mut receiver) = services::intent::channel();
    let shared = std::sync::Arc::new(sender.clone());
    // So a click handler anywhere can raise an intent without a handle to
    // main. Same shape as config::current.
    services::intent::install(sender);

    let app = gpui_platform::application();

    app.with_quit_mode(gpui::QuitMode::LastWindowClosed)
        .run(|cx: &mut App| {
            // The compositor propagates: no compositor means no shell, and
            // saying so clearly beats a shell that silently does nothing.
            let state = match AppState::new(runtime.handle().clone()) {
                Ok(state) => state,
                Err(e) => {
                    eprintln!("lume: {e:#}");
                    eprintln!("lume needs Hyprland with zwlr_layer_shell_v1 support.");
                    std::process::exit(1);
                }
            };

            let compositor = state.compositor.clone();
            cx.set_global(state);

            // Before any surface: a window created before the faces are
            // registered would lay out with a fallback face and only repaint on
            // some later change.
            let loaded = fonts::load(cx);
            tracing::info!(faces = loaded, "fonts loaded");

            surfaces::Surfaces::open_when_ready(cx);

            // Drain intents on the UI thread. This is the only place that
            // touches the UI, and it is the reason the channel exists.
            cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                // The receiver closes only when every sender is dropped,
                // which happens when the IPC thread exits. Nothing else ends
                // this loop.
                while let Some(intent) = receiver.recv().await {
                    tracing::info!(?intent, "intent received");
                    cx.update(|cx| surfaces::apply_intent(cx, intent));
                }
            })
            .detach();

            // IPC is blocking, so it gets its own thread. It needs the tokio
            // runtime for anything async, and the guard is still alive here.
            std::thread::spawn(move || {
                let _entered = runtime.enter();
                serve(compositor, move |cmd, comp| {
                    services::handle(cmd, comp, Some(&shared))
                });
            });
        });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_wire_format_round_trips() {
        // Every command the CLI can be asked for. `Duplicate` is excluded: it
        // is what decoding reports, never something a user sends.
        for cmd in [
            Command::Ping,
            Command::Workspaces,
            Command::FocusWorkspace(4),
            Command::FocusWorkspace(-99),
            Command::ToggleSpecial,
            Command::Quit,
            Command::Panel(services::intent::Side::Left, None),
            Command::Panel(services::intent::Side::Right, None),
            Command::Panel(services::intent::Side::Left, Some(true)),
            Command::Panel(services::intent::Side::Right, Some(false)),
            Command::Picker(services::intent::PickerOp::Toggle),
            Command::Picker(services::intent::PickerOp::Open),
            Command::Picker(services::intent::PickerOp::Close),
            Command::Drawer(
                Some("volume".into()),
                services::intent::DrawerOp::Toggle,
            ),
            Command::Drawer(
                Some("network".into()),
                services::intent::DrawerOp::Open,
            ),
            Command::Drawer(None, services::intent::DrawerOp::Close),
            Command::Launcher(services::intent::LauncherOp::Open),
            Command::Launcher(services::intent::LauncherOp::Close),
        ] {
            let wire = cmd.wire();
            assert_eq!(
                decode(&wire).unwrap(),
                cmd,
                "round-trip failed for {wire:?}"
            );
        }
    }

    #[test]
    fn panel_rejects_a_bad_side_rather_than_guessing() {
        assert!(decode("panel up").is_err());
        assert!(decode("panel left maybe").is_err());
        assert!(decode("panel").is_err());
        assert!(decode("panel left on").is_ok());
        assert!(decode("panel right off").is_ok());
    }
}
