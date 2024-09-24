#![allow(dead_code)]

use std::{
    fmt::Display,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use clap::{Parser, Subcommand};
use color_eyre::eyre::{bail, ensure, Context, ContextCompat, Result};
use directories::ProjectDirs;
use docker_compose_types::{
    Compose, ComposeNetwork, Labels, MapOrEmpty, NetworkSettings, Networks, Service,
};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

static BASE_URL: &str = "https://api.cloudflare.com/client/v4";
static CLIENT: LazyLock<Client> = LazyLock::new(Client::new);
static PROJECT_DIR: LazyLock<ProjectDirs> =
    LazyLock::new(|| ProjectDirs::from("gay", "vaskel", "eurus").unwrap());
static CONFIG_DIR: LazyLock<&Path> = LazyLock::new(|| PROJECT_DIR.config_dir());

#[derive(Debug, Deserialize, Default, Clone)]
struct CloudflareResponse<T> {
    errors: Vec<CloudflareError>,
    result: Option<T>,
}

#[derive(Debug, Deserialize, Clone)]
struct CloudflareError {
    code: i32,
    message: String,
}

#[derive(Debug, Deserialize, Clone)]
struct ZoneDetailsResponse {
    name: String,
    id: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct DnsListResponse {
    name: String,
    id: String,
    #[serde(rename = "type")]
    record_type: String,
    proxied: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct DnsCreateUpdate {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(rename = "type")]
    record_type: String,
    proxied: bool,
    content: String,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone, PartialEq, Eq)]
struct ZoneInfo {
    id: String,
    name: String,
}

impl Display for ZoneInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.name, self.id)
    }
}

#[derive(Debug, Deserialize, Serialize, Default)]
struct Config {
    zones: Vec<ZoneInfo>,
    cloudflare_key: String,
    caddy_network: String,
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand, Clone)]
enum Command {
    #[command(about = "Change DNS settings via the cloudflare api.")]
    Dns,
    #[command(about = "Edit a docker compose file to add caddy proxying.")]
    Web { path: Option<String> },
}

fn get_config() -> Result<Config> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(*CONFIG_DIR)
        .context("Failed to create config directory")?;

    serde_json::from_str(&std::fs::read_to_string(&*CONFIG_DIR.join("config.json"))?)
        .context("Configuration is malformed.")
}

fn prompt_new_zone_config(api_key: &str) -> Result<Config> {
    let zone_id = cliclack::input("Zone ID:").interact()?;

    let res: CloudflareResponse<ZoneDetailsResponse> = (*CLIENT)
        .get(format!("{}/zones/{}", BASE_URL, zone_id))
        .bearer_auth(api_key)
        .send()?
        .json::<CloudflareResponse<ZoneDetailsResponse>>()?;

    if !res.errors.is_empty() {
        bail!("Cloudflare api returned an error: {:?}", res.errors);
    }

    let conf = Config {
        zones: vec![ZoneInfo {
            id: zone_id,
            name: res.result.unwrap().name, // We check for errors earlier.
        }],
        cloudflare_key: api_key.to_string(),
        ..Default::default()
    };
    std::fs::write(
        (*CONFIG_DIR).join("config.json"),
        serde_json::to_string(&conf)?,
    )?;

    Ok(conf)
}

#[derive(Debug, PartialEq, Clone)]
struct ServiceWrapper(Service, String);
impl Eq for ServiceWrapper {}

fn dns() -> Result<()> {
    cliclack::intro("eurus-dns")?;

    let config: Config = match get_config() {
        Ok(c) => {
            if c.zones.is_empty() {
                prompt_new_zone_config(&c.cloudflare_key)?
            } else {
                c
            }
        }
        Err(_) => {
            let api_key = std::env::var("CF_API_KEY")
                .or_else(|_| cliclack::input("Enter your api key.").interact())?;
            prompt_new_zone_config(&api_key)?
        }
    };

    let choices: Vec<_> = config.zones.iter().map(|z| (z, &z.name, "")).collect();
    let domain = cliclack::select("Select a zone")
        .items(&choices)
        .interact()?;

    let res: CloudflareResponse<Vec<DnsListResponse>> = (*CLIENT)
        .get(format!("{BASE_URL}/zones/{}/dns_records", &domain.id))
        .bearer_auth(&config.cloudflare_key)
        .send()?
        .json()?;

    if !res.errors.is_empty() {
        bail!("Cloudflare api returned an error: {:?}", res.errors);
    }

    let domains = res.result.unwrap().clone();
    let subdomain: String =
        cliclack::input("Which subdomain would you like to modify?").interact()?;

    let record_type = cliclack::input("What record type is this?")
        .default_input("CNAME")
        .interact()?;
    let target = cliclack::input("What is the target?")
        .default_input(&domain.name)
        .interact()?;

    let info = domains
        .iter()
        .find(|d| d.name == subdomain.clone())
        .cloned();

    let body = DnsCreateUpdate {
        name: subdomain,
        id: None,
        proxied: true,
        record_type,
        content: target,
    };

    let res: CloudflareResponse<DnsListResponse> = if info.is_some() {
        (*CLIENT)
            .patch(format!(
                "{BASE_URL}/zones/{}/dns_records/{}",
                &domain.id,
                info.unwrap().id
            ))
            .json(&body)
            .bearer_auth(&config.cloudflare_key)
            .send()?
            .json()?
    } else {
        (*CLIENT)
            .post(format!("{BASE_URL}/zones/{}/dns_records", &domain.id))
            .json(&body)
            .bearer_auth(&config.cloudflare_key)
            .send()?
            .json()?
    };

    if !res.errors.is_empty() {
        bail!("Cloudflare api returned an error: {:?}", res.errors);
    }

    println!("Done!");

    Ok(())
}

fn add_or_ignore_label(labels: &mut Labels, key: &str, value: &str) {
    match labels {
        Labels::List(l) => {
            let label = format!("{}={}", key, value);
            if !l.contains(&label) {
                l.push(label);
            }
        }
        Labels::Map(m) => {
            if !m.contains_key(key) {
                m.insert(key.to_string(), value.to_string());
            }
        }
    }
}

fn web(compose_path: Option<String>) -> Result<()> {
    cliclack::intro("eurus-web")?;

    let config = match get_config() {
        Ok(mut c) => {
            if c.caddy_network.is_empty() {
                let network = cliclack::input("Enter the network that caddy is on.").interact()?;
                c.caddy_network = network;
            }
            std::fs::write(
                (*CONFIG_DIR).join("config.json"),
                serde_json::to_string(&c)?,
            )?;
            c
        }
        Err(_) => {
            let network = cliclack::input("Enter the network that caddy is on.").interact()?;
            let config = Config {
                caddy_network: network,
                ..Default::default()
            };
            std::fs::write(
                (*CONFIG_DIR).join("config.json"),
                serde_json::to_string(&config)?,
            )?;
            config
        }
    };

    let file = match compose_path {
        Some(s) => PathBuf::from(s),
        None => {
            static COMPOSE_PATHS: [&str; 2] = ["compose.yaml", "docker-compose.yaml"];

            COMPOSE_PATHS
                .iter()
                .find(|p| Path::new(p).exists())
                .map(PathBuf::from)
                .context("Could not find valid docker-compose file.")?
        }
    };

    ensure!(Path::new(&file).exists(), "The file provided should exist.");

    let contents = std::fs::read_to_string(&file).context("Could not read the file contents.")?;
    let mut compose: Compose =
        serde_yml::from_str(&contents).context("The compose yaml was invalid.")?;

    let services: Vec<_> = compose
        .services
        .0
        .iter()
        .filter(|e| e.1.is_some())
        .map(|(key, value)| (ServiceWrapper(value.clone().unwrap(), key.clone()), key, ""))
        .collect();
    let selected_service = cliclack::select("Select the service to add caddy to")
        .items(&services)
        .interact()?;

    let domain: String = cliclack::input("Enter the domain for this service.").interact()?;
    let port: u16 = loop {
        let text: String = cliclack::input("Enter the port this application exposes").interact()?;

        match text.parse() {
            Ok(n) => break n,
            Err(_) => (),
        }
    };

    let mut service = selected_service.0.clone();

    add_or_ignore_label(&mut service.labels, "caddy", &domain);
    add_or_ignore_label(
        &mut service.labels,
        "caddy.reverse_proxy",
        &format!("{{{{ upstreams {} }}}}", port),
    );

    // get or make the network settings for the traefik network
    let mut network = compose
        .networks
        .0
        .get(&config.caddy_network)
        .map(|n| match n {
            MapOrEmpty::Empty => NetworkSettings {
                ..Default::default()
            },
            MapOrEmpty::Map(m) => m.clone(),
        })
        .unwrap_or(NetworkSettings {
            ..Default::default()
        }); // Should never be None

    network.external = Some(ComposeNetwork::Bool(true));

    compose
        .networks
        .0
        .insert(config.caddy_network.clone(), MapOrEmpty::Map(network));

    match &mut service.networks {
        Networks::Simple(a) => {
            if !a.contains(&config.caddy_network) {
                a.push(config.caddy_network);
            }
        }
        Networks::Advanced(a) => {
            a.0.insert(config.caddy_network, MapOrEmpty::Empty);
        }
    }

    compose
        .services
        .0
        .insert(selected_service.1.clone(), Some(service));

    std::fs::copy(&file, format!("{}.bak", file.display()))?;
    std::fs::write(&file, serde_yml::to_string(&compose)?)?;

    cliclack::outro("Done!")?;

    Ok(())
}

fn main() -> Result<()> {
    color_eyre::install()?;

    let args = Cli::parse();

    match args.command {
        Command::Dns => dns(),
        Command::Web { path } => web(path),
    }
}
