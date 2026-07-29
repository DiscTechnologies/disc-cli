use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::path::Path;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractiveAction {
    EditPassive,
    EditActive,
    Finish,
    Quit,
}

struct DialoguerSubscriptionPrompts {
    theme: ColorfulTheme,
    #[cfg(test)]
    script: Option<ScriptedSubscriptionPrompts>,
}

#[cfg(test)]
struct ScriptedSubscriptionPrompts {
    actions: std::collections::VecDeque<InteractiveAction>,
    passive_selection: HashSet<String>,
    passive_parents: std::collections::VecDeque<String>,
    active_selection: HashSet<String>,
    did_wait_for_stop: bool,
}

impl DialoguerSubscriptionPrompts {
    fn new() -> Self {
        Self {
            theme: ColorfulTheme::default(),
            #[cfg(test)]
            script: None,
        }
    }

    #[cfg(test)]
    fn scripted(actions: Vec<InteractiveAction>) -> Self {
        Self {
            theme: ColorfulTheme::default(),
            script: Some(ScriptedSubscriptionPrompts {
                actions: actions.into(),
                passive_selection: HashSet::new(),
                passive_parents: std::collections::VecDeque::new(),
                active_selection: HashSet::new(),
                did_wait_for_stop: false,
            }),
        }
    }

    fn choose_action(&mut self) -> Result<InteractiveAction> {
        #[cfg(test)]
        if let Some(script) = &mut self.script {
            return Ok(script.actions.pop_front().expect("scripted action"));
        }

        let action = Select::with_theme(&self.theme)
            .with_prompt("Manage subscriptions")
            .items(&[
                "Edit passive signals",
                "Edit active signals",
                "Finish and keep current subscriptions running",
                "Quit and stop all subscriptions",
            ])
            .default(0)
            .interact()
            .context("Failed to read interactive selection.")?;
        match action {
            0 => Ok(InteractiveAction::EditPassive),
            1 => Ok(InteractiveAction::EditActive),
            2 => Ok(InteractiveAction::Finish),
            3 => Ok(InteractiveAction::Quit),
            _ => unreachable!(),
        }
    }

    fn select_passive_signals(
        &mut self,
        passive_signals: &[PassiveSignalSummary],
        selected_passive_ids: &HashSet<String>,
    ) -> Result<HashSet<String>> {
        #[cfg(test)]
        if let Some(script) = &self.script {
            return Ok(script.passive_selection.clone());
        }

        prompt_passive_signal_selection(&self.theme, passive_signals, selected_passive_ids)
    }

    fn select_passive_parent(
        &mut self,
        passive_signals: &[PassiveSignalSummary],
    ) -> Result<PassiveSignalSummary> {
        #[cfg(test)]
        if let Some(script) = &mut self.script {
            let parent_id = script
                .passive_parents
                .pop_front()
                .expect("scripted passive parent");
            return passive_signals
                .iter()
                .find(|signal| signal.passive_signal_id == parent_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing scripted passive parent"));
        }

        prompt_passive_parent(&self.theme, passive_signals)
    }

    fn select_active_signals(
        &mut self,
        active_signals: &[ActiveSignalSummary],
        selected_active_ids: &HashSet<String>,
    ) -> Result<HashSet<String>> {
        #[cfg(test)]
        if let Some(script) = &self.script {
            return Ok(script.active_selection.clone());
        }

        prompt_active_signal_selection(&self.theme, active_signals, selected_active_ids)
    }

    async fn wait_for_stop(&mut self) -> Result<()> {
        #[cfg(test)]
        if let Some(script) = &mut self.script {
            script.did_wait_for_stop = true;
            return Ok(());
        }

        tokio::signal::ctrl_c()
            .await
            .context("Failed to wait for Ctrl+C.")
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
    let mut prompts = DialoguerSubscriptionPrompts::new();
    run_interactive_subscribe_with(client, ws_url, api_key, client_id, command, &mut prompts).await
}

async fn run_interactive_subscribe_with(
    client: &DiscApiClient,
    ws_url: &str,
    api_key: &str,
    client_id: Option<&str>,
    command: &InteractiveSubscribeCommand,
    prompts: &mut DialoguerSubscriptionPrompts,
) -> Result<()> {
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

        match prompts.choose_action()? {
            InteractiveAction::EditPassive => {
                selected_passive_ids =
                    prompts.select_passive_signals(&passive_signals, &selected_passive_ids)?;
            }
            InteractiveAction::EditActive => {
                if passive_signals.is_empty() {
                    println!("No passive signals available.");
                } else {
                    let passive_signal = prompts.select_passive_parent(&passive_signals)?;
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
                        selected_active_ids =
                            prompts.select_active_signals(&active_signals, &selected_active_ids)?;
                    }
                }
            }
            InteractiveAction::Finish => {
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
                prompts.wait_for_stop().await?;
                abort_all_tasks(&mut tasks);
                return Ok(());
            }
            InteractiveAction::Quit => {
                abort_all_tasks(&mut tasks);
                return Ok(());
            }
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
    let (labels, defaults) = passive_selection_options(passive_signals, selected_passive_ids);

    let selection = MultiSelect::with_theme(theme)
        .with_prompt("Toggle passive signal subscriptions")
        .items(&labels)
        .defaults(&defaults)
        .interact()
        .context("Failed to select passive signals.")?;

    Ok(selected_passive_signal_ids(passive_signals, selection))
}

fn passive_selection_options(
    passive_signals: &[PassiveSignalSummary],
    selected_passive_ids: &HashSet<String>,
) -> (Vec<String>, Vec<bool>) {
    let labels = passive_signals
        .iter()
        .map(|signal| format!("{} ({})", signal.label, signal.passive_signal_id))
        .collect::<Vec<_>>();
    let defaults = passive_signals
        .iter()
        .map(|signal| selected_passive_ids.contains(&signal.passive_signal_id))
        .collect::<Vec<_>>();
    (labels, defaults)
}

fn selected_passive_signal_ids(
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

    Ok(passive_signals[selection].clone())
}

fn prompt_active_signal_selection(
    theme: &ColorfulTheme,
    active_signals: &[ActiveSignalSummary],
    selected_active_ids: &HashSet<String>,
) -> Result<HashSet<String>> {
    let (labels, defaults) = active_selection_options(active_signals, selected_active_ids);

    let selection = MultiSelect::with_theme(theme)
        .with_prompt("Toggle active signal subscriptions")
        .items(&labels)
        .defaults(&defaults)
        .interact()
        .context("Failed to select active signals.")?;

    Ok(merge_active_signal_selection(
        active_signals,
        selected_active_ids,
        selection,
    ))
}

fn active_selection_options(
    active_signals: &[ActiveSignalSummary],
    selected_active_ids: &HashSet<String>,
) -> (Vec<String>, Vec<bool>) {
    let labels = active_signals
        .iter()
        .map(|signal| format!("{} ({})", signal.label, signal.active_signal_id))
        .collect::<Vec<_>>();
    let defaults = active_signals
        .iter()
        .map(|signal| selected_active_ids.contains(&signal.active_signal_id))
        .collect::<Vec<_>>();
    (labels, defaults)
}

fn merge_active_signal_selection(
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
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::fs;
    use std::io::{self, Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener as TokioTcpListener;
    use tokio::task::JoinHandle;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    use crate::cli::{
        ActiveSignalsCommand, ApiKeyCommand, AuthCommand, Cli, ConfigCommand,
        InteractiveSubscribeCommand, JsonOutputFormat, ListOutputFormat, PassiveSignalsCommand,
        RootCommand, SignalsCommand, StreamCommand, StreamOptions, StreamOutputFilter,
        StreamOutputFormat, TailCommand, WindowSemantics,
    };
    use crate::config::{ConfigStore, StoredConfig};
    use crate::http::{ActiveSignalSummary, DiscApiClient, PassiveSignalSummary};
    use crate::output::SharedWriter;
    use crate::ws::{SubscriptionKind, SubscriptionSpec};

    use super::{
        DialoguerSubscriptionPrompts, InteractiveAction, ReconcileSubscriptionContext,
        abort_all_tasks, active_selection_options, merge_active_signal_selection,
        passive_selection_options, print_subscription_summary, reconcile_subscriptions,
        resolve_api_key_input, run_auth, run_config, run_interactive_subscribe_with, run_signals,
        run_with_store, selected_passive_signal_ids, stream_options_from_tail,
        strip_ansi_sequences, validate_api_key,
    };

    fn temporary_root(test_name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "disc-cli-command-{test_name}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn stream_options() -> StreamOptions {
        StreamOptions {
            output: StreamOutputFilter::Data,
            window_semantics: WindowSemantics::Ordinal,
            backfill: false,
            backfill_from: None,
            backfill_to: None,
            backfill_count: None,
            include_status: false,
            once: false,
            timeout: Some(Duration::from_millis(1)),
            no_reconnect: true,
        }
    }

    fn serve_once(body: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
        let address = listener.local_addr().expect("server address");
        let body = body.to_owned();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let count = stream.read(&mut buffer).expect("read request");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });
        format!("http://{address}")
    }

    fn serve_json_responses(bodies: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
        let address = listener.local_addr().expect("server address");
        std::thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                loop {
                    let count = stream.read(&mut buffer).expect("read request");
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
        });
        format!("http://{address}")
    }

    async fn serve_websocket_data_once() -> (String, JoinHandle<()>) {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind WebSocket listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept connection");
            let mut socket =
                accept_hdr_async(stream, |_request: &Request, mut response: Response| {
                    response.headers_mut().insert(
                        "sec-websocket-protocol",
                        "apiKey-test-key".parse().expect("protocol response"),
                    );
                    Ok(response)
                })
                .await
                .expect("accept WebSocket");
            socket
                .next()
                .await
                .expect("subscription frame")
                .expect("valid subscription");
            socket
                .send(Message::Text(
                    serde_json::json!({
                        "type": "DATA",
                        "streamKey": "PASSIVE_SIGNAL:signal-one:ordinal",
                        "sequence": 1,
                        "payloadType": "result",
                        "emittedAtEpochMs": 100,
                        "payload": {"value": 42}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("send data event");
            let _ = socket.close(None).await;
        });
        (format!("ws://{address}"), server)
    }

    #[test]
    fn api_key_validation_strips_terminal_sequences_and_rejects_blank_values() {
        assert_eq!(strip_ansi_sequences("\u{1b}[32msecret\u{1b}[0m"), "secret");
        assert_eq!(
            validate_api_key("  \u{1b}[32msecret\u{1b}[0m\n".to_owned()).expect("valid key"),
            "secret"
        );
        assert!(
            validate_api_key(" \n\t ".to_owned())
                .expect_err("blank key")
                .to_string()
                .contains("cannot be empty")
        );
        assert_eq!(
            resolve_api_key_input(Some(" direct-key ".to_owned()), false).expect("direct API key"),
            "direct-key"
        );
    }

    #[test]
    fn config_commands_persist_update_and_reset_values() {
        let root = temporary_root("config");
        let store = ConfigStore::at_root(root.clone());

        run_config(ConfigCommand::Show, &store).expect("show defaults");
        run_config(
            ConfigCommand::Set {
                http_base_url: Some("https://api.example.test".to_owned()),
                ws_url: Some("wss://signals.example.test".to_owned()),
                client_id: Some("client-one".to_owned()),
            },
            &store,
        )
        .expect("set config");
        let stored = store.load_config().expect("load config");
        assert_eq!(
            stored.http_base_url.as_deref(),
            Some("https://api.example.test")
        );
        assert_eq!(stored.ws_url.as_deref(), Some("wss://signals.example.test"));
        assert_eq!(stored.client_id.as_deref(), Some("client-one"));
        run_config(ConfigCommand::Show, &store).expect("show stored config");

        run_config(
            ConfigCommand::Set {
                http_base_url: None,
                ws_url: Some("wss://changed.example.test".to_owned()),
                client_id: None,
            },
            &store,
        )
        .expect("partially update config");
        let stored = store.load_config().expect("load updated config");
        assert_eq!(
            stored.http_base_url.as_deref(),
            Some("https://api.example.test")
        );
        assert_eq!(stored.ws_url.as_deref(), Some("wss://changed.example.test"));

        run_config(ConfigCommand::Reset, &store).expect("reset config");
        let reset = store.load_config().expect("load reset config");
        assert!(reset.http_base_url.is_none());
        assert!(reset.ws_url.is_none());
        assert!(reset.client_id.is_none());
        fs::remove_dir_all(root).expect("remove config root");
    }

    #[tokio::test]
    async fn auth_set_and_clear_commands_manage_credentials_and_endpoint_options() {
        let root = temporary_root("auth");
        let store = ConfigStore::at_root(root.clone());

        run_auth(
            AuthCommand::ApiKey(ApiKeyCommand::Set {
                value: Some(" secret ".to_owned()),
                stdin: false,
            }),
            None,
            Some("https://api.example.test".to_owned()),
            Some("wss://signals.example.test".to_owned()),
            Some("client-one".to_owned()),
            &store,
        )
        .await
        .expect("set auth");
        assert_eq!(
            store
                .load_auth()
                .expect("load auth")
                .expect("stored auth")
                .api_key,
            "secret"
        );
        let config = store.load_config().expect("load endpoint config");
        assert_eq!(
            config.http_base_url.as_deref(),
            Some("https://api.example.test")
        );
        assert_eq!(config.ws_url.as_deref(), Some("wss://signals.example.test"));
        assert_eq!(config.client_id.as_deref(), Some("client-one"));

        run_auth(AuthCommand::Clear, None, None, None, None, &store)
            .await
            .expect("clear stored auth");
        assert!(store.load_auth().expect("load cleared auth").is_none());
        run_auth(AuthCommand::Clear, None, None, None, None, &store)
            .await
            .expect("clear absent auth");
        fs::remove_dir_all(root).expect("remove auth root");
    }

    #[tokio::test]
    async fn root_dispatcher_routes_config_auth_and_signal_commands() {
        let root = temporary_root("root-dispatch");
        let store = ConfigStore::at_root(root.clone());
        run_with_store(
            Cli {
                api_key: None,
                http_base_url: None,
                ws_url: None,
                client_id: None,
                command: RootCommand::Config(ConfigCommand::Show),
            },
            &store,
        )
        .await
        .expect("dispatch config");
        run_with_store(
            Cli {
                api_key: None,
                http_base_url: None,
                ws_url: None,
                client_id: None,
                command: RootCommand::Auth(AuthCommand::Clear),
            },
            &store,
        )
        .await
        .expect("dispatch auth");

        let body = serde_json::json!({
            "passiveSignals": [{
                "passiveSignalId": "passive-one",
                "label": "Revenue"
            }]
        })
        .to_string();
        run_with_store(
            Cli {
                api_key: Some("test-key".to_owned()),
                http_base_url: Some(serve_once(&body)),
                ws_url: None,
                client_id: Some("client-one".to_owned()),
                command: RootCommand::Signals(SignalsCommand::Passive(
                    PassiveSignalsCommand::List {
                        format: ListOutputFormat::Json,
                    },
                )),
            },
            &store,
        )
        .await
        .expect("dispatch signals");
        if root.exists() {
            fs::remove_dir_all(root).expect("remove dispatch root");
        }
    }

    #[tokio::test]
    async fn whoami_command_validates_and_renders_both_json_formats() {
        let root = temporary_root("whoami");
        let store = ConfigStore::at_root(root.clone());
        let body = serde_json::json!({
            "authType": "API_KEY",
            "authTokenId": "token-one",
            "sessionId": null,
            "apiKeyId": "key-one",
            "userId": "user-one",
            "userType": "SUBJECT",
            "expiresAt": null,
            "revalidateAt": "2026-07-28T12:00:00Z"
        })
        .to_string();

        for format in [JsonOutputFormat::Json, JsonOutputFormat::Ndjson] {
            run_auth(
                AuthCommand::Whoami { format },
                Some("test-key".to_owned()),
                Some(serve_once(&body)),
                None,
                None,
                &store,
            )
            .await
            .expect("whoami command");
        }
        if root.exists() {
            fs::remove_dir_all(root).expect("remove whoami root");
        }
    }

    #[tokio::test]
    async fn signal_list_and_get_commands_cover_passive_and_active_routes() {
        let root = temporary_root("signals");
        let store = ConfigStore::at_root(root.clone());
        let passive_list = serde_json::json!({
            "passiveSignals": [{
                "passiveSignalId": "passive-one",
                "label": "Revenue",
                "status": "active"
            }]
        })
        .to_string();
        for format in [
            ListOutputFormat::Json,
            ListOutputFormat::Ndjson,
            ListOutputFormat::Table,
        ] {
            run_signals(
                SignalsCommand::Passive(PassiveSignalsCommand::List { format }),
                Some("test-key".to_owned()),
                Some(serve_once(&passive_list)),
                None,
                None,
                &store,
            )
            .await
            .expect("passive list");
        }

        for format in [JsonOutputFormat::Json, JsonOutputFormat::Ndjson] {
            run_signals(
                SignalsCommand::Passive(PassiveSignalsCommand::Get {
                    passive_signal_id: "passive-one".to_owned(),
                    format,
                }),
                Some("test-key".to_owned()),
                Some(serve_once(
                    r#"{"passiveSignalId":"passive-one","label":"Revenue"}"#,
                )),
                None,
                None,
                &store,
            )
            .await
            .expect("passive get");
        }

        let active_list = serde_json::json!({
            "activeSignals": [{
                "activeSignalId": "active-one",
                "passiveSignalId": "passive-one",
                "label": "Revenue average",
                "isPaused": true
            }]
        })
        .to_string();
        for format in [
            ListOutputFormat::Json,
            ListOutputFormat::Ndjson,
            ListOutputFormat::Table,
        ] {
            run_signals(
                SignalsCommand::Active(ActiveSignalsCommand::List {
                    passive_signal_id: "passive-one".to_owned(),
                    format,
                }),
                Some("test-key".to_owned()),
                Some(serve_once(&active_list)),
                None,
                None,
                &store,
            )
            .await
            .expect("active list");
        }

        for format in [JsonOutputFormat::Json, JsonOutputFormat::Ndjson] {
            run_signals(
                SignalsCommand::Active(ActiveSignalsCommand::Get {
                    active_signal_id: "active-one".to_owned(),
                    format,
                }),
                Some("test-key".to_owned()),
                Some(serve_once(
                    r#"{"activeSignalId":"active-one","label":"Revenue average"}"#,
                )),
                None,
                None,
                &store,
            )
            .await
            .expect("active get");
        }
        if root.exists() {
            fs::remove_dir_all(root).expect("remove signals root");
        }
    }

    #[tokio::test]
    async fn subscribe_and_tail_commands_dispatch_passive_and_active_streams() {
        let root = temporary_root("stream-commands");
        let store = ConfigStore::at_root(root.clone());
        let destination = root.join("events.ndjson");
        fs::create_dir_all(&root).expect("create stream command root");
        let mut options = stream_options();
        options.once = true;
        options.timeout = Some(Duration::from_secs(1));

        for kind in [SubscriptionKind::Passive, SubscriptionKind::Active] {
            let (ws_url, server) = serve_websocket_data_once().await;
            let command = StreamCommand {
                signal_id: "signal-one".to_owned(),
                options: options.clone(),
                format: StreamOutputFormat::Ndjson,
                destination: Some(destination.clone()),
            };
            let signals_command = match kind {
                SubscriptionKind::Passive => {
                    SignalsCommand::Passive(PassiveSignalsCommand::Subscribe(command))
                }
                SubscriptionKind::Active => {
                    SignalsCommand::Active(ActiveSignalsCommand::Subscribe(command))
                }
            };
            run_signals(
                signals_command,
                Some("test-key".to_owned()),
                Some("http://unused.example.test".to_owned()),
                Some(ws_url),
                None,
                &store,
            )
            .await
            .expect("stream command");
            server.await.expect("WebSocket server");
        }

        let (ws_url, server) = serve_websocket_data_once().await;
        let mut filtered_options = options.clone();
        filtered_options.output = StreamOutputFilter::Status;
        filtered_options.no_reconnect = true;
        run_signals(
            SignalsCommand::Passive(PassiveSignalsCommand::Subscribe(StreamCommand {
                signal_id: "signal-one".to_owned(),
                options: filtered_options,
                format: StreamOutputFormat::Pretty,
                destination: None,
            })),
            Some("test-key".to_owned()),
            Some("http://unused.example.test".to_owned()),
            Some(ws_url),
            None,
            &store,
        )
        .await
        .expect("filtered stream command");
        server.await.expect("filtered WebSocket server");

        for kind in [SubscriptionKind::Passive, SubscriptionKind::Active] {
            let (ws_url, server) = serve_websocket_data_once().await;
            let command = TailCommand {
                signal_id: "signal-one".to_owned(),
                output: StreamOutputFilter::Data,
                window_semantics: WindowSemantics::Ordinal,
                backfill: true,
                backfill_from: None,
                backfill_to: None,
                backfill_count: Some(1),
                include_status: false,
                once: true,
                timeout: Some(Duration::from_secs(1)),
                no_reconnect: true,
                format: StreamOutputFormat::Pretty,
            };
            let signals_command = match kind {
                SubscriptionKind::Passive => {
                    SignalsCommand::Passive(PassiveSignalsCommand::Tail(command))
                }
                SubscriptionKind::Active => {
                    SignalsCommand::Active(ActiveSignalsCommand::Tail(command))
                }
            };
            run_signals(
                signals_command,
                Some("test-key".to_owned()),
                Some("http://unused.example.test".to_owned()),
                Some(ws_url),
                None,
                &store,
            )
            .await
            .expect("tail command");
            server.await.expect("WebSocket server");
        }

        let lines = fs::read_to_string(&destination).expect("stream destination");
        assert_eq!(lines.lines().count(), 2);
        fs::remove_dir_all(root).expect("remove stream command root");
    }

    #[tokio::test]
    async fn interactive_subscription_flow_edits_cached_selections_and_finishes_cleanly() {
        let root = temporary_root("interactive");
        fs::create_dir_all(&root).expect("create interactive root");
        let http_base_url = serve_json_responses(vec![
            serde_json::json!({
                "passiveSignals": [{
                    "passiveSignalId": "passive-one",
                    "label": "Revenue"
                }]
            })
            .to_string(),
            serde_json::json!({
                "activeSignals": [{
                    "activeSignalId": "active-one",
                    "passiveSignalId": "passive-one",
                    "label": "Revenue average"
                }]
            })
            .to_string(),
        ]);
        let client = DiscApiClient::new(http_base_url, "test-key").expect("API client");
        let mut prompts = DialoguerSubscriptionPrompts::scripted(vec![
            InteractiveAction::EditPassive,
            InteractiveAction::EditActive,
            InteractiveAction::EditActive,
            InteractiveAction::Finish,
        ]);
        let script = prompts.script.as_mut().expect("scripted prompts");
        script.passive_selection = HashSet::from(["passive-one".to_owned()]);
        script.passive_parents =
            VecDeque::from(["passive-one".to_owned(), "passive-one".to_owned()]);
        script.active_selection = HashSet::from(["active-one".to_owned()]);
        let command = InteractiveSubscribeCommand {
            options: stream_options(),
            format: StreamOutputFormat::Ndjson,
            destination: root.join("interactive.ndjson"),
        };

        run_interactive_subscribe_with(
            &client,
            "ws://127.0.0.1:1",
            "test-key",
            Some("client-one"),
            &command,
            &mut prompts,
        )
        .await
        .expect("interactive subscription");

        assert!(
            prompts
                .script
                .as_ref()
                .expect("scripted prompts")
                .did_wait_for_stop
        );
        fs::remove_dir_all(root).expect("remove interactive root");
    }

    #[tokio::test]
    async fn interactive_subscription_flow_handles_empty_catalogues_and_quit() {
        let root = temporary_root("interactive-empty");
        fs::create_dir_all(&root).expect("create interactive root");
        let empty_client = DiscApiClient::new(
            serve_json_responses(vec![r#"{"passiveSignals":[]}"#.to_owned()]),
            "test-key",
        )
        .expect("empty API client");
        let mut empty_prompts = DialoguerSubscriptionPrompts::scripted(vec![
            InteractiveAction::EditActive,
            InteractiveAction::Quit,
        ]);
        let empty_command = InteractiveSubscribeCommand {
            options: stream_options(),
            format: StreamOutputFormat::Pretty,
            destination: root.join("empty.ndjson"),
        };
        run_interactive_subscribe_with(
            &empty_client,
            "ws://127.0.0.1:1",
            "test-key",
            None,
            &empty_command,
            &mut empty_prompts,
        )
        .await
        .expect("empty interactive subscription");

        let no_active_client = DiscApiClient::new(
            serve_json_responses(vec![
                serde_json::json!({
                    "passiveSignals": [{
                        "passiveSignalId": "passive-one",
                        "label": "Revenue"
                    }]
                })
                .to_string(),
                r#"{"activeSignals":[]}"#.to_owned(),
            ]),
            "test-key",
        )
        .expect("API client without active signals");
        let mut no_active_prompts = DialoguerSubscriptionPrompts::scripted(vec![
            InteractiveAction::EditActive,
            InteractiveAction::Quit,
        ]);
        no_active_prompts
            .script
            .as_mut()
            .expect("scripted prompts")
            .passive_parents = VecDeque::from(["passive-one".to_owned()]);
        let no_active_command = InteractiveSubscribeCommand {
            options: stream_options(),
            format: StreamOutputFormat::Json,
            destination: root.join("no-active.ndjson"),
        };
        run_interactive_subscribe_with(
            &no_active_client,
            "ws://127.0.0.1:1",
            "test-key",
            None,
            &no_active_command,
            &mut no_active_prompts,
        )
        .await
        .expect("interactive subscription without active signals");

        fs::remove_dir_all(root).expect("remove interactive root");
    }

    #[test]
    fn tail_options_map_without_dropping_any_subscription_contract() {
        let command = TailCommand {
            signal_id: "signal-one".to_owned(),
            output: StreamOutputFilter::Status,
            window_semantics: WindowSemantics::Elapsed,
            backfill: true,
            backfill_from: Some(10),
            backfill_to: Some(20),
            backfill_count: Some(30),
            include_status: true,
            once: true,
            timeout: Some(Duration::from_secs(4)),
            no_reconnect: true,
            format: StreamOutputFormat::Json,
        };

        let options = stream_options_from_tail(&command);
        assert_eq!(options.output, StreamOutputFilter::Status);
        assert_eq!(options.window_semantics, WindowSemantics::Elapsed);
        assert!(options.backfill);
        assert_eq!(options.backfill_from, Some(10));
        assert_eq!(options.backfill_to, Some(20));
        assert_eq!(options.backfill_count, Some(30));
        assert!(options.include_status);
        assert!(options.once);
        assert_eq!(options.timeout, Some(Duration::from_secs(4)));
        assert!(options.no_reconnect);
    }

    #[tokio::test]
    async fn subscription_reconciliation_adds_preserves_and_removes_desired_tasks() {
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(io::sink())));
        let options = stream_options();
        let mut tasks = HashMap::new();
        let passive_ids = HashSet::from(["passive-one".to_owned()]);
        let active_ids = HashSet::from(["active-one".to_owned()]);

        reconcile_subscriptions(
            &mut tasks,
            ReconcileSubscriptionContext {
                writer: &writer,
                ws_url: "ws://127.0.0.1:1",
                api_key: "test-key",
                client_id: Some("client-one"),
                options: &options,
                format: StreamOutputFormat::Ndjson,
            },
            &passive_ids,
            &active_ids,
        );
        assert_eq!(tasks.len(), 2);
        assert!(tasks.contains_key(&SubscriptionSpec {
            kind: SubscriptionKind::Passive,
            signal_id: "passive-one".to_owned(),
        }));
        assert!(tasks.contains_key(&SubscriptionSpec {
            kind: SubscriptionKind::Active,
            signal_id: "active-one".to_owned(),
        }));

        reconcile_subscriptions(
            &mut tasks,
            ReconcileSubscriptionContext {
                writer: &writer,
                ws_url: "ws://127.0.0.1:1",
                api_key: "test-key",
                client_id: None,
                options: &options,
                format: StreamOutputFormat::Pretty,
            },
            &passive_ids,
            &HashSet::new(),
        );
        assert_eq!(tasks.len(), 1);

        abort_all_tasks(&mut tasks);
        assert!(tasks.is_empty());
    }

    #[test]
    fn subscription_summary_handles_empty_and_selected_signal_sets() {
        let passive_signals = vec![PassiveSignalSummary {
            passive_signal_id: "passive-one".to_owned(),
            label: "Revenue".to_owned(),
        }];
        let active_signals = vec![ActiveSignalSummary {
            active_signal_id: "active-one".to_owned(),
            passive_signal_id: "passive-one".to_owned(),
            label: "Revenue average".to_owned(),
        }];
        let cache = HashMap::from([("passive-one".to_owned(), active_signals)]);
        let destination = PathBuf::from("signals.ndjson");

        print_subscription_summary(
            &passive_signals,
            &cache,
            &HashSet::new(),
            &HashSet::new(),
            &destination,
        );
        print_subscription_summary(
            &passive_signals,
            &cache,
            &HashSet::from(["passive-one".to_owned()]),
            &HashSet::from(["active-one".to_owned()]),
            &destination,
        );
    }

    #[test]
    fn selection_models_build_labels_defaults_and_preserve_other_parent_selections() {
        let passive_signals = vec![
            PassiveSignalSummary {
                passive_signal_id: "passive-one".to_owned(),
                label: "Revenue".to_owned(),
            },
            PassiveSignalSummary {
                passive_signal_id: "passive-two".to_owned(),
                label: "Cost".to_owned(),
            },
        ];
        let (labels, defaults) =
            passive_selection_options(&passive_signals, &HashSet::from(["passive-two".to_owned()]));
        assert_eq!(labels, vec!["Revenue (passive-one)", "Cost (passive-two)"]);
        assert_eq!(defaults, vec![false, true]);
        assert_eq!(
            selected_passive_signal_ids(&passive_signals, vec![0]),
            HashSet::from(["passive-one".to_owned()])
        );

        let active_signals = vec![
            ActiveSignalSummary {
                active_signal_id: "active-one".to_owned(),
                passive_signal_id: "passive-one".to_owned(),
                label: "Revenue mean".to_owned(),
            },
            ActiveSignalSummary {
                active_signal_id: "active-two".to_owned(),
                passive_signal_id: "passive-one".to_owned(),
                label: "Revenue maximum".to_owned(),
            },
        ];
        let selected = HashSet::from(["active-two".to_owned(), "other-parent".to_owned()]);
        let (labels, defaults) = active_selection_options(&active_signals, &selected);
        assert_eq!(
            labels,
            vec!["Revenue mean (active-one)", "Revenue maximum (active-two)"]
        );
        assert_eq!(defaults, vec![false, true]);
        assert_eq!(
            merge_active_signal_selection(&active_signals, &selected, vec![0]),
            HashSet::from(["active-one".to_owned(), "other-parent".to_owned()])
        );
    }

    #[test]
    fn stored_config_fixture_remains_serializable_for_command_tests() {
        let value = StoredConfig::default();
        assert!(
            serde_json::to_string(&value)
                .expect("serialize config")
                .contains('{')
        );
    }
}
