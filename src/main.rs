#![allow(dead_code)]

use std::{
    collections::HashMap,
    fmt::Display,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use color_eyre::eyre::{bail, ensure, Context, ContextCompat, Result};
use directories::ProjectDirs;
use docker_compose_types::{
    Compose, ComposeNetwork, Labels, MapOrEmpty, NetworkSettings, Networks,
};
use inquire::{Autocomplete, Editor, Select, Text};
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

#[derive(Debug, Deserialize, Serialize, Default)]
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
    traefik_network: String,
    traefik_tls: String,
}

#[derive(Debug, Clone)]
struct SubdomainAutoComplete {
    current: Vec<String>,
}

impl Autocomplete for SubdomainAutoComplete {
    fn get_suggestions(
        &mut self,
        input: &str,
    ) -> std::result::Result<Vec<String>, inquire::CustomUserError> {
        let input = input.to_lowercase();
        std::result::Result::Ok(
            self.current
                .iter()
                .filter(|i| i.contains(&input))
                .map(String::from)
                .collect(),
        )
    }

    fn get_completion(
        &mut self,
        _: &str,
        highlighted_suggestion: Option<String>,
    ) -> std::result::Result<inquire::autocompletion::Replacement, inquire::CustomUserError> {
        std::result::Result::Ok(highlighted_suggestion)
    }
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
    let zone_id = Text::new("Zone ID:")
        .with_help_message("This can be found on the dashboard.")
        .prompt()?;

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

fn dns() -> Result<()> {
    let config: Config = match get_config() {
        Ok(c) => {
            if c.zones.is_empty() {
                prompt_new_zone_config(&c.cloudflare_key)?
            } else {
                c
            }
        }
        Err(_) => {
            let api_key = std::env::var("CF_API_KEY").or_else(|_| {
                Text::new("Enter your api key.")
                    .with_help_message(
                        "This can also be provided via the `CF_API_KEY` environment variable.",
                    )
                    .prompt()
            })?;
            prompt_new_zone_config(&api_key)?
        }
    };

    let domain = Select::new("Select a zone", config.zones).prompt()?;

    let res: CloudflareResponse<Vec<DnsListResponse>> = (*CLIENT)
        .get(format!("{BASE_URL}/zones/{}/dns_records", &domain.id))
        .bearer_auth(&config.cloudflare_key)
        .send()?
        .json()?;

    if !res.errors.is_empty() {
        bail!("Cloudflare api returned an error: {:?}", res.errors);
    }

    let domains = res.result.unwrap().clone();
    let autocomplete = SubdomainAutoComplete {
        current: domains.iter().map(|d| d.name.clone()).collect(),
    };
    let subdomain = Text::new("What subdomain would you like to modify?")
        .with_autocomplete(autocomplete)
        .prompt()?;

    let record_type = Text::new("What record type is this?")
        .with_default("CNAME")
        .prompt()?;
    let target = Text::new("What is the target?")
        .with_default(&domain.name)
        .prompt()?;

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

fn traefik() -> Result<()> {
    static STATIC_LABELS: LazyLock<HashMap<&str, &str>> = LazyLock::new(|| {
        HashMap::from([
            ("traefik.enable", "true"),
            (
                "traefik.http.routers.${COMPOSE_PROJECT_NAME}.entrypoints",
                "websecure",
            ),
            ("traefik.http.routers.${COMPOSE_PROJECT_NAME}.tls", "true"),
        ])
    });
    static RULE_LABEL: &str = "traefik.http.routers.${COMPOSE_PROJECT_NAME}.rule";
    static CERT_RESOLVER_LABEL: &str =
        "traefik.http.routers.${COMPOSE_PROJECT_NAME}.tls.certresolver";
    static TRAEFIK_NETWORK_LABEL: &str = "traefik.docker.network";
    static TRAEFIK_SERVICE_LABEL: &str = "";
    static TRAEFIK_SERVICE_PORT_LABEL: &str =
        "traefik.http.services.${COMPOSE_PROJECT_NAME}.loadbalancer.server.port";

    let config = match get_config() {
        Ok(mut c) => {
            if c.traefik_network.is_empty() {
                let network = Text::new("Enter the network that traefik is on.").prompt()?;
                c.traefik_network = network;
            }
            if c.traefik_tls.is_empty() {
                let tls = Text::new("Enter your TLS provider for traefik.").prompt()?;
                c.traefik_tls = tls;
            }
            std::fs::write(
                (*CONFIG_DIR).join("config.json"),
                serde_json::to_string(&c)?,
            )?;
            c
        }
        Err(_) => {
            let network = Text::new("Enter the network that traefik is on.").prompt()?;
            let tls = Text::new("Enter your TLS provider for traefik.").prompt()?;
            let config = Config {
                traefik_network: network,
                traefik_tls: tls,
                ..Default::default()
            };
            std::fs::write(
                (*CONFIG_DIR).join("config.json"),
                serde_json::to_string(&config)?,
            )?;
            config
        }
    };

    let file = match std::env::args().nth(2) {
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

    let services = compose.services.0.iter().filter(|e| e.1.is_some());
    let selected_service = Select::new(
        "Select the service to add traefik to",
        services.map(|s| s.0).collect(),
    )
    .prompt()?;

    let service = compose
        .services
        .0
        .get(selected_service)
        .context("Could not get the service.")?;

    let rule = Editor::new("Enter the rule for this service.").prompt()?;
    let port: Option<u16> = loop {
        let text = Text::new("Enter the port this application exposes")
            .with_default("None")
            .prompt_skippable()?;

        match text.as_deref() {
            Some("None") => break None,
            Some(t) => match t.parse() {
                Ok(n) => break Some(n),
                Err(_) => continue,
            },
            None => break None,
        }
    };

    ensure!(service.is_some(), "The service is none.");

    let mut service = service.clone().unwrap();
    for (key, value) in &*STATIC_LABELS {
        add_or_ignore_label(&mut service.labels, key, value);
    }

    add_or_ignore_label(&mut service.labels, RULE_LABEL, &rule);
    add_or_ignore_label(
        &mut service.labels,
        CERT_RESOLVER_LABEL,
        &config.traefik_tls,
    );
    add_or_ignore_label(
        &mut service.labels,
        TRAEFIK_NETWORK_LABEL,
        &config.traefik_network,
    );

    if port.is_some() {
        let port = port.unwrap();
        add_or_ignore_label(
            &mut service.labels,
            TRAEFIK_SERVICE_PORT_LABEL,
            &port.to_string(),
        );
    }

    // get or make the network settings for the traefik network
    let mut network = compose
        .networks
        .0
        .get(&config.traefik_network)
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
        .insert(config.traefik_network.clone(), MapOrEmpty::Map(network));

    match &mut service.networks {
        Networks::Simple(a) => {
            if !a.contains(&config.traefik_network) {
                a.push(config.traefik_network);
            }
        }
        Networks::Advanced(a) => {
            a.0.insert(config.traefik_network, MapOrEmpty::Empty);
        }
    }

    compose
        .services
        .0
        .insert(selected_service.clone(), Some(service));

    std::fs::write(&file, serde_yml::to_string(&compose)?)?;

    Ok(())
}

fn main() -> Result<()> {
    color_eyre::install()?;

    let cmd = match std::env::args().nth(1).map(|f| f.to_lowercase()) {
        Some(s) => s,
        None => {
            bail!("no command given.");
        }
    };

    match cmd.as_str() {
        "dns" => dns(),
        "traefik" => traefik(),
        _ => bail!("Invalid command, it should be ``dns`` or ``traefik``."),
    }
}
