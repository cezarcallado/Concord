use std::path::PathBuf;

use anyhow::{bail, Result};
use voxhub_server::{serve_until, Options};

const HELP: &str = "concord-server

USAGE: concord-server [OPTIONS]

  --data-dir <DIR>      config, database and livekit.yaml live here (default ./data)
  --livekit-bin <PATH>  livekit-server executable (default ./bin/livekit-server.exe)
  --no-livekit          do not start LiveKit (tests)
  --console             also log to stderr
  --no-diag             skip the connection diagnostics shown after start (IPv4 / IPv6, router,
                        Windows Firewall)
  --log-dir <DIR>       log directory (default %LOCALAPPDATA%\\concord\\logs\\hub)
  --reset-password <NAME>
                        forget NAME's personal password and print a one-time code to log in
                        with (then choose a new password); the server does not start
  --reset-owner         forget the server's owner (Dono): whoever then logs in from this PC
                        (127.0.0.1) becomes the new one; the server does not start. Does not
                        touch `owner` in config.toml
  -h, --help            show this help
";

struct Cli {
    opts: Options,
    console: bool,
    log_dir: Option<PathBuf>,
    reset_password: Option<String>,
    reset_owner: bool,
    no_diag: bool,
}

fn parse_args() -> Result<Cli> {
    let mut cli = Cli {
        opts: Options {
            data_dir: "data".into(),
            no_livekit: false,
            livekit_bin: "bin/livekit-server.exe".into(),
            bind_override: None,
        },
        console: false,
        log_dir: None,
        reset_password: None,
        reset_owner: false,
        no_diag: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| anyhow::anyhow!("{name} needs a value"));
        match a.as_str() {
            "--data-dir" => cli.opts.data_dir = value("--data-dir")?.into(),
            "--livekit-bin" => cli.opts.livekit_bin = value("--livekit-bin")?.into(),
            "--log-dir" => cli.log_dir = Some(value("--log-dir")?.into()),
            "--reset-password" => cli.reset_password = Some(value("--reset-password")?),
            "--reset-owner" => cli.reset_owner = true,
            "--no-livekit" => cli.opts.no_livekit = true,
            "--console" => cli.console = true,
            "--no-diag" => cli.no_diag = true,
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?} (see --help)"),
        }
    }
    Ok(cli)
}

fn main() -> Result<()> {
    let cli = parse_args()?;
    if let Some(name) = &cli.reset_password {
        // A host tool: no runtime, no LiveKit, nothing but the database (safe while the server runs).
        let msg = voxhub_server::admin::reset_password_cli(&cli.opts.data_dir, name)?;
        println!("{msg}");
        return Ok(());
    }
    if cli.reset_owner {
        let msg = voxhub_server::owner::reset_owner_cli(&cli.opts.data_dir)?;
        println!("{msg}");
        return Ok(());
    }
    let _log = vox_log::init("hub", cli.log_dir.as_deref(), cli.console);
    let rt = voxhub_server::runtime()?;
    // v1.0.2: what the connection diagnostics check (in the background, after the start).
    let livekit_exe = (!cli.opts.no_livekit).then(|| std::path::absolute(&cli.opts.livekit_bin).unwrap_or_else(|_| cli.opts.livekit_bin.clone()));
    let no_diag = cli.no_diag;
    let result = rt.block_on(serve_until(cli.opts, move |server| {
        print_server_code(server.tls_pin());
        if !no_diag {
            let lk = &server.config.livekit;
            let hub_port = server.addr.port();
            voxhub_server::diag::spawn(voxhub_server::diag::Input {
                addrs: server.addrs.clone(),
                dual_stack: server.dual_stack,
                hub_port,
                signal_port: lk.signal_port,
                rtc_tcp_port: lk.rtc_tcp_port,
                rtc_udp_port: lk.rtc_udp_port,
                server_exe: std::env::current_exe().ok(),
                livekit_exe,
            });
        }
        wait_ctrl_c()
    }));
    if let Err(e) = &result {
        tracing::error!("fatal: {e:#}");
        eprintln!("error: {e:#}");
    }
    tracing::info!("bye");
    result
}

/// v1.1 (T0): the code people can compare with "Servidor > Segurança da conexão" in the app.
/// Written with `writeln!` and its error ignored: a closed console must not stop the server.
fn print_server_code(pin: Option<[u8; 32]>) {
    use std::io::Write;
    let line = "==============================================================================";
    let text = match pin {
        Some(pin) => format!(
            "  Conexão protegida (TLS) ligada. Código do servidor: {}\n  Quem entrar pode conferir este código no app: menu do servidor > Segurança da conexão.",
            vox_proto::tls_server_code(&pin)
        ),
        None => "  ATENÇÃO: conexão protegida desligada (tls = \"off\" no config.toml): senhas e mensagens passam pela rede sem criptografia.".to_string(),
    };
    let _ = writeln!(std::io::stderr(), "\n{line}\n{text}\n{line}\n");
}

async fn wait_ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!("cannot wait for ctrl+c: {e}");
        return;
    }
    tracing::info!("ctrl+c received, shutting down");
}
