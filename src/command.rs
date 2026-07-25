use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::{self, Read};
use std::path::Path;
use std::pin::Pin;

use anyhow::{Context, Result};
use dialoguer::{MultiSelect, Select, theme::ColorfulTheme};
use tokio::task::JoinHandle;

use crate::cli::{
    ActiveSignalsCommand, ApiKeyCommand, AuthCommand, Cli, ConfigCommand,
    InteractiveSubscribeCommand, PassiveSignalsCommand, RootCommand, SignalsCommand, StreamCommand,
    StreamOptions, TailCommand,
};
use crate::config::{ConfigStore, StoredAuth};
use crate::http::{ActiveSignalSummary, DiscApiClient, PassiveSignalSummary};
use crate::output::{
    SharedWriter, create_file_writer, create_stdout_writer, print_json_value, print_signal_list,
    should_emit_event, validate_to_json, write_subscription_event,
};
use crate::ws::{SubscriptionKind, SubscriptionSpec, run_subscription};

struct ReconcileSubscriptionContext<'a> {
    writer: &'a SharedWriter,
    ws_url: &'a str,
    api_key: &'a str,
    client_id: Option<&'a str>,
    options: &'a StreamOptions,
    format: crate::cli::StreamOutputFormat,
}

trait SubscriptionPrompter {
    fn choose_action(&mut self) -> Result<usize>;
    fn select_passive_signals(
        &mut self,
        passive_signals: &[PassiveSignalSummary],
        selected_passive_ids: &HashSet<String>,
    ) -> Result<HashSet<String>>;
    fn choose_passive_parent(
        &mut self,
        passive_signals: &[PassiveSignalSummary],
    ) -> Result<PassiveSignalSummary>;
    fn select_active_signals(
        &mut self,
        active_signals: &[ActiveSignalSummary],
        selected_active_ids: &HashSet<String>,
    ) -> Result<HashSet<String>>;
    fn wait_for_shutdown(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
}

struct DialogSubscriptionPrompter {
    theme: ColorfulTheme,
}

impl DialogSubscriptionPrompter {
    fn new() -> Self {
        Self {
            theme: ColorfulTheme::default(),
        }
    }
}

impl SubscriptionPrompter for DialogSubscriptionPrompter {
    fn choose_action(&mut self) -> Result<usize> {
        Select::with_theme(&self.theme)
            .with_prompt("Manage subscriptions")
            .items(&[
                "Edit passive signals",
                "Edit active signals",
                "Finish and keep current subscriptions running",
                "Quit and stop all subscriptions",
            ])
            .default(0)
            .interact()
            .context("Failed to read interactive selection.")
    }

    fn select_passive_signals(
        &mut self,
        passive_signals: &[PassiveSignalSummary],
        selected_passive_ids: &HashSet<String>,
    ) -> Result<HashSet<String>> {
        prompt_passive_signal_selection(&self.theme, passive_signals, selected_passive_ids)
    }

    fn choose_passive_parent(
        &mut self,
        passive_signals: &[PassiveSignalSummary],
    ) -> Result<PassiveSignalSummary> {
        prompt_passive_parent(&self.theme, passive_signals)
    }

    fn select_active_signals(
        &mut self,
        active_signals: &[ActiveSignalSummary],
        selected_active_ids: &HashSet<String>,
    ) -> Result<HashSet<String>> {
        prompt_active_signal_selection(&self.theme, active_signals, selected_active_ids)
    }

    fn wait_for_shutdown(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async {
            tokio::signal::ctrl_c()
                .await
                .context("Failed to wait for Ctrl+C.")
        })
    }
}

pub async fn run(cli: Cli) -> Result<()> {
    let store = ConfigStore::discover()?;
    run_with_store(cli, &store).await
}

async fn run_with_store(cli: Cli, store: &ConfigStore) -> Result<()> {
    let api_key = cli.api_key.clone();
    let http_base_url = cli.http_base_url.clone();
    let ws_url = cli.ws_url.clone();
    let client_id = cli.client_id.clone();

    match cli.command {
        RootCommand::Auth(command) => {
            run_auth(command, api_key, http_base_url, ws_url, client_id, store).await
        }
        RootCommand::Config(command) => run_config(command, store),
        RootCommand::Signals(command) => {
            run_signals(command, api_key, http_base_url, ws_url, client_id, store).await
        }
    }
}

async fn run_auth(
    command: AuthCommand,
    api_key: Option<String>,
    http_base_url: Option<String>,
    ws_url: Option<String>,
    client_id: Option<String>,
    store: &ConfigStore,
) -> Result<()> {
    match command {
        AuthCommand::ApiKey(command) => match command {
            ApiKeyCommand::Set { value, stdin } => {
                let api_key = resolve_api_key_input(value, stdin)?;
                store.save_auth(&StoredAuth {
                    api_key: api_key.clone(),
                })?;
                let mut config = store.load_config()?;
                if http_base_url.is_some() {
                    config.http_base_url = http_base_url;
                }
                if ws_url.is_some() {
                    config.ws_url = ws_url;
                }
                if client_id.is_some() {
                    config.client_id = client_id;
                }
                store.save_config(&config)?;
                println!("Stored API key in {}.", store.root_dir().display());
                Ok(())
            }
        },
        AuthCommand::Whoami { format } => {
            let effective = store.resolve(
                api_key.as_deref(),
                http_base_url.as_deref(),
                ws_url.as_deref(),
                client_id.as_deref(),
            )?;
            let client = DiscApiClient::new(effective.http_base_url, &effective.api_key)?;
            let response = client.validate().await?;
            let json = validate_to_json(&response);
            print_json_value(&json, format)
        }
        AuthCommand::Clear => {
            let removed = store.clear_auth()?;
            if removed {
                println!("Cleared stored API key.");
            } else {
                println!("No stored API key to clear.");
            }
            Ok(())
        }
    }
}

fn run_config(command: ConfigCommand, store: &ConfigStore) -> Result<()> {
    match command {
        ConfigCommand::Show => {
            let stored = store.load_config()?;
            let http_base_url = stored
                .http_base_url
                .unwrap_or_else(|| "https://api.disc.tech (default)".to_owned());
            let ws_url = stored
                .ws_url
                .unwrap_or_else(|| "wss://signals.disc.tech (default)".to_owned());
            let client_id = stored.client_id.unwrap_or_else(|| "(not set)".to_owned());
            println!("http_base_url: {http_base_url}");
            println!("ws_url:        {ws_url}");
            println!("client_id:     {client_id}");
        }
        ConfigCommand::Set {
            http_base_url,
            ws_url,
            client_id,
        } => {
            let mut config = store.load_config()?;
            if let Some(url) = http_base_url {
                config.http_base_url = Some(url);
            }
            if let Some(url) = ws_url {
                config.ws_url = Some(url);
            }
            if let Some(id) = client_id {
                config.client_id = Some(id);
            }
            store.save_config(&config)?;
            println!("Configuration updated.");
        }
        ConfigCommand::Reset => {
            store.save_config(&Default::default())?;
            println!("Configuration reset to defaults.");
        }
    }
    Ok(())
}

async fn run_signals(
    command: SignalsCommand,
    api_key: Option<String>,
    http_base_url: Option<String>,
    ws_url: Option<String>,
    client_id: Option<String>,
    store: &ConfigStore,
) -> Result<()> {
    let effective = store.resolve(
        api_key.as_deref(),
        http_base_url.as_deref(),
        ws_url.as_deref(),
        client_id.as_deref(),
    )?;
    let client = DiscApiClient::new(effective.http_base_url.clone(), &effective.api_key)?;

    match command {
        SignalsCommand::Subscribe(command) => {
            run_interactive_subscribe(
                &client,
                &effective.ws_url,
                &effective.api_key,
                effective.client_id.as_deref(),
                &command,
            )
            .await
        }
        SignalsCommand::Passive(command) => match command {
            PassiveSignalsCommand::List { format } => {
                let signals = client.list_passive_signals().await?;
                print_signal_list(&signals, format)
            }
            PassiveSignalsCommand::Get {
                passive_signal_id,
                format,
            } => {
                let signal = client.get_passive_signal(&passive_signal_id).await?;
                print_json_value(&signal, format)
            }
            PassiveSignalsCommand::Subscribe(command) => {
                run_stream_command(
                    SubscriptionKind::Passive,
                    &effective.ws_url,
                    &effective.api_key,
                    effective.client_id.as_deref(),
                    &command,
                )
                .await
            }
            PassiveSignalsCommand::Tail(command) => {
                run_tail_command(
                    SubscriptionKind::Passive,
                    &effective.ws_url,
                    &effective.api_key,
                    effective.client_id.as_deref(),
                    &command,
                )
                .await
            }
        },
        SignalsCommand::Active(command) => match command {
            ActiveSignalsCommand::List {
                passive_signal_id,
                format,
            } => {
                let signals = client.list_active_signals(&passive_signal_id).await?;
                print_signal_list(&signals, format)
            }
            ActiveSignalsCommand::Get {
                active_signal_id,
                format,
            } => {
                let signal = client.get_active_signal(&active_signal_id).await?;
                print_json_value(&signal, format)
            }
            ActiveSignalsCommand::Subscribe(command) => {
                run_stream_command(
                    SubscriptionKind::Active,
                    &effective.ws_url,
                    &effective.api_key,
                    effective.client_id.as_deref(),
                    &command,
                )
                .await
            }
            ActiveSignalsCommand::Tail(command) => {
                run_tail_command(
                    SubscriptionKind::Active,
                    &effective.ws_url,
                    &effective.api_key,
                    effective.client_id.as_deref(),
                    &command,
                )
                .await
            }
        },
    }
}

async fn run_stream_command(
    kind: SubscriptionKind,
    ws_url: &str,
    api_key: &str,
    client_id: Option<&str>,
    command: &StreamCommand,
) -> Result<()> {
    let writer = match &command.destination {
        Some(path) => create_file_writer(path)?,
        None => create_stdout_writer(),
    };
    let spec = SubscriptionSpec {
        kind,
        signal_id: command.signal_id.clone(),
    };

    run_subscription(
        ws_url,
        api_key,
        client_id,
        &spec,
        &command.options,
        true,
        |event| {
            if should_emit_event(&event, command.options.output) {
                write_subscription_event(&writer, &event, command.format)?;
                if command.options.once {
                    return Ok(true);
                }
            }

            Ok(false)
        },
    )
    .await
}

async fn run_tail_command(
    kind: SubscriptionKind,
    ws_url: &str,
    api_key: &str,
    client_id: Option<&str>,
    command: &TailCommand,
) -> Result<()> {
    let writer = create_stdout_writer();
    let spec = SubscriptionSpec {
        kind,
        signal_id: command.signal_id.clone(),
    };
    let options = stream_options_from_tail(command);

    run_subscription(ws_url, api_key, client_id, &spec, &options, true, |event| {
        if should_emit_event(&event, options.output) {
            write_subscription_event(&writer, &event, command.format)?;
            if options.once {
                return Ok(true);
            }
        }

        Ok(false)
    })
    .await
}

async fn run_interactive_subscribe(
    client: &DiscApiClient,
    ws_url: &str,
    api_key: &str,
    client_id: Option<&str>,
    command: &InteractiveSubscribeCommand,
) -> Result<()> {
    let mut prompter = DialogSubscriptionPrompter::new();
    run_interactive_subscribe_with_prompter(
        client,
        ws_url,
        api_key,
        client_id,
        command,
        &mut prompter,
    )
    .await
}

async fn run_interactive_subscribe_with_prompter<P>(
    client: &DiscApiClient,
    ws_url: &str,
    api_key: &str,
    client_id: Option<&str>,
    command: &InteractiveSubscribeCommand,
    prompter: &mut P,
) -> Result<()>
where
    P: SubscriptionPrompter,
{
    let passive_signals = client.list_passive_signal_summaries().await?;
    let writer = create_file_writer(&command.destination)?;
    let mut selected_passive_ids = HashSet::<String>::new();
    let mut selected_active_ids = HashSet::<String>::new();
    let mut active_signal_cache = HashMap::<String, Vec<ActiveSignalSummary>>::new();
    let mut tasks = HashMap::<SubscriptionSpec, JoinHandle<()>>::new();

    loop {
        print_subscription_summary(
            &passive_signals,
            &active_signal_cache,
            &selected_passive_ids,
            &selected_active_ids,
            &command.destination,
        );

        let action = prompter.choose_action()?;

        match action {
            0 => {
                let next_selected =
                    prompter.select_passive_signals(&passive_signals, &selected_passive_ids)?;
                selected_passive_ids = next_selected;
            }
            1 => {
                if passive_signals.is_empty() {
                    println!("No passive signals available.");
                } else {
                    let passive_signal = prompter.choose_passive_parent(&passive_signals)?;
                    let active_signals = match active_signal_cache
                        .get(&passive_signal.passive_signal_id)
                    {
                        Some(cached) => cached.clone(),
                        None => {
                            let fetched = client
                                .list_active_signal_summaries(&passive_signal.passive_signal_id)
                                .await?;
                            active_signal_cache
                                .insert(passive_signal.passive_signal_id.clone(), fetched.clone());
                            fetched
                        }
                    };

                    if active_signals.is_empty() {
                        println!("No active signals under `{}`.", passive_signal.label);
                    } else {
                        selected_active_ids = prompter
                            .select_active_signals(&active_signals, &selected_active_ids)?;
                    }
                }
            }
            2 => {
                reconcile_subscriptions(
                    &mut tasks,
                    ReconcileSubscriptionContext {
                        writer: &writer,
                        ws_url,
                        api_key,
                        client_id,
                        options: &command.options,
                        format: command.format,
                    },
                    &selected_passive_ids,
                    &selected_active_ids,
                );
                println!(
                    "Subscriptions are running. Output is being appended to {}. Press Ctrl+C to stop the CLI.",
                    command.destination.display()
                );
                prompter.wait_for_shutdown().await?;
                abort_all_tasks(&mut tasks);
                return Ok(());
            }
            3 => {
                abort_all_tasks(&mut tasks);
                return Ok(());
            }
            _ => unreachable!(),
        }

        reconcile_subscriptions(
            &mut tasks,
            ReconcileSubscriptionContext {
                writer: &writer,
                ws_url,
                api_key,
                client_id,
                options: &command.options,
                format: command.format,
            },
            &selected_passive_ids,
            &selected_active_ids,
        );
    }
}

fn reconcile_subscriptions(
    tasks: &mut HashMap<SubscriptionSpec, JoinHandle<()>>,
    context: ReconcileSubscriptionContext<'_>,
    selected_passive_ids: &HashSet<String>,
    selected_active_ids: &HashSet<String>,
) {
    let desired_specs = selected_passive_ids
        .iter()
        .map(|signal_id| SubscriptionSpec {
            kind: SubscriptionKind::Passive,
            signal_id: signal_id.clone(),
        })
        .chain(
            selected_active_ids
                .iter()
                .map(|signal_id| SubscriptionSpec {
                    kind: SubscriptionKind::Active,
                    signal_id: signal_id.clone(),
                }),
        )
        .collect::<HashSet<_>>();

    let existing_specs = tasks.keys().cloned().collect::<Vec<_>>();

    for spec in existing_specs {
        if !desired_specs.contains(&spec)
            && let Some(task) = tasks.remove(&spec)
        {
            task.abort();
        }
    }

    for spec in desired_specs {
        if tasks.contains_key(&spec) {
            continue;
        }

        let writer = context.writer.clone();
        let ws_url = context.ws_url.to_owned();
        let api_key = context.api_key.to_owned();
        let client_id = context.client_id.map(str::to_owned);
        let options = context.options.clone();
        let format = context.format;
        let spec_for_task = spec.clone();
        let task = tokio::spawn(async move {
            let _ = run_subscription(
                &ws_url,
                &api_key,
                client_id.as_deref(),
                &spec_for_task,
                &options,
                false,
                |event| {
                    if should_emit_event(&event, options.output) {
                        write_subscription_event(&writer, &event, format)?;
                    }

                    Ok(false)
                },
            )
            .await;
        });
        tasks.insert(spec, task);
    }
}

fn stream_options_from_tail(command: &TailCommand) -> StreamOptions {
    StreamOptions {
        output: command.output,
        window_semantics: command.window_semantics,
        backfill: command.backfill,
        backfill_from: command.backfill_from,
        backfill_to: command.backfill_to,
        backfill_count: command.backfill_count,
        include_status: command.include_status,
        once: command.once,
        timeout: command.timeout,
        no_reconnect: command.no_reconnect,
    }
}

fn abort_all_tasks(tasks: &mut HashMap<SubscriptionSpec, JoinHandle<()>>) {
    for (_, task) in tasks.drain() {
        task.abort();
    }
}

fn print_subscription_summary(
    passive_signals: &[PassiveSignalSummary],
    active_signal_cache: &HashMap<String, Vec<ActiveSignalSummary>>,
    selected_passive_ids: &HashSet<String>,
    selected_active_ids: &HashSet<String>,
    destination: &Path,
) {
    println!();
    println!("Current destination: {}", destination.display());
    println!("Selected passive signals:");
    if selected_passive_ids.is_empty() {
        println!("  - none");
    } else {
        for passive_signal in passive_signals {
            if selected_passive_ids.contains(&passive_signal.passive_signal_id) {
                println!(
                    "  - {} ({})",
                    passive_signal.label, passive_signal.passive_signal_id
                );
            }
        }
    }

    println!("Selected active signals:");
    if selected_active_ids.is_empty() {
        println!("  - none");
    } else {
        for active_signals in active_signal_cache.values() {
            for active_signal in active_signals {
                if selected_active_ids.contains(&active_signal.active_signal_id) {
                    println!(
                        "  - {} ({}) <- {}",
                        active_signal.label,
                        active_signal.active_signal_id,
                        active_signal.passive_signal_id
                    );
                }
            }
        }
    }
    println!();
}

fn prompt_passive_signal_selection(
    theme: &ColorfulTheme,
    passive_signals: &[PassiveSignalSummary],
    selected_passive_ids: &HashSet<String>,
) -> Result<HashSet<String>> {
    let labels = passive_signals
        .iter()
        .map(|signal| format!("{} ({})", signal.label, signal.passive_signal_id))
        .collect::<Vec<_>>();
    let defaults = passive_signals
        .iter()
        .map(|signal| selected_passive_ids.contains(&signal.passive_signal_id))
        .collect::<Vec<_>>();

    let selection = MultiSelect::with_theme(theme)
        .with_prompt("Toggle passive signal subscriptions")
        .items(&labels)
        .defaults(&defaults)
        .interact()
        .context("Failed to select passive signals.")?;

    Ok(passive_selection_from_indices(passive_signals, selection))
}

fn passive_selection_from_indices(
    passive_signals: &[PassiveSignalSummary],
    selection: Vec<usize>,
) -> HashSet<String> {
    selection
        .into_iter()
        .map(|index| passive_signals[index].passive_signal_id.clone())
        .collect()
}

fn prompt_passive_parent(
    theme: &ColorfulTheme,
    passive_signals: &[PassiveSignalSummary],
) -> Result<PassiveSignalSummary> {
    let labels = passive_signals
        .iter()
        .map(|signal| format!("{} ({})", signal.label, signal.passive_signal_id))
        .collect::<Vec<_>>();

    let selection = Select::with_theme(theme)
        .with_prompt("Choose passive signal to expand its active signals")
        .items(&labels)
        .default(0)
        .interact()
        .context("Failed to choose passive signal.")?;

    Ok(passive_parent_from_index(passive_signals, selection))
}

fn passive_parent_from_index(
    passive_signals: &[PassiveSignalSummary],
    selection: usize,
) -> PassiveSignalSummary {
    passive_signals[selection].clone()
}

fn prompt_active_signal_selection(
    theme: &ColorfulTheme,
    active_signals: &[ActiveSignalSummary],
    selected_active_ids: &HashSet<String>,
) -> Result<HashSet<String>> {
    let labels = active_signals
        .iter()
        .map(|signal| format!("{} ({})", signal.label, signal.active_signal_id))
        .collect::<Vec<_>>();
    let defaults = active_signals
        .iter()
        .map(|signal| selected_active_ids.contains(&signal.active_signal_id))
        .collect::<Vec<_>>();

    let selection = MultiSelect::with_theme(theme)
        .with_prompt("Toggle active signal subscriptions")
        .items(&labels)
        .defaults(&defaults)
        .interact()
        .context("Failed to select active signals.")?;

    Ok(active_selection_from_indices(
        active_signals,
        selected_active_ids,
        selection,
    ))
}

fn active_selection_from_indices(
    active_signals: &[ActiveSignalSummary],
    selected_active_ids: &HashSet<String>,
    selection: Vec<usize>,
) -> HashSet<String> {
    let mut next_selected = selected_active_ids.clone();
    next_selected.retain(|signal_id| {
        active_signals
            .iter()
            .all(|signal| signal.active_signal_id != *signal_id)
    });
    for index in selection {
        next_selected.insert(active_signals[index].active_signal_id.clone());
    }

    next_selected
}

fn resolve_api_key_input(value: Option<String>, stdin: bool) -> Result<String> {
    if let Some(value) = value {
        validate_api_key(value)
    } else if stdin {
        let mut buffer = String::new();
        io::stdin()
            .read_to_string(&mut buffer)
            .context("Failed to read API key from stdin.")?;
        validate_api_key(buffer)
    } else {
        let input = rpassword::prompt_password("Disc API key: ")
            .context("Failed to read API key from terminal prompt.")?;
        validate_api_key(input)
    }
}

fn strip_ansi_sequences(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some('[') = chars.next() {
                for next in chars.by_ref() {
                    if matches!(next, '\x40'..='\x7e') {
                        break;
                    }
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn validate_api_key(raw_value: String) -> Result<String> {
    let trimmed = strip_ansi_sequences(&raw_value).trim().to_owned();
    if trimmed.is_empty() {
        anyhow::bail!("API key cannot be empty.");
    }

    Ok(trimmed)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio::net::{TcpListener as AsyncTcpListener, TcpStream};
    use tokio::task::JoinHandle as TokioJoinHandle;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::{
        handshake::server::{Request, Response},
        http::header::SEC_WEBSOCKET_PROTOCOL,
        protocol::Message,
    };

    use crate::cli::{
        ActiveSignalsCommand, JsonOutputFormat, ListOutputFormat, StreamOutputFilter,
        StreamOutputFormat, WindowSemantics,
    };
    use crate::config::{StoredAuth, StoredConfig};

    use super::*;

    fn test_store(name: &str) -> ConfigStore {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        ConfigStore::from_root(std::env::temp_dir().join(format!(
            "disc-cli-command-{name}-{}-{unique}",
            std::process::id()
        )))
    }

    fn cleanup(store: &ConfigStore) {
        if store.root_dir().exists() {
            std::fs::remove_dir_all(store.root_dir()).expect("remove command store");
        }
    }

    fn spawn_server(status: &str, body: &str) -> (String, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
        let address = listener.local_addr().expect("server address");
        let status = status.to_owned();
        let body = body.to_owned();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = vec![0; 4096];
            let length = stream.read(&mut request).expect("read");
            sender
                .send(String::from_utf8_lossy(&request[..length]).into_owned())
                .expect("send request");
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).expect("respond");
        });
        (format!("http://{address}"), receiver)
    }

    fn spawn_response_sequence(bodies: Vec<&str>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind sequence server");
        let address = listener.local_addr().expect("sequence server address");
        let bodies = bodies.into_iter().map(str::to_owned).collect::<Vec<_>>();
        std::thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().expect("accept sequence request");
                let mut request = vec![0; 4096];
                let _ = stream.read(&mut request).expect("read sequence request");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("respond to sequence request");
            }
        });
        format!("http://{address}")
    }

    struct MockSubscriptionPrompter {
        actions: VecDeque<usize>,
        passive_selections: VecDeque<HashSet<String>>,
        passive_parents: VecDeque<PassiveSignalSummary>,
        active_selections: VecDeque<HashSet<String>>,
        has_waited_for_shutdown: bool,
    }

    impl SubscriptionPrompter for MockSubscriptionPrompter {
        fn choose_action(&mut self) -> Result<usize> {
            Ok(self.actions.pop_front().expect("next action"))
        }

        fn select_passive_signals(
            &mut self,
            _passive_signals: &[PassiveSignalSummary],
            _selected_passive_ids: &HashSet<String>,
        ) -> Result<HashSet<String>> {
            Ok(self
                .passive_selections
                .pop_front()
                .expect("passive selection"))
        }

        fn choose_passive_parent(
            &mut self,
            _passive_signals: &[PassiveSignalSummary],
        ) -> Result<PassiveSignalSummary> {
            Ok(self.passive_parents.pop_front().expect("passive parent"))
        }

        fn select_active_signals(
            &mut self,
            _active_signals: &[ActiveSignalSummary],
            _selected_active_ids: &HashSet<String>,
        ) -> Result<HashSet<String>> {
            Ok(self
                .active_selections
                .pop_front()
                .expect("active selection"))
        }

        fn wait_for_shutdown(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
            self.has_waited_for_shutdown = true;
            Box::pin(std::future::ready(Ok(())))
        }
    }

    async fn accept_websocket(stream: TcpStream) -> WebSocketStream<TcpStream> {
        tokio_tungstenite::accept_hdr_async(stream, |request: &Request, mut response: Response| {
            let protocol = request
                .headers()
                .get(SEC_WEBSOCKET_PROTOCOL)
                .expect("requested protocol")
                .to_str()
                .expect("valid requested protocol")
                .split(',')
                .next()
                .expect("at least one protocol")
                .trim();
            response.headers_mut().insert(
                SEC_WEBSOCKET_PROTOCOL,
                protocol.parse().expect("valid selected protocol"),
            );
            Ok(response)
        })
        .await
        .expect("websocket handshake")
    }

    async fn spawn_websocket_event() -> (String, TokioJoinHandle<()>) {
        let listener = AsyncTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind websocket");
        let address = listener.local_addr().expect("websocket address");
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept websocket");
            let mut websocket = accept_websocket(stream).await;
            let subscribe = websocket.next().await.expect("subscribe").expect("frame");
            assert!(matches!(subscribe, Message::Binary(_)));
            websocket
                .send(Message::Text(r#"{"type":"READY"}"#.into()))
                .await
                .expect("send control event");
            websocket
                .send(Message::Text(
                    r#"{"sk":"PASSIVE_SIGNAL:signal:ordinal","sq":1,"k":"psr","at":1,"p":{"value":7}}"#
                        .into(),
                ))
                .await
                .expect("send event");
            websocket.close(None).await.expect("close websocket");
        });
        (format!("ws://{address}"), task)
    }

    fn stream_options_once() -> StreamOptions {
        StreamOptions {
            output: StreamOutputFilter::Data,
            window_semantics: WindowSemantics::Ordinal,
            backfill: false,
            backfill_from: None,
            backfill_to: None,
            backfill_count: None,
            include_status: false,
            once: true,
            timeout: Some(Duration::from_secs(1)),
            no_reconnect: true,
        }
    }

    #[test]
    fn api_key_validation_trims_whitespace_and_terminal_sequences() {
        assert_eq!(
            validate_api_key(" \u{1b}[31msecret\u{1b}[0m \n".to_owned()).expect("valid key"),
            "secret"
        );
        assert_eq!(strip_ansi_sequences("plain"), "plain");
        assert_eq!(strip_ansi_sequences("\u{1b}xhidden"), "hidden");
        assert!(
            validate_api_key(" \n".to_owned())
                .expect_err("empty key")
                .to_string()
                .contains("cannot be empty")
        );
        assert_eq!(
            resolve_api_key_input(Some(" direct ".to_owned()), false).expect("direct key"),
            "direct"
        );
    }

    #[test]
    fn config_commands_show_set_and_reset_stored_values() {
        let store = test_store("config");

        run_config(ConfigCommand::Show, &store).expect("show defaults");
        run_config(
            ConfigCommand::Set {
                http_base_url: Some("https://http".to_owned()),
                ws_url: Some("wss://ws".to_owned()),
                client_id: Some("client".to_owned()),
            },
            &store,
        )
        .expect("set config");
        let stored = store.load_config().expect("stored config");
        assert_eq!(stored.http_base_url.as_deref(), Some("https://http"));
        assert_eq!(stored.ws_url.as_deref(), Some("wss://ws"));
        assert_eq!(stored.client_id.as_deref(), Some("client"));

        run_config(
            ConfigCommand::Set {
                http_base_url: None,
                ws_url: None,
                client_id: None,
            },
            &store,
        )
        .expect("no-op set");
        run_config(ConfigCommand::Show, &store).expect("show stored");
        run_config(ConfigCommand::Reset, &store).expect("reset config");
        let reset = store.load_config().expect("reset config");
        assert!(reset.http_base_url.is_none());
        assert!(reset.ws_url.is_none());
        assert!(reset.client_id.is_none());
        cleanup(&store);
    }

    #[tokio::test]
    async fn auth_set_clear_and_whoami_cover_stored_and_remote_flows() {
        let store = test_store("auth");
        run_auth(
            AuthCommand::ApiKey(ApiKeyCommand::Set {
                value: Some(" secret ".to_owned()),
                stdin: false,
            }),
            None,
            Some("https://http".to_owned()),
            Some("wss://ws".to_owned()),
            Some("client".to_owned()),
            &store,
        )
        .await
        .expect("set key");
        assert_eq!(
            store.load_auth().expect("load auth").expect("auth").api_key,
            "secret"
        );
        let config = store.load_config().expect("config");
        assert_eq!(config.http_base_url.as_deref(), Some("https://http"));
        assert_eq!(config.ws_url.as_deref(), Some("wss://ws"));
        assert_eq!(config.client_id.as_deref(), Some("client"));

        let (base_url, request) = spawn_server(
            "200 OK",
            r#"{"authType":"API_KEY","authTokenId":"token","sessionId":null,"apiKeyId":"key","userId":"7","userType":"individual","expiresAt":null,"revalidateAt":"later"}"#,
        );
        run_auth(
            AuthCommand::Whoami {
                format: JsonOutputFormat::Ndjson,
            },
            Some("secret".to_owned()),
            Some(base_url),
            None,
            None,
            &store,
        )
        .await
        .expect("whoami");
        assert!(
            request
                .recv()
                .expect("request")
                .starts_with("GET /validate ")
        );

        run_auth(AuthCommand::Clear, None, None, None, None, &store)
            .await
            .expect("clear existing");
        run_auth(AuthCommand::Clear, None, None, None, None, &store)
            .await
            .expect("clear absent");
        cleanup(&store);
    }

    #[tokio::test]
    async fn passive_and_active_signal_read_commands_use_http_client() {
        let store = test_store("signals");
        let cases = [
            (
                SignalsCommand::Passive(PassiveSignalsCommand::List {
                    format: ListOutputFormat::Json,
                }),
                r#"{"passiveSignals":[]}"#,
                "/passive-signals",
            ),
            (
                SignalsCommand::Passive(PassiveSignalsCommand::Get {
                    passive_signal_id: "passive".to_owned(),
                    format: JsonOutputFormat::Json,
                }),
                r#"{"passiveSignalId":"passive"}"#,
                "/passive-signals/passive",
            ),
            (
                SignalsCommand::Active(ActiveSignalsCommand::List {
                    passive_signal_id: "passive".to_owned(),
                    format: ListOutputFormat::Ndjson,
                }),
                r#"{"activeSignals":[]}"#,
                "/passive-signals/passive/active-signals",
            ),
            (
                SignalsCommand::Active(ActiveSignalsCommand::Get {
                    active_signal_id: "active".to_owned(),
                    format: JsonOutputFormat::Ndjson,
                }),
                r#"{"activeSignalId":"active"}"#,
                "/active-signals/active",
            ),
        ];

        for (command, body, expected_path) in cases {
            let (base_url, request) = spawn_server("200 OK", body);
            run_signals(
                command,
                Some("key".to_owned()),
                Some(base_url),
                Some("ws://unused".to_owned()),
                None,
                &store,
            )
            .await
            .expect("signal command");
            assert!(
                request
                    .recv()
                    .expect("request")
                    .starts_with(&format!("GET {expected_path} "))
            );
        }
        cleanup(&store);
    }

    #[tokio::test]
    async fn stream_and_tail_commands_write_websocket_events() {
        let store = test_store("stream-output");
        std::fs::create_dir_all(store.root_dir()).expect("create output directory");
        let destination = store.root_dir().join("events.ndjson");
        let (ws_url, server) = spawn_websocket_event().await;
        let stream = StreamCommand {
            signal_id: "signal".to_owned(),
            options: stream_options_once(),
            format: StreamOutputFormat::Ndjson,
            destination: Some(destination.clone()),
        };

        run_stream_command(
            SubscriptionKind::Passive,
            &ws_url,
            "key",
            Some("client"),
            &stream,
        )
        .await
        .expect("stream command");
        server.await.expect("stream server");
        assert!(
            std::fs::read_to_string(&destination)
                .expect("stream output")
                .contains("\"streamKey\":\"PASSIVE_SIGNAL:signal:ordinal\"")
        );

        let (ws_url, server) = spawn_websocket_event().await;
        let tail = TailCommand {
            signal_id: "signal".to_owned(),
            output: StreamOutputFilter::All,
            window_semantics: WindowSemantics::Ordinal,
            backfill: false,
            backfill_from: None,
            backfill_to: None,
            backfill_count: None,
            include_status: false,
            once: true,
            timeout: Some(Duration::from_secs(1)),
            no_reconnect: true,
            format: StreamOutputFormat::Pretty,
        };
        run_tail_command(SubscriptionKind::Active, &ws_url, "key", None, &tail)
            .await
            .expect("tail command");
        server.await.expect("tail server");

        let (ws_url, server) = spawn_websocket_event().await;
        let mut continuous_stream = stream;
        continuous_stream.options.once = false;
        run_stream_command(
            SubscriptionKind::Passive,
            &ws_url,
            "key",
            None,
            &continuous_stream,
        )
        .await
        .expect("continuous stream command");
        server.await.expect("continuous stream server");

        let (ws_url, server) = spawn_websocket_event().await;
        let mut continuous_tail = tail;
        continuous_tail.once = false;
        run_tail_command(
            SubscriptionKind::Active,
            &ws_url,
            "key",
            None,
            &continuous_tail,
        )
        .await
        .expect("continuous tail command");
        server.await.expect("continuous tail server");
        cleanup(&store);
    }

    #[tokio::test]
    async fn signal_subscription_routes_cover_passive_and_active_variants() {
        let store = test_store("signal-subscriptions");

        for is_active in [false, true] {
            let (ws_url, server) = spawn_websocket_event().await;
            let stream = StreamCommand {
                signal_id: "signal".to_owned(),
                options: stream_options_once(),
                format: StreamOutputFormat::Json,
                destination: None,
            };
            let command = if is_active {
                SignalsCommand::Active(ActiveSignalsCommand::Subscribe(stream))
            } else {
                SignalsCommand::Passive(PassiveSignalsCommand::Subscribe(stream))
            };
            run_signals(
                command,
                Some("key".to_owned()),
                Some("http://unused".to_owned()),
                Some(ws_url),
                None,
                &store,
            )
            .await
            .expect("subscribe route");
            server.await.expect("subscribe server");

            let (ws_url, server) = spawn_websocket_event().await;
            let tail = TailCommand {
                signal_id: "signal".to_owned(),
                output: StreamOutputFilter::All,
                window_semantics: WindowSemantics::Ordinal,
                backfill: false,
                backfill_from: None,
                backfill_to: None,
                backfill_count: None,
                include_status: false,
                once: true,
                timeout: Some(Duration::from_secs(1)),
                no_reconnect: true,
                format: StreamOutputFormat::Ndjson,
            };
            let command = if is_active {
                SignalsCommand::Active(ActiveSignalsCommand::Tail(tail))
            } else {
                SignalsCommand::Passive(PassiveSignalsCommand::Tail(tail))
            };
            run_signals(
                command,
                Some("key".to_owned()),
                Some("http://unused".to_owned()),
                Some(ws_url),
                None,
                &store,
            )
            .await
            .expect("tail route");
            server.await.expect("tail server");
        }
        cleanup(&store);
    }

    #[tokio::test]
    async fn root_dispatch_uses_the_supplied_config_store() {
        let store = test_store("root-dispatch");
        let config_cli = Cli {
            api_key: None,
            http_base_url: None,
            ws_url: None,
            client_id: None,
            command: RootCommand::Config(ConfigCommand::Reset),
        };
        run_with_store(config_cli, &store)
            .await
            .expect("config dispatch");

        let auth_cli = Cli {
            api_key: None,
            http_base_url: None,
            ws_url: None,
            client_id: None,
            command: RootCommand::Auth(AuthCommand::Clear),
        };
        run_with_store(auth_cli, &store)
            .await
            .expect("auth dispatch");

        let (base_url, request) = spawn_server("200 OK", r#"{"passiveSignals":[]}"#);
        let signals_cli = Cli {
            api_key: Some("key".to_owned()),
            http_base_url: Some(base_url),
            ws_url: Some("ws://unused".to_owned()),
            client_id: Some("client".to_owned()),
            command: RootCommand::Signals(SignalsCommand::Passive(PassiveSignalsCommand::List {
                format: ListOutputFormat::Table,
            })),
        };
        run_with_store(signals_cli, &store)
            .await
            .expect("signals dispatch");
        assert!(
            request
                .recv()
                .expect("signals request")
                .starts_with("GET /passive-signals ")
        );
        cleanup(&store);
    }

    #[tokio::test]
    async fn interactive_subscription_handles_empty_signal_catalogue() {
        let store = test_store("interactive-empty");
        std::fs::create_dir_all(store.root_dir()).expect("create interactive directory");
        let base_url = spawn_response_sequence(vec![r#"{"passiveSignals":[]}"#]);
        let client = DiscApiClient::new(base_url, "key").expect("interactive client");
        let command = InteractiveSubscribeCommand {
            options: stream_options_once(),
            format: StreamOutputFormat::Ndjson,
            destination: store.root_dir().join("events.ndjson"),
        };
        let mut prompter = MockSubscriptionPrompter {
            actions: VecDeque::from([1, 3]),
            passive_selections: VecDeque::new(),
            passive_parents: VecDeque::new(),
            active_selections: VecDeque::new(),
            has_waited_for_shutdown: false,
        };

        run_interactive_subscribe_with_prompter(
            &client,
            "ws://unused",
            "key",
            None,
            &command,
            &mut prompter,
        )
        .await
        .expect("empty interactive flow");
        assert!(!prompter.has_waited_for_shutdown);
        cleanup(&store);
    }

    #[tokio::test]
    async fn interactive_subscription_edits_signals_caches_results_and_finishes() {
        let store = test_store("interactive-edit");
        std::fs::create_dir_all(store.root_dir()).expect("create interactive directory");
        let base_url = spawn_response_sequence(vec![
            r#"{"passiveSignals":[{"passiveSignalId":"p1","label":"One"},{"passiveSignalId":"p2","label":"Two"}]}"#,
            r#"{"activeSignals":[{"activeSignalId":"a1","passiveSignalId":"p1","label":"Active"}]}"#,
            r#"{"activeSignals":[]}"#,
        ]);
        let client = DiscApiClient::new(base_url, "key").expect("interactive client");
        let command = InteractiveSubscribeCommand {
            options: stream_options_once(),
            format: StreamOutputFormat::Json,
            destination: store.root_dir().join("events.json"),
        };
        let p1 = PassiveSignalSummary {
            passive_signal_id: "p1".to_owned(),
            label: "One".to_owned(),
        };
        let p2 = PassiveSignalSummary {
            passive_signal_id: "p2".to_owned(),
            label: "Two".to_owned(),
        };
        let mut prompter = MockSubscriptionPrompter {
            actions: VecDeque::from([0, 1, 1, 1, 2]),
            passive_selections: VecDeque::from([HashSet::from(["p1".to_owned()])]),
            passive_parents: VecDeque::from([p1.clone(), p1, p2]),
            active_selections: VecDeque::from([
                HashSet::from(["a1".to_owned()]),
                HashSet::from(["a1".to_owned()]),
            ]),
            has_waited_for_shutdown: false,
        };

        run_interactive_subscribe_with_prompter(
            &client,
            "not a websocket url",
            "key",
            Some("client"),
            &command,
            &mut prompter,
        )
        .await
        .expect("interactive edit flow");
        assert!(prompter.has_waited_for_shutdown);
        cleanup(&store);
    }

    #[tokio::test]
    async fn interactive_entrypoint_reports_destination_creation_errors() {
        let store = test_store("interactive-entrypoint");
        let base_url = spawn_response_sequence(vec![r#"{"passiveSignals":[]}"#]);
        let client = DiscApiClient::new(base_url, "key").expect("interactive client");
        let command = InteractiveSubscribeCommand {
            options: stream_options_once(),
            format: StreamOutputFormat::Ndjson,
            destination: store.root_dir().join("missing").join("events.ndjson"),
        };

        let error = run_interactive_subscribe(&client, "ws://unused", "key", None, &command)
            .await
            .expect_err("destination error");
        assert!(error.to_string().contains("Failed to open"));
        cleanup(&store);
    }

    #[tokio::test]
    async fn interactive_signal_route_dispatches_to_the_entrypoint() {
        let store = test_store("interactive-route");
        let base_url = spawn_response_sequence(vec![r#"{"passiveSignals":[]}"#]);
        let command = InteractiveSubscribeCommand {
            options: stream_options_once(),
            format: StreamOutputFormat::Ndjson,
            destination: store.root_dir().join("missing").join("events.ndjson"),
        };

        let error = run_signals(
            SignalsCommand::Subscribe(command),
            Some("key".to_owned()),
            Some(base_url),
            Some("ws://unused".to_owned()),
            Some("client".to_owned()),
            &store,
        )
        .await
        .expect_err("interactive destination error");

        assert!(error.to_string().contains("Failed to open"));
        cleanup(&store);
    }

    #[test]
    fn dialog_prompter_reports_unavailable_terminal_input() {
        let mut prompter = DialogSubscriptionPrompter::new();
        let passive_signals = vec![PassiveSignalSummary {
            passive_signal_id: "passive".to_owned(),
            label: "Passive".to_owned(),
        }];
        let active_signals = vec![ActiveSignalSummary {
            active_signal_id: "active".to_owned(),
            passive_signal_id: "passive".to_owned(),
            label: "Active".to_owned(),
        }];

        assert!(prompter.choose_action().is_err());
        assert!(
            prompter
                .select_passive_signals(&passive_signals, &HashSet::new())
                .is_err()
        );
        assert!(prompter.choose_passive_parent(&passive_signals).is_err());
        assert!(
            prompter
                .select_active_signals(&active_signals, &HashSet::new())
                .is_err()
        );
    }

    #[tokio::test]
    async fn dialog_shutdown_waiter_can_be_cancelled() {
        let mut prompter = DialogSubscriptionPrompter::new();
        let result =
            tokio::time::timeout(Duration::from_millis(1), prompter.wait_for_shutdown()).await;

        assert!(result.is_err());
    }

    #[test]
    fn dialog_selection_results_map_indices_and_preserve_other_parents() {
        let passive_signals = vec![
            PassiveSignalSummary {
                passive_signal_id: "p1".to_owned(),
                label: "One".to_owned(),
            },
            PassiveSignalSummary {
                passive_signal_id: "p2".to_owned(),
                label: "Two".to_owned(),
            },
        ];
        assert_eq!(
            passive_selection_from_indices(&passive_signals, vec![1]),
            HashSet::from(["p2".to_owned()])
        );
        assert_eq!(
            passive_parent_from_index(&passive_signals, 0).passive_signal_id,
            "p1"
        );

        let active_signals = vec![
            ActiveSignalSummary {
                active_signal_id: "a1".to_owned(),
                passive_signal_id: "p1".to_owned(),
                label: "One".to_owned(),
            },
            ActiveSignalSummary {
                active_signal_id: "a2".to_owned(),
                passive_signal_id: "p1".to_owned(),
                label: "Two".to_owned(),
            },
        ];
        assert_eq!(
            active_selection_from_indices(
                &active_signals,
                &HashSet::from(["other-parent".to_owned(), "a1".to_owned()]),
                vec![1],
            ),
            HashSet::from(["other-parent".to_owned(), "a2".to_owned()])
        );
    }

    #[test]
    fn tail_options_copy_every_stream_setting() {
        let tail = TailCommand {
            signal_id: "signal".to_owned(),
            output: StreamOutputFilter::Status,
            window_semantics: WindowSemantics::Elapsed,
            backfill: true,
            backfill_from: Some(1),
            backfill_to: Some(2),
            backfill_count: Some(3),
            include_status: true,
            once: true,
            timeout: Some(Duration::from_secs(4)),
            no_reconnect: true,
            format: StreamOutputFormat::Json,
        };

        let options = stream_options_from_tail(&tail);

        assert_eq!(options.output, StreamOutputFilter::Status);
        assert_eq!(options.window_semantics, WindowSemantics::Elapsed);
        assert!(options.backfill);
        assert_eq!(options.backfill_from, Some(1));
        assert_eq!(options.backfill_to, Some(2));
        assert_eq!(options.backfill_count, Some(3));
        assert!(options.include_status);
        assert!(options.once);
        assert_eq!(options.timeout, Some(Duration::from_secs(4)));
        assert!(options.no_reconnect);
    }

    #[tokio::test]
    async fn subscription_task_reconciliation_adds_preserves_and_aborts_tasks() {
        let writer = create_stdout_writer();
        let options = StreamOptions {
            output: StreamOutputFilter::Data,
            window_semantics: WindowSemantics::Ordinal,
            backfill: false,
            backfill_from: None,
            backfill_to: None,
            backfill_count: None,
            include_status: false,
            once: false,
            timeout: Some(Duration::from_secs(30)),
            no_reconnect: true,
        };
        let mut tasks = HashMap::new();
        let selected_passive = HashSet::from(["passive".to_owned()]);
        let selected_active = HashSet::from(["active".to_owned()]);

        reconcile_subscriptions(
            &mut tasks,
            ReconcileSubscriptionContext {
                writer: &writer,
                ws_url: "not a websocket url",
                api_key: "key",
                client_id: Some("client"),
                options: &options,
                format: StreamOutputFormat::Ndjson,
            },
            &selected_passive,
            &selected_active,
        );
        assert_eq!(tasks.len(), 2);

        reconcile_subscriptions(
            &mut tasks,
            ReconcileSubscriptionContext {
                writer: &writer,
                ws_url: "not a websocket url",
                api_key: "key",
                client_id: None,
                options: &options,
                format: StreamOutputFormat::Pretty,
            },
            &selected_passive,
            &selected_active,
        );
        assert_eq!(tasks.len(), 2);

        reconcile_subscriptions(
            &mut tasks,
            ReconcileSubscriptionContext {
                writer: &writer,
                ws_url: "not a websocket url",
                api_key: "key",
                client_id: None,
                options: &options,
                format: StreamOutputFormat::Pretty,
            },
            &HashSet::new(),
            &HashSet::new(),
        );
        assert!(tasks.is_empty());

        let (ws_url, server) = spawn_websocket_event().await;
        reconcile_subscriptions(
            &mut tasks,
            ReconcileSubscriptionContext {
                writer: &writer,
                ws_url: &ws_url,
                api_key: "key",
                client_id: None,
                options: &options,
                format: StreamOutputFormat::Ndjson,
            },
            &HashSet::from(["signal".to_owned()]),
            &HashSet::new(),
        );
        server.await.expect("reconciled subscription server");
        let spec = SubscriptionSpec {
            kind: SubscriptionKind::Passive,
            signal_id: "signal".to_owned(),
        };
        let task = tasks.remove(&spec).expect("reconciled subscription task");
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("reconciled task timeout")
            .expect("reconciled task");

        let task = tokio::spawn(std::future::pending::<()>());
        tasks.insert(
            SubscriptionSpec {
                kind: SubscriptionKind::Passive,
                signal_id: "one".to_owned(),
            },
            task,
        );
        abort_all_tasks(&mut tasks);
        assert!(tasks.is_empty());
    }

    #[test]
    fn subscription_summary_handles_empty_and_selected_sets() {
        let passive_signals = vec![PassiveSignalSummary {
            passive_signal_id: "passive".to_owned(),
            label: "Passive".to_owned(),
        }];
        let active_signals = vec![ActiveSignalSummary {
            active_signal_id: "active".to_owned(),
            passive_signal_id: "passive".to_owned(),
            label: "Active".to_owned(),
        }];
        let destination = PathBuf::from("signals.ndjson");

        print_subscription_summary(
            &passive_signals,
            &HashMap::new(),
            &HashSet::new(),
            &HashSet::new(),
            &destination,
        );
        print_subscription_summary(
            &passive_signals,
            &HashMap::from([("passive".to_owned(), active_signals)]),
            &HashSet::from(["passive".to_owned()]),
            &HashSet::from(["active".to_owned()]),
            &destination,
        );
    }

    #[tokio::test]
    async fn signal_commands_require_auth_before_network_work() {
        let store = test_store("missing-auth");

        let error = run_signals(
            SignalsCommand::Passive(PassiveSignalsCommand::List {
                format: ListOutputFormat::Table,
            }),
            None,
            None,
            None,
            None,
            &store,
        )
        .await
        .expect_err("missing auth");

        assert!(error.to_string().contains("API key is not configured"));
        cleanup(&store);
    }

    #[test]
    fn stored_config_fixture_is_serializable() {
        let value = StoredConfig {
            http_base_url: None,
            ws_url: None,
            client_id: None,
        };
        let auth = StoredAuth {
            api_key: "key".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(value).expect("config")["ws_url"],
            Value::Null
        );
        assert_eq!(serde_json::to_value(auth).expect("auth")["api_key"], "key");
    }
}
