mod bar;

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
    // Re-serialise so the wire format and the parsed enum cannot drift.
    let wire = match cmd {
        Command::Ping => "ping".to_string(),
        Command::Workspaces => "workspaces".to_string(),
        Command::FocusWorkspace(id) => format!("workspace {id}"),
        Command::ToggleSpecial => "toggle-special".to_string(),
        Command::Quit => "quit".to_string(),
        Command::Duplicate => return Ok(()),
    };

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

            bar::open_when_ready(cx);

            // IPC is blocking, so it gets its own thread. It needs the tokio
            // runtime for anything async, and the guard is still alive here.
            std::thread::spawn(move || {
                let _entered = runtime.enter();
                serve(compositor, handle_default);
            });
        });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_wire_format_round_trips() {
        // Whatever the client serialises must decode back to the same command,
        // or a keybind silently does the wrong thing.
        for cmd in [
            Command::Ping,
            Command::Workspaces,
            Command::FocusWorkspace(4),
            Command::FocusWorkspace(-99),
            Command::ToggleSpecial,
            Command::Quit,
        ] {
            let wire = match cmd {
                Command::Ping => "ping".to_string(),
                Command::Workspaces => "workspaces".to_string(),
                Command::FocusWorkspace(id) => format!("workspace {id}"),
                Command::ToggleSpecial => "toggle-special".to_string(),
                Command::Quit => "quit".to_string(),
                Command::Duplicate => unreachable!(),
            };
            assert_eq!(decode(&wire).unwrap(), cmd, "round-trip failed for {wire}");
        }
    }
}