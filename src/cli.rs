use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum JsonOutputFormat {
    Json,
    Ndjson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ListOutputFormat {
    Table,
    Json,
    Ndjson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StreamOutputFormat {
    Pretty,
    Json,
    Ndjson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum WindowSemantics {
    Elapsed,
    Ordinal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StreamOutputFilter {
    Data,
    Status,
    Events,
    All,
}

#[derive(Debug, Parser)]
#[command(name = "disc", version, about = "Disc signals CLI")]
pub struct Cli {
    #[arg(long, global = true, env = "DISC_API_KEY")]
    pub api_key: Option<String>,
    #[arg(long, alias = "api-url", global = true, env = "DISC_HTTP_BASE_URL")]
    pub http_base_url: Option<String>,
    #[arg(long, global = true, env = "DISC_WS_URL")]
    pub ws_url: Option<String>,
    #[arg(long, global = true, env = "DISC_CLIENT_ID")]
    pub client_id: Option<String>,
    #[command(subcommand)]
    pub command: RootCommand,
}

#[derive(Debug, Subcommand)]
pub enum RootCommand {
    #[command(subcommand)]
    Auth(AuthCommand),
    #[command(subcommand)]
    Config(ConfigCommand),
    #[command(subcommand)]
    Signals(SignalsCommand),
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    #[command(about = "Show effective configuration")]
    Show,
    #[command(about = "Update stored configuration")]
    Set {
        #[arg(long)]
        http_base_url: Option<String>,
        #[arg(long)]
        ws_url: Option<String>,
        #[arg(long)]
        client_id: Option<String>,
    },
    #[command(about = "Reset stored configuration to defaults")]
    Reset,
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    #[command(about = "Sign in through your browser")]
    Login {
        #[arg(long, default_value_t = false)]
        no_browser: bool,
        #[arg(long)]
        issuer: Option<String>,
        #[arg(long)]
        oauth_client_id: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long, hide = true)]
        machine_label: Option<String>,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long, default_value_t = false)]
        device: bool,
    },
    #[command(about = "List stored subject profiles")]
    List,
    #[command(about = "Select a stored subject profile")]
    Use { profile: String },
    #[command(subcommand)]
    ApiKey(ApiKeyCommand),
    #[command(
        alias = "status",
        about = "Show the active authenticated identity and subject"
    )]
    Whoami {
        #[arg(long, value_enum, default_value_t = JsonOutputFormat::Json)]
        format: JsonOutputFormat,
    },
    Clear {
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    #[command(about = "Revoke OAuth credentials and remove stored profiles")]
    Logout {
        #[arg(long, default_value_t = false)]
        all: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ApiKeyCommand {
    Set {
        #[arg(long)]
        value: Option<String>,
        #[arg(long, default_value_t = false)]
        stdin: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum SignalsCommand {
    Subscribe(InteractiveSubscribeCommand),
    #[command(subcommand)]
    Passive(PassiveSignalsCommand),
    #[command(subcommand)]
    Active(ActiveSignalsCommand),
}

#[derive(Debug, Subcommand)]
pub enum PassiveSignalsCommand {
    List {
        #[arg(long, value_enum, default_value_t = ListOutputFormat::Table)]
        format: ListOutputFormat,
    },
    Get {
        passive_signal_id: String,
        #[arg(long, value_enum, default_value_t = JsonOutputFormat::Json)]
        format: JsonOutputFormat,
    },
    Subscribe(StreamCommand),
    Tail(TailCommand),
}

#[derive(Debug, Subcommand)]
pub enum ActiveSignalsCommand {
    List {
        #[arg(long = "for-passive")]
        passive_signal_id: String,
        #[arg(long, value_enum, default_value_t = ListOutputFormat::Table)]
        format: ListOutputFormat,
    },
    Get {
        active_signal_id: String,
        #[arg(long, value_enum, default_value_t = JsonOutputFormat::Json)]
        format: JsonOutputFormat,
    },
    Subscribe(StreamCommand),
    Tail(TailCommand),
}

#[derive(Debug, Args, Clone)]
pub struct StreamOptions {
    #[arg(long, value_enum, default_value_t = StreamOutputFilter::Data)]
    pub output: StreamOutputFilter,
    #[arg(long, value_enum, default_value_t = WindowSemantics::Ordinal)]
    pub window_semantics: WindowSemantics,
    #[arg(long, default_value_t = false)]
    pub backfill: bool,
    #[arg(long)]
    pub backfill_from: Option<i64>,
    #[arg(long)]
    pub backfill_to: Option<i64>,
    #[arg(long)]
    pub backfill_count: Option<u32>,
    #[arg(long, default_value_t = false)]
    pub include_status: bool,
    #[arg(long, default_value_t = false)]
    pub once: bool,
    #[arg(long, value_parser = humantime::parse_duration)]
    pub timeout: Option<std::time::Duration>,
    #[arg(long, default_value_t = false)]
    pub no_reconnect: bool,
}

#[derive(Debug, Args, Clone)]
pub struct StreamCommand {
    pub signal_id: String,
    #[command(flatten)]
    pub options: StreamOptions,
    #[arg(long, value_enum, default_value_t = StreamOutputFormat::Ndjson)]
    pub format: StreamOutputFormat,
    #[arg(long)]
    pub destination: Option<PathBuf>,
}

#[derive(Debug, Args, Clone)]
pub struct TailCommand {
    pub signal_id: String,
    #[arg(long, value_enum, default_value_t = StreamOutputFilter::All)]
    pub output: StreamOutputFilter,
    #[arg(long, value_enum, default_value_t = WindowSemantics::Ordinal)]
    pub window_semantics: WindowSemantics,
    #[arg(long, default_value_t = false)]
    pub backfill: bool,
    #[arg(long)]
    pub backfill_from: Option<i64>,
    #[arg(long)]
    pub backfill_to: Option<i64>,
    #[arg(long)]
    pub backfill_count: Option<u32>,
    #[arg(long, default_value_t = false)]
    pub include_status: bool,
    #[arg(long, default_value_t = false)]
    pub once: bool,
    #[arg(long, value_parser = humantime::parse_duration)]
    pub timeout: Option<std::time::Duration>,
    #[arg(long, default_value_t = false)]
    pub no_reconnect: bool,
    #[arg(long, value_enum, default_value_t = StreamOutputFormat::Pretty)]
    pub format: StreamOutputFormat,
}

#[derive(Debug, Args, Clone)]
pub struct InteractiveSubscribeCommand {
    #[command(flatten)]
    pub options: StreamOptions,
    #[arg(long, value_enum, default_value_t = StreamOutputFormat::Ndjson)]
    pub format: StreamOutputFormat,
    #[arg(long, default_value = "disc-signals.ndjson")]
    pub destination: PathBuf,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{AuthCommand, Cli, RootCommand};

    #[test]
    fn auth_login_uses_interactive_production_defaults() {
        let cli = Cli::try_parse_from(["disc", "auth", "login"]).expect("parse auth login");

        let RootCommand::Auth(AuthCommand::Login {
            device,
            no_browser,
            issuer,
            oauth_client_id,
            subject,
            ..
        }) = cli.command
        else {
            panic!("expected auth login");
        };
        assert!(!device);
        assert!(!no_browser);
        assert!(issuer.is_none());
        assert!(oauth_client_id.is_none());
        assert!(subject.is_none());
    }

    #[test]
    fn nested_auth_login_remains_available_with_advanced_overrides() {
        let cli = Cli::try_parse_from([
            "disc",
            "auth",
            "login",
            "--device",
            "--issuer",
            "https://sso.example.test/realms/disc",
            "--oauth-client-id",
            "disc-cli-test",
            "--subject",
            "subject-42",
        ])
        .expect("parse nested login");

        let RootCommand::Auth(AuthCommand::Login {
            device,
            issuer,
            oauth_client_id,
            subject,
            ..
        }) = cli.command
        else {
            panic!("expected nested auth login");
        };
        assert!(device);
        assert_eq!(
            issuer.as_deref(),
            Some("https://sso.example.test/realms/disc")
        );
        assert_eq!(oauth_client_id.as_deref(), Some("disc-cli-test"));
        assert_eq!(subject.as_deref(), Some("subject-42"));
    }
}
