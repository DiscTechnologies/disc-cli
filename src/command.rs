use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::path::Path;

use anyhow::{Context, Result};
use dialoguer::{MultiSelect, Select, theme::ColorfulTheme};
use tokio::task::JoinHandle;

use crate::cli::{
    ActiveSignalsCommand, ApiKeyCommand, AuthCommand, Cli, InteractiveSubscribeCommand,
    PassiveSignalsCommand, RootCommand, SignalsCommand, StreamCommand, StreamOptions, TailCommand,
};
use crate::config::{ConfigStore, StoredAuth};
use crate::http::{ActiveSignalSummary, DiscApiClient, PassiveSignalSummary};
use crate::output::{
    SharedWriter, create_file_writer, create_stdout_writer, print_json_value, print_signal_list,
    should_emit_event, validate_to_json, write_subscription_event,
};
use crate::ws::{SubscriptionKind, SubscriptionSpec, run_subscription};

pub async fn run(cli: Cli) -> Result<()> {
    let store = ConfigStore::discover()?;
    let api_key = cli.api_key.clone();
    let http_base_url = cli.http_base_url.clone();
    let ws_url = cli.ws_url.clone();
    let client_id = cli.client_id.clone();

    match cli.command {
        RootCommand::Auth(command) => {
            run_auth(command, api_key, http_base_url, ws_url, client_id, &store).await
        }
        RootCommand::Signals(command) => {
            run_signals(command, api_key, http_base_url, ws_url, client_id, &store).await
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
    let passive_signals = client.list_passive_signal_summaries().await?;
    let writer = create_file_writer(&command.destination)?;
    let theme = ColorfulTheme::default();
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

        let action = Select::with_theme(&theme)
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
            0 => {
                let next_selected = prompt_passive_signal_selection(
                    &theme,
                    &passive_signals,
                    &selected_passive_ids,
                )?;
                selected_passive_ids = next_selected;
            }
            1 => {
                if passive_signals.is_empty() {
                    println!("No passive signals available.");
                } else {
                    let passive_signal = prompt_passive_parent(&theme, &passive_signals)?;
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
                        selected_active_ids = prompt_active_signal_selection(
                            &theme,
                            &active_signals,
                            &selected_active_ids,
                        )?;
                    }
                }
            }
            2 => {
                reconcile_subscriptions(
                    &mut tasks,
                    &writer,
                    ws_url,
                    api_key,
                    client_id,
                    &command.options,
                    command.format,
                    &selected_passive_ids,
                    &selected_active_ids,
                );
                println!(
                    "Subscriptions are running. Output is being appended to {}. Press Ctrl+C to stop the CLI.",
                    command.destination.display()
                );
                tokio::signal::ctrl_c()
                    .await
                    .context("Failed to wait for Ctrl+C.")?;
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
            &writer,
            ws_url,
            api_key,
            client_id,
            &command.options,
            command.format,
            &selected_passive_ids,
            &selected_active_ids,
        );
    }
}

fn reconcile_subscriptions(
    tasks: &mut HashMap<SubscriptionSpec, JoinHandle<()>>,
    writer: &SharedWriter,
    ws_url: &str,
    api_key: &str,
    client_id: Option<&str>,
    options: &crate::cli::StreamOptions,
    format: crate::cli::StreamOutputFormat,
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
        if desired_specs.contains(&spec) == false {
            if let Some(task) = tasks.remove(&spec) {
                task.abort();
            }
        }
    }

    for spec in desired_specs {
        if tasks.contains_key(&spec) {
            continue;
        }

        let writer = writer.clone();
        let ws_url = ws_url.to_owned();
        let api_key = api_key.to_owned();
        let client_id = client_id.map(str::to_owned);
        let options = options.clone();
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

    let next_selected = selection
        .into_iter()
        .map(|index| passive_signals[index].passive_signal_id.clone())
        .collect::<HashSet<_>>();

    Ok(next_selected)
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

    let mut next_selected = selected_active_ids.clone();
    next_selected.retain(|signal_id| {
        active_signals
            .iter()
            .all(|signal| signal.active_signal_id != *signal_id)
    });
    for index in selection {
        next_selected.insert(active_signals[index].active_signal_id.clone());
    }

    Ok(next_selected)
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
