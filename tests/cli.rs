mod common;
use clap::Parser;
use common::{Backend, Fixture};
use ouro_console::{
    cli::{self, Action, Cli, TokenConfig},
    client::*,
};
#[test]
fn parses_serve_defaults_and_global_options_after_subcommand() {
    let cli = Cli::try_parse_from([
        "ouro-console",
        "serve",
        "--fake",
        "--read-only",
        "--actor",
        "observer",
        "--trace-url",
        "https://traces.example/{trace_id}",
    ])
    .unwrap();
    assert!(cli.fake && cli.read_only);
    assert_eq!(cli.actor.as_deref(), Some("observer"));
    assert!(matches!(cli.command,Action::Serve {bind} if bind.to_string() == "127.0.0.1:8090"));
}
#[test]
fn every_script_command_parses_and_mutations_require_reason() {
    for args in [
        vec!["runs", "--state", "running"],
        vec!["history", "--since-ms", "1", "--before-ms", "2"],
        vec!["inspect", "run-002"],
        vec!["run-log", "run-002"],
        vec!["pending", "--event-type", "review.decision"],
        vec!["definitions"],
        vec!["workers"],
        vec!["journal", "--run", "run-002"],
        vec!["counts", "--definition", "review"],
        vec!["status"],
    ] {
        assert!(Cli::try_parse_from(std::iter::once("ouro-console").chain(args)).is_ok());
    }
    for args in [
        vec!["cancel-run", "run-002"],
        vec!["cancel-tree", "run-002"],
        vec!["pause-definition", "review"],
        vec!["resume-definition", "cleanup"],
        vec!["stop-all"],
        vec!["resume-all"],
        vec![
            "respond",
            "run-003",
            "/decision",
            "--event",
            r#"{"source":"cli","id":"1","type":"review.decision"}"#,
        ],
    ] {
        assert!(
            Cli::try_parse_from(std::iter::once("ouro-console").chain(args.iter().copied()))
                .is_err()
        );
        assert!(
            Cli::try_parse_from(
                std::iter::once("ouro-console")
                    .chain(args)
                    .chain(["--reason", "reviewed"])
            )
            .is_ok()
        );
    }
    assert!(Cli::try_parse_from(["ouro-console", "runs", "--state", "bogus"]).is_err());
}
async fn script_dispatch_uses_shared_client_and_read_only_policy(backend: Backend) {
    let fixture = Fixture::new(&backend).await;
    let mut session = Session {
        client: fixture.client.clone(),
        actor: "script-operator".into(),
        read_only: false,
    };
    let parse = |args: Vec<&str>| {
        Cli::try_parse_from(std::iter::once("ouro-console").chain(args))
            .unwrap()
            .command
    };
    let reply = cli::execute(
        &session,
        parse(vec![
            "stop-all",
            "--reason",
            "maintenance",
            "--request-id",
            "cli-stop",
        ]),
    )
    .await
    .unwrap();
    assert_eq!(reply["request"]["actor"], "script-operator");
    assert!(fixture.stopped().await.unwrap());
    assert_eq!(
        cli::execute(&session, parse(vec!["status"])).await.unwrap(),
        true
    );
    let runs = cli::execute(
        &session,
        parse(vec!["runs", "--state", "running", "--limit", "100"]),
    )
    .await
    .unwrap();
    assert_eq!(runs["items"].as_array().unwrap().len(), 1);
    session.read_only = true;
    assert!(matches!(
        cli::execute(&session, parse(vec!["resume-all", "--reason", "test"])).await,
        Err(ControlError::ReadOnly)
    ));
}
#[test]
fn token_configuration_rejects_empty_or_whitespace_values() {
    for value in ["", "has space", "line\nbreak"] {
        assert!(TokenConfig::load(None, Some(value.into())).is_err());
    }
    assert!(TokenConfig::load(None, None).unwrap().is_none());
    assert_eq!(
        TokenConfig::load(None, Some("secret".into()))
            .unwrap()
            .unwrap()
            .token,
        "secret"
    );
    let path = std::env::temp_dir().join(format!("ouro-console-{}.json", uuid::Uuid::new_v4()));
    std::fs::write(&path, r#"{"token":"file-secret","actor":"file-actor"}"#).unwrap();
    let config = TokenConfig::load(Some(&path), Some("ignored".into()))
        .unwrap()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(config.actor.as_deref(), Some("file-actor"));
    assert_eq!(config.token, "file-secret");
}

common::backend_tests!(script_dispatch_uses_shared_client_and_read_only_policy);
