//! CLI parsing tests.
use super::*;

#[test]
fn bare_jade_opens_the_console() {
    let cli = Cli::try_parse_from(["jade"]).expect("parse");
    assert!(cli.cmd.is_none(), "no subcommand should mean the TUI");
    assert!(!cli.continue_last);
    assert!(cli.session.is_none());
}

#[test]
fn session_and_auth_flags_parse() {
    let cli = Cli::try_parse_from(["jade", "-c"]).expect("parse");
    assert!(cli.continue_last);

    let cli = Cli::try_parse_from(["jade", "--session", "sess-123"]).expect("parse");
    assert_eq!(cli.session.as_deref(), Some("sess-123"));

    let cli = Cli::try_parse_from(["jade", "login"]).expect("parse");
    assert!(matches!(cli.cmd, Some(Commands::Login)));

    let cli = Cli::try_parse_from(["jade", "auth", "list"]).expect("parse");
    assert!(matches!(
        cli.cmd,
        Some(Commands::Auth { cmd: AuthCmd::List })
    ));

    let cli = Cli::try_parse_from(["jade", "models"]).expect("parse");
    assert!(matches!(cli.cmd, Some(Commands::Models)));
}

#[test]
fn subcommands_still_parse() {
    let cli = Cli::try_parse_from(["jade", "jobs"]).expect("parse");
    assert!(matches!(cli.cmd, Some(Commands::Jobs)));
    let cli = Cli::try_parse_from(["jade", "--on", "laptop", "run", "echo hi"]).expect("parse");
    assert!(matches!(cli.cmd, Some(Commands::Run(_))));
    assert_eq!(cli.on, "laptop");
}

#[test]
fn cwd_flag_is_global() {
    let cli = Cli::try_parse_from(["jade", "--cwd", "/tmp/x", "jobs"]).expect("parse");
    assert_eq!(cli.cwd.as_deref(), Some(std::path::Path::new("/tmp/x")));

    let cli = Cli::try_parse_from(["jade", "--cwd", "/tmp/x", "run", "pwd"]).expect("parse");
    assert_eq!(cli.cwd.as_deref(), Some(std::path::Path::new("/tmp/x")));

    let cli = Cli::try_parse_from(["jade", "run", "--cwd", "/tmp/y", "pwd"]).expect("parse");
    match cli.cmd {
        Some(Commands::Run(args)) => {
            assert_eq!(args.cwd.as_deref(), Some(std::path::Path::new("/tmp/y")));
        }
        other => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn shell_run_keeps_every_word() {
    let cli =
        Cli::try_parse_from(["jade", "run", "--shell", "echo", "hello", "world"]).expect("parse");
    let Commands::Run(args) = cli.cmd.expect("run") else {
        panic!("expected run");
    };
    let req = args.into_request().expect("request");
    match req.spec {
        jade_proto::TaskSpec::Shell { command } => assert_eq!(command, "echo hello world"),
        other => panic!("expected shell, got {other:?}"),
    }

    let cli = Cli::try_parse_from(["jade", "run", "echo", "hello"]).expect("parse");
    let Commands::Run(args) = cli.cmd.expect("run") else {
        panic!("expected run");
    };
    let req = args.into_request().expect("request");
    assert!(matches!(req.spec, jade_proto::TaskSpec::Argv { .. }));
}

#[test]
fn ask_subcommand_and_new_flag_parse() {
    let cli = Cli::try_parse_from(["jade", "--cwd", "/tmp/x", "--new"]).expect("parse --new");
    assert!(cli.new_session);
    assert!(cli.cmd.is_none());

    let cli = Cli::try_parse_from([
        "jade",
        "ask",
        "--output-format",
        "json",
        "--permission-mode",
        "auto",
        "explain",
        "this",
    ])
    .expect("parse ask");
    assert_eq!(cli.headless.output_format, headless::OutputFormat::Json);
    assert_eq!(cli.headless.permission_mode.as_deref(), Some("auto"));
    match cli.cmd {
        Some(Commands::Ask { prompt }) => assert_eq!(prompt, vec!["explain", "this"]),
        other => panic!("expected Ask, got {other:?}"),
    }
}

#[test]
fn env_model_does_not_block_tui_while_cli_model_flag_does() {
    let matches = Cli::command()
        .try_get_matches_from(["jade", "--model", "gpt-5"])
        .expect("parse");
    let cli = Cli::from_arg_matches(&matches).expect("from matches");
    assert_eq!(
        tui_unsupported_flag(&cli.headless, &matches),
        Some("--model")
    );

    let env_only = headless::HeadlessArgs {
        model: Some("gpt-5".into()),
        ..Default::default()
    };
    assert_eq!(tui_unsupported_flag_with(&env_only, false), None);
}
