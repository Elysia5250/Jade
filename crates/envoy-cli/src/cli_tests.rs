//! CLI parsing tests.
use super::*;


#[test]
fn bare_envoy_opens_the_console() {
    let cli = Cli::try_parse_from(["envoy"]).expect("parse");
    assert!(cli.cmd.is_none(), "no subcommand should mean the TUI");
    assert!(!cli.continue_last);
    assert!(cli.session.is_none());
}

#[test]
fn session_and_auth_flags_parse() {
    let cli = Cli::try_parse_from(["envoy", "-c"]).expect("parse");
    assert!(cli.continue_last);

    let cli = Cli::try_parse_from(["envoy", "--session", "sess-123"]).expect("parse");
    assert_eq!(cli.session.as_deref(), Some("sess-123"));

    let cli = Cli::try_parse_from(["envoy", "login"]).expect("parse");
    assert!(matches!(cli.cmd, Some(Commands::Login)));

    let cli = Cli::try_parse_from(["envoy", "auth", "list"]).expect("parse");
    assert!(matches!(
        cli.cmd,
        Some(Commands::Auth {
            cmd: AuthCmd::List
        })
    ));

    let cli = Cli::try_parse_from(["envoy", "models"]).expect("parse");
    assert!(matches!(cli.cmd, Some(Commands::Models)));
}

#[test]
fn subcommands_still_parse() {
    let cli = Cli::try_parse_from(["envoy", "jobs"]).expect("parse");
    assert!(matches!(cli.cmd, Some(Commands::Jobs)));
    let cli = Cli::try_parse_from(["envoy", "--on", "laptop", "run", "echo hi"]).expect("parse");
    assert!(matches!(cli.cmd, Some(Commands::Run(_))));
    assert_eq!(cli.on, "laptop");
}

#[test]
fn cwd_flag_is_global() {
    let cli = Cli::try_parse_from(["envoy", "--cwd", "/tmp/x", "jobs"]).expect("parse");
    assert_eq!(cli.cwd.as_deref(), Some(std::path::Path::new("/tmp/x")));

    let cli = Cli::try_parse_from(["envoy", "--cwd", "/tmp/x", "run", "pwd"]).expect("parse");
    assert_eq!(cli.cwd.as_deref(), Some(std::path::Path::new("/tmp/x")));

    let cli = Cli::try_parse_from(["envoy", "run", "--cwd", "/tmp/y", "pwd"]).expect("parse");
    match cli.cmd {
        Some(Commands::Run(args)) => {
            assert_eq!(args.cwd.as_deref(), Some(std::path::Path::new("/tmp/y")));
        }
        other => panic!("expected Run, got {other:?}"),
    }
}
