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
    #[arg(long, global = true, env = "DISC_HTTP_BASE_URL")]
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
    Signals(SignalsCommand),
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    #[command(subcommand)]
    ApiKey(ApiKeyCommand),
    Whoami {
        #[arg(long, value_enum, default_value_t = JsonOutputFormat::Json)]
        format: JsonOutputFormat,
    },
    Clear,
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
