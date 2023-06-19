use poise::serenity_prelude as serenity;
use shuttle_poise::ShuttlePoise;
use shuttle_secrets::SecretStore;

pub mod crates;
pub mod godbolt;
pub mod misc;
pub mod moderation;
pub mod playground;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

// const EMBED_COLOR: (u8, u8, u8) = (0xf7, 0x4c, 0x00);
const EMBED_COLOR: (u8, u8, u8) = (0xb7, 0x47, 0x00); // slightly less saturated

/// Used for playground stdout + stderr, or godbolt asm + stderr
/// If the return value is empty, returns " " instead, because Discord displays those better in
/// a code block than "".
fn merge_output_and_errors<'a>(output: &'a str, errors: &'a str) -> std::borrow::Cow<'a, str> {
    match (output.trim(), errors.trim()) {
        ("", "") => " ".into(),
        (output, "") => output.into(),
        ("", errors) => errors.into(),
        (output, errors) => format!("{errors}\n\n{output}").into(),
    }
}

/// In prefix commands, react with a red cross emoji. In slash commands, respond with a short
/// explanation.
async fn acknowledge_fail(error: poise::FrameworkError<'_, Data, Error>) {
    if let poise::FrameworkError::Command { error, ctx } = error {
        log::warn!("Reacting with red cross because of error: {}", error);

        match ctx {
            poise::Context::Prefix(ctx) => {
                if let Err(e) = ctx
                    .msg
                    .react(ctx, serenity::ReactionType::from('❌'))
                    .await
                {
                    log::warn!("Failed to react with red cross: {}", e);
                }
            }
            poise::Context::Application(_) => {
                if let Err(e) = ctx.say(format!("❌ {}", error)).await {
                    log::warn!(
                        "Failed to send failure acknowledgment slash command response: {}",
                        e
                    );
                }
            }
        }
    } else {
        on_error(error).await;
    }
}

async fn on_error(error: poise::FrameworkError<'_, Data, Error>) {
    log::warn!("Encountered error: {:?}", error);
    if let poise::FrameworkError::ArgumentParse { error, ctx, .. } = error {
        let response = if error.is::<poise::CodeBlockError>() {
            "\
Missing code block. Please use the following markdown:
\\`code here\\`
or
\\`\\`\\`rust
code here
\\`\\`\\`"
                .to_owned()
        } else if let Some(multiline_help) = ctx.command().help_text {
            format!("**{}**\n{}", error, multiline_help())
        } else {
            error.to_string()
        };

        if let Err(e) = ctx.say(response).await {
            log::warn!("{}", e)
        }
    } else if let poise::FrameworkError::Command { ctx, error } = error {
        if let Err(e) = ctx.say(error.to_string()).await {
            log::warn!("{}", e)
        }
    }
}

async fn event_handler(ctx: &serenity::Context, event: &poise::Event, data: &Data) -> Result<(), Error> {
    match event {
        poise::Event::MessageUpdate { event, .. } => {
            showcase::try_update_showcase_message(ctx, data, event.id).await?
        }
        poise::Event::MessageDelete {
            deleted_message_id, ..
        } => showcase::try_delete_showcase_message(ctx, data, *deleted_message_id).await?,
        poise::Event::GuildMemberAddition { new_member } => {
            const RUSTIFICATION_DELAY: u64 = 30; // in minutes

            tokio::time::sleep(std::time::Duration::from_secs(RUSTIFICATION_DELAY * 60)).await;

            // Ignore errors because the user may have left already
            let _: Result<_, _> = ctx
                .http
                .add_member_role(
                    new_member.guild_id.0,
                    new_member.user.id.0,
                    data.rustacean_role.0,
                    Some(&format!(
                        "Automatically rustified after {} minutes",
                        RUSTIFICATION_DELAY
                    )),
                )
                .await;
        }
        _ => {}
    }

    Ok(())
}


#[derive(Debug)]
pub struct Data {
    bot_user_id: serenity::UserId,
    mod_role_id: serenity::RoleId,
    rustacean_role: serenity::RoleId,
    beginner_channel: serenity::ChannelId,
    bot_start_time: std::time::Instant,
    http: reqwest::Client,
    godbolt_metadata: std::sync::Mutex<godbolt::GodboltMetadata>,
}

fn env_var<T: std::str::FromStr>(name: &str) -> Result<T, Error>
    where
        T::Err: std::fmt::Display,
{
    Ok(std::env::var(name)
        .map_err(|_| format!("Missing {}", name))?
        .parse()
        .map_err(|e| format!("Invalid {}: {}", name, e))?)
}

async fn find_custom_emoji(ctx: Context<'_>, emoji_name: &str) -> Option<serenity::Emoji> {
    ctx.guild_id()?
        .to_guild_cached(ctx.discord())?
        .emojis
        .values()
        .find(|emoji| emoji.name.eq_ignore_ascii_case(emoji_name))
        .cloned()
}

async fn custom_emoji_code(ctx: Context<'_>, emoji_name: &str, fallback: char) -> String {
    match find_custom_emoji(ctx, emoji_name).await {
        Some(emoji) => emoji.to_string(),
        None => fallback.to_string(),
    }
}

/// In prefix commands, react with a custom emoji from the guild, or fallback to a default Unicode
/// emoji.
///
/// In slash commands, currently nothing happens.
async fn acknowledge_success(
    ctx: Context<'_>,
    emoji_name: &str,
    fallback: char,
) -> Result<(), Error> {
    let emoji = find_custom_emoji(ctx, emoji_name).await;
    match ctx {
        Context::Prefix(ctx) => {
            let reaction = emoji
                .map(serenity::ReactionType::from)
                .unwrap_or_else(|| serenity::ReactionType::from(fallback));

            ctx.msg.react(ctx.discord, reaction).await?;
        }
        Context::Application(_) => {
            let msg_content = match emoji {
                Some(e) => e.to_string(),
                None => fallback.to_string(),
            };
            if let Ok(reply) = ctx.say(msg_content).await {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let msg = reply.message().await?;
                // ignore errors as to not fail if ephemeral
                let _: Result<_, _> = msg.delete(ctx.discord()).await;
            }
        }
    }
    Ok(())
}

/// Truncates the message with a given truncation message if the
/// text is too long. "Too long" means, it either goes beyond Discord's 2000 char message limit,
/// or if the text_body has too many lines.
///
/// Only `text_body` is truncated. `text_end` will always be appended at the end. This is useful
/// for example for large code blocks. You will want to truncate the code block contents, but the
/// finalizing triple backticks (` ` `) should always stay - that's what `text_end` is for.
async fn trim_text(
    mut text_body: &str,
    text_end: &str,
    truncation_msg_future: impl std::future::Future<Output=String>,
) -> String {
    const MAX_OUTPUT_LINES: usize = 45;

    // Err with the future inside if no truncation occurs
    let mut truncation_msg_maybe = Err(truncation_msg_future);

    // check Discord's 2000 char message limit first
    if text_body.len() + text_end.len() > 2000 {
        let truncation_msg = match truncation_msg_maybe {
            Ok(msg) => msg,
            Err(future) => future.await,
        };

        // This is how long the text body may be at max to conform to Discord's limit
        let available_space = 2000_usize
            .saturating_sub(text_end.len())
            .saturating_sub(truncation_msg.len());

        let mut cut_off_point = available_space;
        while !text_body.is_char_boundary(cut_off_point) {
            cut_off_point -= 1;
        }

        text_body = &text_body[..cut_off_point];
        truncation_msg_maybe = Ok(truncation_msg);
    }

    // check number of lines
    let text_body = if text_body.lines().count() > MAX_OUTPUT_LINES {
        truncation_msg_maybe = Ok(match truncation_msg_maybe {
            Ok(msg) => msg,
            Err(future) => future.await,
        });

        text_body
            .lines()
            .take(MAX_OUTPUT_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text_body.to_owned()
    };

    if let Ok(truncation_msg) = truncation_msg_maybe {
        format!("{}{}{}", text_body, text_end, truncation_msg)
    } else {
        format!("{}{}", text_body, text_end)
    }
}

async fn reply_potentially_long_text(
    ctx: Context<'_>,
    text_body: &str,
    text_end: &str,
    truncation_msg_future: impl std::future::Future<Output=String>,
) -> Result<(), Error> {
    ctx.say(trim_text(text_body, text_end, truncation_msg_future).await)
        .await?;
    Ok(())
}

#[shuttle_runtime::main]
async fn poise(#[shuttle_secrets::Secrets] secret_store: SecretStore) -> ShuttlePoise<Data, Error> {
    env_logger::init();

    let data = Data::new(&secret_store);

    let discord_token = env_var::<String>("DISCORD_TOKEN")?;
    let mod_role_id = env_var("MOD_ROLE_ID")?;
    let rustacean_role = env_var("RUSTACEAN_ROLE_ID")?;
    let reports_channel = env_var("REPORTS_CHANNEL_ID").ok();
    let showcase_channel = env_var("SHOWCASE_CHANNEL_ID")?;
    let beginner_channel = env_var("BEGINNER_CHANNEL_ID")?;
    let database_url = env_var::<String>("DATABASE_URL")?;
    let custom_prefixes = env_var("CUSTOM_PREFIXES")?;

    let framework = poise::Framework::builder()
        .token(secret_store.get("DISCORD_TOKEN").unwrap())
        .setup(move |ctx, ready, f| {
            Box::pin(async move {
                poise::builtins::register_in_guild(ctx, &f.options().commands, serenity::GuildId(data.discord_guild)).await?;
                ctx.set_activity(serenity::ActivityData::listening("/help"));
                Ok(Data {
                    bot_user_id: bot.user.id,
                    mod_role_id,
                    rustacean_role,
                    beginner_channel,
                    bot_start_time: std::time::Instant::now(),
                    http: reqwest::Client::new(),
                    godbolt_metadata: std::sync::Mutex::new(godbolt::GodboltMetadata::default()),
                })
            })
        })
        .options(poise::FrameworkOptions {
            commands: vec![
                playground::play(),
                playground::playwarn(),
                playground::eval(),
                playground::miri(),
                playground::expand(),
                playground::clippy(),
                playground::fmt(),
                playground::microbench(),
                playground::procmacro(),
                godbolt::godbolt(),
                godbolt::mca(),
                godbolt::llvmir(),
                godbolt::targets(),
                crates::crate_(),
                crates::doc(),
                moderation::cleanup(),
                moderation::ban(),
                moderation::move_(),
                misc::go(),
                misc::source(),
                misc::help(),
                misc::register(),
                misc::uptime(),
                misc::conradluget(),
            ],
            prefix_options: poise::PrefixFrameworkOptions {
                prefix: Some("?".into()),
                additional_prefixes: vec![
                    poise::Prefix::Literal("🦀 "),
                    poise::Prefix::Literal("🦀"),
                    poise::Prefix::Literal("<:ferris:358652670585733120> "),
                    poise::Prefix::Literal("<:ferris:358652670585733120>"),
                    poise::Prefix::Regex(
                        "(yo|hey) (crab|ferris|fewwis),? can you (please |pwease )?"
                            .parse()
                            .unwrap(),
                    ),
                ],
                edit_tracker: Some(poise::EditTracker::for_timespan(
                    std::time::Duration::from_days(2),
                )),
                ..Default::default()
            },
            /// The global error handler for all error cases that may occur
            on_error: |error| Box::pin(on_error(error)),
            /// This code is run before every command
            pre_command: |ctx| {
                Box::pin(async move {
                    let channel_name = ctx
                        .channel_id()
                        .name(&ctx.discord())
                        .await
                        .unwrap_or_else(|_| "<unknown>".to_owned());
                    let author = ctx.author().tag();

                    match ctx {
                        poise::Context::Prefix(ctx) => {
                            log::info!("{} in {}: {}", author, channel_name, &ctx.msg.content);
                        }
                        poise::Context::Application(ctx) => {
                            let command_name = &ctx.interaction.data().name;

                            log::info!(
                            "{} in {} used slash command '{}'",
                            author,
                            channel_name,
                            command_name
                        );
                        }
                    }
                })
            },
            /// This code is run after a command if it was successful (returned Ok)
            post_command: |ctx| {
                Box::pin(async move {
                    println!("Executed command {}!", ctx.command().qualified_name);
                })
            },
            /// Every command invocation must pass this check to continue execution
            command_check: Some(|_ctx| {
                Box::pin(async move {
                    Ok(true)
                })
            }),
            /// Enforce command checks even for owners (enforced by default)
            /// Set to true to bypass checks, which is useful for testing
            skip_checks_for_owners: false,
            event_handler: |ctx, event, _framework, data| {
                Box::pin(async move {
                    event_handler(ctx, event, data)
                })
            },
            ..Default::default()
        })
        .intents(
            serenity::GatewayIntents::all(),
        )
        .build()
        .await
        .map_err(shuttle_runtime::CustomError::new)?;


    if custom_prefixes {
        options.commands.push(poise::Command {
            subcommands: vec![
                prefixes::prefix_add(),
                prefixes::prefix_remove(),
                prefixes::prefix_list(),
                prefixes::prefix_reset(),
            ],
            ..prefixes::prefix()
        });
    }

    // Use different implementations for rustify because of different feature sets
    let application_rustify = a::application_rustify();
    options.commands.push(poise::Command {
        context_menu_action: application_rustify.context_menu_action,
        slash_action: application_rustify.slash_action,
        context_menu_name: application_rustify.context_menu_name,
        parameters: application_rustify.parameters,
        ..a::rustify()
    });

    if reports_channel.is_some() {
        options.commands.push(a::report());
    }

    Ok(framework.into())
}
