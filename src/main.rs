extern crate backoff;
extern crate discord;
#[macro_use]
extern crate failure;
extern crate futures;
extern crate irc;
#[macro_use]
extern crate lazy_static;
extern crate openssl_probe;
extern crate regex;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate slack_api;
extern crate spmc;
extern crate termion;
extern crate textwrap;
extern crate toml;
extern crate websocket;
#[macro_use]
extern crate derive_more;
extern crate chrono;

#[macro_use]
mod conn;
mod tui;
use tui::TUI;
mod bimap;
mod discord_conn;
mod slack_conn;

#[derive(Debug, Deserialize, Clone)]
struct SlackConfig {
    token: String,
}

#[derive(Debug, Deserialize, Clone)]
struct DiscordConfig {
    name: String,
}

#[derive(Debug, Deserialize)]
struct Config {
    discord_token: Option<String>,
    slack: Option<Vec<SlackConfig>>,
    discord: Option<Vec<DiscordConfig>>,
}

fn main() {
    use conn::Event;
    use discord_conn::DiscordConn;
    use slack_conn::SlackConn;
    use std::fs::File;
    use std::io::Read;
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};
    use std::thread;

    openssl_probe::init_ssl_cert_env_vars();

    let homedir = std::env::var("HOME").unwrap_or_else(|_| {
        println!("You don't even have a $HOME? :'(");
        std::process::exit(1)
    });
    let config_path = PathBuf::from(homedir).join(".omnichat.toml");
    let mut contents = String::new();
    File::open(&config_path)
        .unwrap_or_else(|_| {
            println!(
                "No config file found, expected a config file at {:?}",
                config_path
            );
            std::process::exit(1)
        })
        .read_to_string(&mut contents)
        .unwrap_or_else(|_| {
            println!("Unable to read config file at {:?}", &config_path);
            std::process::exit(1)
        });

    let config: Config = toml::from_str(&contents).unwrap_or_else(|_| {
        println!("{:?} is not a valid omnichat config file", &config_path);
        std::process::exit(1)
    });

    let tui = TUI::new();

    // Start all the slack connections first, because we can't do the Discord stuff fully async
    if let Some(slack) = config.slack {
        for c in slack {
            let sender = tui.sender();
            thread::spawn(move || match SlackConn::new(c.token, sender.clone()) {
                Ok(connection) => sender.send(Event::Connected(connection)).unwrap(),
                Err(err) => sender.send(omnierror!(err)).unwrap(),
            });
        }
    }

    // Discord only permits one connection per user, so we need to redistribute the incoming events
    if let (&Some(ref discord_token), &Some(ref discord)) = (&config.discord_token, &config.discord)
    {
        // This operation is blocking, but is the only I/O required to hook up to Discord, and we
        // only need to do this operation once per token, and we only permit one token so far so it
        // doesn't matter

        let sender = tui.sender();
        let discord_token = discord_token.clone();
        let discord = discord.clone();
        thread::spawn(move || {
            use backoff::{Error, ExponentialBackoff, Operation};
            let mut op =
                || discord::Discord::from_user_token(&discord_token).map_err(Error::Transient);
            let mut backoff = ExponentialBackoff::default();
            let dis = op.retry(&mut backoff).unwrap_or_else(|e| {
                println!("Unable to connect to Discord:\n{:#?}", e);
                std::process::exit(1);
            });

            let (mut connection, info) = {
                let mut op = || dis.connect().map_err(Error::Transient);
                let mut backoff = ExponentialBackoff::default();
                op.retry(&mut backoff).unwrap_or_else(|e| {
                    println!("Unable to connect to Discord:\n{:#?}", e);
                    std::process::exit(1);
                })
            };

            let dis = Arc::new(RwLock::new(dis));

            let (discord_sender, discord_receiver) = spmc::channel();

            // Spawn a thread that copies the incoming Discord events out to every omnichat server
            let error_sender = sender.clone();
            thread::spawn(move || loop {
                match connection.recv_event() {
                    Ok(ev) => discord_sender.send(ev).unwrap(),
                    Err(discord::Error::Closed(..)) => break,
                    Err(err) => error_sender.send(omnierror!(err)).unwrap(),
                }
            });

            for c in discord.iter().cloned() {
                let sender = sender.clone();
                let info = info.clone();
                let dis = dis.clone();
                let discord_receiver = discord_receiver.clone();
                thread::spawn(move || {
                    match DiscordConn::new(dis, info, discord_receiver, &c.name, sender.clone()) {
                        Ok(connection) => sender.send(Event::Connected(connection)).unwrap(),
                        Err(err) => sender.send(omnierror!(err)).unwrap(),
                    }
                });
            }
        });
    }

    tui.run();
}
