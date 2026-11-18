//! `factory-dash`: serve the live page, or print the picture once.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use factory_dash::net::Net;
use factory_dash::records::{Links, Reader};
use factory_dash::server::{Settings, serve};

const USAGE: &str = "\
factory-dash serve [options]      serve the live page
factory-dash snapshot [options]   print the picture once, as JSON

  --state <dir>          the factory's state directory
                         (default: ~/.local/state/software-factory)
  --listen <addr:port>   where to listen (default: 127.0.0.1:8717)
  --allow <network>      a network that may connect, as 192.168.101.0/24;
                         may be given more than once
                         (default: 127.0.0.0/8, this machine alone)
  --name <host>          a host name the page may be asked for under,
                         besides addresses; may be given more than once
  --temporal-ui <url>    the Temporal server's web pages, to link to
  --namespace <name>     the factory's namespace there
                         (default: software-factory)
";

/// What can go wrong before the page is served.
#[derive(Debug)]
enum Error {
    Usage(String),
    Listen(String, std::io::Error),
    Serve(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Usage(what) => write!(f, "{what}\n\n{USAGE}"),
            Error::Listen(address, source) => write!(f, "cannot listen on {address}: {source}"),
            Error::Serve(source) => write!(f, "the server stopped: {source}"),
        }
    }
}

struct Asked {
    command: String,
    listen: String,
    settings: Settings,
}

fn asked(args: &[String]) -> Result<Asked, Error> {
    let mut args = args.iter();
    let command = args
        .next()
        .cloned()
        .ok_or_else(|| Error::Usage("no command given".to_string()))?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut state = home.join(".local/state/software-factory");
    let mut listen = "127.0.0.1:8717".to_string();
    let mut allow = Vec::new();
    let mut names = Vec::new();
    let mut links = Links {
        temporal_ui: None,
        namespace: "software-factory".to_string(),
    };
    while let Some(option) = args.next() {
        let mut value = || {
            args.next()
                .cloned()
                .ok_or_else(|| Error::Usage(format!("`{option}` needs a value")))
        };
        match option.as_str() {
            "--state" => state = PathBuf::from(value()?),
            "--listen" => listen = value()?,
            "--allow" => {
                let network = value()?;
                allow.push(Net::parse(&network).ok_or_else(|| {
                    Error::Usage(format!(
                        "`{network}` is not a network like 192.168.101.0/24"
                    ))
                })?);
            }
            "--name" => names.push(value()?),
            "--temporal-ui" => links.temporal_ui = Some(value()?),
            "--namespace" => links.namespace = value()?,
            other => return Err(Error::Usage(format!("unknown option `{other}`"))),
        }
    }
    if allow.is_empty() {
        allow.extend(Net::parse("127.0.0.0/8"));
    }
    Ok(Asked {
        command,
        listen,
        settings: Settings {
            state,
            allow,
            names,
            links,
        },
    })
}

async fn run(args: &[String]) -> Result<(), Error> {
    let asked = asked(args)?;
    match asked.command.as_str() {
        "serve" => {
            let listener = tokio::net::TcpListener::bind(&asked.listen)
                .await
                .map_err(|source| Error::Listen(asked.listen.clone(), source))?;
            eprintln!(
                "serving {} on http://{} to {} allowed network(s)",
                asked.settings.state.display(),
                asked.listen,
                asked.settings.allow.len()
            );
            serve(listener, asked.settings).await.map_err(Error::Serve)
        }
        "snapshot" => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or(0);
            let picture = Reader::new(asked.settings.state, asked.settings.links).picture(now);
            println!("{picture:#}");
            Ok(())
        }
        other => Err(Error::Usage(format!("unknown command `{other}`"))),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("factory-dash {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    match run(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("factory-dash: {error}");
            ExitCode::FAILURE
        }
    }
}
