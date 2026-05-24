use std::future::Future;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_native_tls::TlsConnector;

use crate::adapter_types::{IrcAdapterConfig, IrcTriggerPolicy};

const IRC_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(20);
const IRC_JOIN_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrcPrivateMessage {
    pub nick: String,
    pub target: String,
    pub text: String,
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrcLine {
    Ping { token: String },
    Privmsg(IrcPrivateMessage),
    Other(String),
}

pub async fn run_irc_adapter_once_with_connected<F, Fut>(
    config: &IrcAdapterConfig,
    password: Option<&str>,
    on_connected: F,
) -> Result<IrcPrivateMessage>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let (mut reader, mut writer) = open_registered_connection(config, password).await?;
    join_channel(reader.as_mut(), writer.as_mut(), config).await?;
    on_connected().await?;
    loop {
        let raw = read_irc_line(reader.as_mut()).await?;
        match parse_irc_line(&raw) {
            IrcLine::Ping { token } => {
                write_irc_command(writer.as_mut(), &format!("PONG :{token}")).await?;
            }
            IrcLine::Privmsg(message) => {
                if message.target == config.channel
                    && should_trigger(config, &message.nick, &message.text)
                {
                    return Ok(message);
                }
            }
            IrcLine::Other(_) => {}
        }
    }
}

pub async fn run_irc_adapter_loop_with_connected<F, Fut, G, MsgFut, H, OutFut>(
    config: &IrcAdapterConfig,
    password: Option<&str>,
    on_connected: F,
    mut on_message: G,
    mut take_outbound_messages: H,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<()>>,
    G: FnMut(IrcPrivateMessage) -> MsgFut,
    MsgFut: Future<Output = Result<()>>,
    H: FnMut() -> OutFut,
    OutFut: Future<Output = Result<Vec<String>>>,
{
    let (mut reader, mut writer) = open_registered_connection(config, password).await?;
    join_channel(reader.as_mut(), writer.as_mut(), config).await?;
    on_connected().await?;
    loop {
        send_pending_messages(writer.as_mut(), config, &mut take_outbound_messages).await?;
        let raw = match timeout(Duration::from_secs(1), read_irc_line(reader.as_mut())).await {
            Ok(raw) => raw?,
            Err(_) => continue,
        };
        match parse_irc_line(&raw) {
            IrcLine::Ping { token } => {
                write_irc_command(writer.as_mut(), &format!("PONG :{token}")).await?;
            }
            IrcLine::Privmsg(message) => {
                if message.target == config.channel
                    && should_trigger(config, &message.nick, &message.text)
                {
                    on_message(message).await?;
                }
            }
            IrcLine::Other(_) => {}
        }
    }
}

async fn send_pending_messages<H, OutFut>(
    writer: &mut (dyn AsyncWrite + Unpin + Send),
    config: &IrcAdapterConfig,
    take_outbound_messages: &mut H,
) -> Result<()>
where
    H: FnMut() -> OutFut,
    OutFut: Future<Output = Result<Vec<String>>>,
{
    for text in take_outbound_messages().await? {
        write_irc_command(writer, &format!("PRIVMSG {} :{text}", config.channel)).await?;
    }
    Ok(())
}

pub fn parse_irc_line(raw: &str) -> IrcLine {
    if let Some(token) = raw.strip_prefix("PING :") {
        return IrcLine::Ping {
            token: token.to_string(),
        };
    }
    let Some(rest) = raw.strip_prefix(':') else {
        return IrcLine::Other(raw.to_string());
    };
    let Some((prefix, rest)) = rest.split_once(' ') else {
        return IrcLine::Other(raw.to_string());
    };
    let nick = prefix.split('!').next().unwrap_or(prefix).to_string();
    let Some(rest) = rest.strip_prefix("PRIVMSG ") else {
        return IrcLine::Other(raw.to_string());
    };
    let Some((target, text)) = rest.split_once(" :") else {
        return IrcLine::Other(raw.to_string());
    };
    IrcLine::Privmsg(IrcPrivateMessage {
        nick,
        target: target.to_string(),
        text: text.to_string(),
        raw: raw.to_string(),
    })
}

pub fn should_trigger(config: &IrcAdapterConfig, sender: &str, text: &str) -> bool {
    if sender.eq_ignore_ascii_case(&config.nick) {
        return false;
    }
    match config.trigger {
        IrcTriggerPolicy::AllMessages => true,
        IrcTriggerPolicy::Mention => text
            .to_ascii_lowercase()
            .contains(&config.nick.to_ascii_lowercase()),
    }
}

async fn open_registered_connection(
    config: &IrcAdapterConfig,
    password: Option<&str>,
) -> Result<(
    Box<dyn AsyncBufRead + Unpin + Send>,
    Box<dyn AsyncWrite + Unpin + Send>,
)> {
    if config.tls {
        let stream = TcpStream::connect((config.server.as_str(), config.port)).await?;
        let connector = tls_connector()?;
        let stream = connector.connect(&config.server, stream).await?;
        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);
        register_irc(&mut reader, &mut writer, config, password).await?;
        Ok((Box::new(reader), Box::new(writer)))
    } else {
        let stream = TcpStream::connect((config.server.as_str(), config.port)).await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        register_irc(&mut reader, &mut writer, config, password).await?;
        Ok((Box::new(reader), Box::new(writer)))
    }
}

fn tls_connector() -> Result<TlsConnector> {
    Ok(TlsConnector::from(
        native_tls::TlsConnector::builder().build()?,
    ))
}

async fn register_irc(
    reader: &mut (dyn AsyncBufRead + Unpin + Send),
    writer: &mut (dyn AsyncWrite + Unpin + Send),
    config: &IrcAdapterConfig,
    password: Option<&str>,
) -> Result<()> {
    if let Some(password) = password {
        write_irc_command(writer, &format!("PASS {password}")).await?;
    }
    write_irc_command(writer, &format!("NICK {}", config.nick)).await?;
    write_irc_command(
        writer,
        &format!("USER {} 0 * :{}", config.username, config.realname),
    )
    .await?;
    wait_for_line(
        reader,
        writer,
        IRC_REGISTRATION_TIMEOUT,
        |line| line.contains(&format!(" 001 {} ", config.nick)),
        "IRC registration welcome",
    )
    .await?;
    Ok(())
}

async fn join_channel(
    reader: &mut (dyn AsyncBufRead + Unpin + Send),
    writer: &mut (dyn AsyncWrite + Unpin + Send),
    config: &IrcAdapterConfig,
) -> Result<()> {
    write_irc_command(writer, &format!("JOIN {}", config.channel)).await?;
    wait_for_line(
        reader,
        writer,
        IRC_JOIN_TIMEOUT,
        |line| {
            line.contains(&format!(" JOIN {}", config.channel))
                || line.contains(&format!(" JOIN :{}", config.channel))
                || line.contains(&format!(" 366 {} {} ", config.nick, config.channel))
        },
        "IRC JOIN confirmation",
    )
    .await?;
    Ok(())
}

async fn wait_for_line(
    reader: &mut (dyn AsyncBufRead + Unpin + Send),
    writer: &mut (dyn AsyncWrite + Unpin + Send),
    timeout: Duration,
    predicate: impl Fn(&str) -> bool,
    description: &str,
) -> Result<String> {
    loop {
        let line = tokio::time::timeout(timeout, read_irc_line(reader))
            .await
            .map_err(|_| anyhow!("timed out waiting for {description}"))??;
        if let IrcLine::Ping { token } = parse_irc_line(&line) {
            write_irc_command(writer, &format!("PONG :{token}")).await?;
        }
        if is_irc_error_numeric(&line) {
            bail!("IRC server returned an error while waiting for {description}: {line}");
        }
        if predicate(&line) {
            return Ok(line);
        }
    }
}

fn is_irc_error_numeric(line: &str) -> bool {
    let Some(rest) = line.strip_prefix(':') else {
        return false;
    };
    let Some((_prefix, rest)) = rest.split_once(' ') else {
        return false;
    };
    let Some(code) = rest.split_whitespace().next() else {
        return false;
    };
    code.len() == 3 && (code.starts_with('4') || code.starts_with('5'))
}

async fn read_irc_line(reader: &mut (dyn AsyncBufRead + Unpin + Send)) -> Result<String> {
    let mut line = String::new();
    let length = reader.read_line(&mut line).await?;
    if length == 0 {
        bail!("IRC connection closed");
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

async fn write_irc_command(
    writer: &mut (dyn AsyncWrite + Unpin + Send),
    command: &str,
) -> Result<()> {
    writer.write_all(command.as_bytes()).await?;
    writer.write_all(b"\r\n").await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(trigger: IrcTriggerPolicy) -> IrcAdapterConfig {
        IrcAdapterConfig {
            server: "irc.example.com".to_string(),
            port: 6667,
            tls: false,
            nick: "exo-bot".to_string(),
            username: "exo".to_string(),
            realname: "Exo".to_string(),
            channel: "#exo".to_string(),
            password_secret_id: None,
            trigger,
        }
    }

    #[test]
    fn parses_ping_and_privmsg() {
        assert_eq!(
            parse_irc_line("PING :abc"),
            IrcLine::Ping {
                token: "abc".to_string()
            }
        );
        assert_eq!(
            parse_irc_line(":alice!u@h PRIVMSG #exo :hello exo-bot"),
            IrcLine::Privmsg(IrcPrivateMessage {
                nick: "alice".to_string(),
                target: "#exo".to_string(),
                text: "hello exo-bot".to_string(),
                raw: ":alice!u@h PRIVMSG #exo :hello exo-bot".to_string(),
            })
        );
    }

    #[test]
    fn mention_trigger_ignores_unmentioned_and_self_messages() {
        let mention_config = config(IrcTriggerPolicy::Mention);
        assert!(should_trigger(&mention_config, "alice", "hello exo-bot"));
        assert!(!should_trigger(&mention_config, "alice", "hello"));
        assert!(!should_trigger(&mention_config, "exo-bot", "exo-bot"));
        assert!(should_trigger(
            &config(IrcTriggerPolicy::AllMessages),
            "alice",
            "hello"
        ));
    }

    #[test]
    fn detects_irc_error_numerics() {
        assert!(is_irc_error_numeric(
            ":cadmium.libera.chat 432 * exoclaw-test-12345 :Erroneous Nickname"
        ));
        assert!(!is_irc_error_numeric(
            ":cadmium.libera.chat 001 exoclaw :Welcome"
        ));
    }
}
