//! `pocketskynet-client` — a CLI for the PocketSkynet server over HTTP/1.1
//! or HTTP/3.
//!
//! ```text
//! pocketskynet-client --server https://127.0.0.1:9099 --insecure --key 0x… rooms
//! pocketskynet-client --http3 --insecure login
//! ```

use clap::{Parser, Subcommand};
use pocketskynet_client::{Client, Transport, TransportOptions, Wallet};

#[derive(Parser)]
#[command(
    name = "pocketskynet-client",
    about = "PocketSkynet messenger client (HTTP/1.1 and HTTP/3)",
    version
)]
struct Cli {
    /// Server base URL.
    #[arg(long, global = true, default_value = "http://127.0.0.1:9099")]
    server: String,

    /// Use HTTP/3 over QUIC (ALPN `h3`) instead of HTTP/1.1. QUIC mandates
    /// TLS, so pair it with --insecure against a self-signed dev server.
    #[arg(long, global = true)]
    http3: bool,

    /// UDP port for HTTP/3 when it differs from the URL's port. The server's
    /// `make start` serves QUIC on the same number as HTTPS, so this is
    /// rarely needed.
    #[arg(long, global = true)]
    http3_port: Option<u16>,

    /// Skip TLS certificate verification (self-signed dev certs only).
    #[arg(long, global = true)]
    insecure: bool,

    /// secp256k1 private key, 0x-prefixed hex.
    #[arg(long, global = true, env = "POCKETSKYNET_KEY", hide_env_values = true)]
    key: Option<String>,

    /// A JWT from an earlier `login`, reused instead of signing in again.
    /// Prefer this for scripted use: each `login` consumes a challenge and
    /// the production server caps logins at 5/min/IP, so re-logging in on
    /// every command trips a 429.
    #[arg(
        long,
        global = true,
        env = "POCKETSKYNET_TOKEN",
        hide_env_values = true
    )]
    token: Option<String>,

    /// Username for first-time login (defaults to the protocol's
    /// deterministic username for the address).
    #[arg(long, global = true)]
    username: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Log in (challenge → sign → JWT) and print the JWT and address.
    Login,
    /// List the rooms the wallet is a member of.
    Rooms,
    /// Create a channel.
    CreateRoom { name: String },
    /// Send a plaintext message to a room.
    Send { room_id: String, text: String },
    /// Show the most recent messages in a room.
    Messages {
        room_id: String,
        /// How many messages to fetch (1–100).
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Ping the unauthenticated health endpoint.
    Health,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let options = TransportOptions {
        insecure: cli.insecure,
        ..TransportOptions::default()
    };
    let transport = if cli.http3 {
        Transport::http3(&cli.server, &options, cli.http3_port).await?
    } else {
        Transport::http1(&cli.server, &options)?
    };
    let mut client = Client::new(transport);

    // Health needs no wallet; everything else signs in first.
    if let Command::Health = cli.command {
        let health = client.health().await?;
        println!("status:    {}", health.status);
        if let Some(uptime) = health.uptime {
            println!("uptime:    {uptime}s");
        }
        println!("transport: {}", client.transport_name());
        return Ok(());
    }

    // `login` always signs in with the key (its whole job is to mint a JWT).
    // Every other authenticated command reuses a supplied token when there is
    // one — the scripted path that avoids the 5/min login limiter — and falls
    // back to signing in with the key otherwise.
    let signed_in = if let Command::Login = cli.command {
        Some(sign_in(&mut client, cli.key.as_deref(), cli.username.as_deref()).await?)
    } else if let Some(token) = cli.token.as_deref() {
        client.set_token(token.to_owned());
        None
    } else {
        Some(sign_in(&mut client, cli.key.as_deref(), cli.username.as_deref()).await?)
    };

    match cli.command {
        Command::Health => unreachable!("handled above"),
        Command::Login => {
            let login = signed_in.expect("login always signs in");
            println!("address:   {}", login.user.wallet_address);
            println!("username:  {}", login.user.username);
            println!("transport: {}", client.transport_name());
            println!("jwt:       {}", login.token);
        }
        Command::Rooms => {
            let rooms = client.rooms().await?;
            if rooms.is_empty() {
                println!("(no rooms)");
            }
            for room in rooms {
                println!(
                    "{}  {}  members={} unread={}",
                    room.id,
                    room.name.as_deref().unwrap_or("(unnamed)"),
                    room.member_count.unwrap_or(0),
                    room.unread_count.unwrap_or(0),
                );
            }
        }
        Command::CreateRoom { name } => {
            let room = client.create_room(&name, None).await?;
            println!(
                "created {}  {}",
                room.id,
                room.name.as_deref().unwrap_or("")
            );
        }
        Command::Send { room_id, text } => {
            let message = client.send_message(&room_id, &text).await?;
            println!(
                "sent {} at {}",
                message.id,
                message.message_timestamp.unwrap_or(0)
            );
        }
        Command::Messages { room_id, limit } => {
            let messages = client.messages(&room_id, limit.clamp(1, 100)).await?;
            if messages.is_empty() {
                println!("(no messages)");
            }
            for message in messages {
                let who = message
                    .sender
                    .as_ref()
                    .map(|s| s.username.clone())
                    .unwrap_or_else(|| message.sender_address.clone());
                let body = if message.is_encrypted.unwrap_or(false) {
                    "(encrypted)".to_owned()
                } else {
                    message.content.clone()
                };
                println!("[{}] {who}: {body}", message.message_timestamp.unwrap_or(0));
            }
        }
    }

    Ok(())
}

/// Sign in with the private key: challenge → EIP-191 sign → JWT, stored on
/// `client`. Fails with a clear message when no key was supplied.
async fn sign_in(
    client: &mut Client,
    key: Option<&str>,
    username: Option<&str>,
) -> Result<pocketskynet_client::types::LoginResponse, Box<dyn std::error::Error>> {
    let key = key.ok_or(
        "a private key is required: pass --key 0x… or set POCKETSKYNET_KEY \
         (or reuse a JWT with --token / POCKETSKYNET_TOKEN)",
    )?;
    let wallet =
        Wallet::from_private_key_hex(key).map_err(|e| format!("invalid private key: {e:?}"))?;
    Ok(client.login(&wallet, username).await?)
}
