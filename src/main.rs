#![allow(dead_code)]

use std::{fmt::Display, sync::LazyLock};

use color_eyre::eyre::{bail, Context, ContextCompat, Result};
use directories::ProjectDirs;
use inquire::{autocompletion::Replacement, Autocomplete, Select, Text};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

static BASE_URL: &str = "https://api.cloudflare.com/client/v4";
static CLIENT: LazyLock<Client> = LazyLock::new(|| Client::new());

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
    content: String
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
    email: String,
    zones: Vec<ZoneInfo>,
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
                .map(|s| String::from(s))
                .collect(),
        )
    }

    fn get_completion(
        &mut self,
        _: &str,
        highlighted_suggestion: Option<String>,
    ) -> std::result::Result<inquire::autocompletion::Replacement, inquire::CustomUserError> {
        std::result::Result::Ok(match highlighted_suggestion {
            Some(s) => Replacement::Some(s),
            None => Replacement::None,
        })
    }
}

fn main() -> Result<()> {
    color_eyre::install()?;

    let api_key = std::env::var("CF_API_KEY").or_else(|_| {
        Text::new("Enter your api key.")
            .with_help_message(
                "This can also be provided via the `CF_API_KEY` environment variable.",
            )
            .prompt()
    })?;

    let proj_dirs = ProjectDirs::from("gay", "vaskel", "eurus")
        .context("Failed to get configuration directory.")?;

    let config_dir = proj_dirs.config_dir();
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&config_dir)
        .context("Failed to create config directory")?;

    let config: Config = match std::fs::read_to_string(config_dir.join("config.json")) {
        Ok(s) => serde_json::from_str(&s).context("Configuration is malformed.")?,
        Err(_) => {
            let zone_id = Text::new("Zone ID:")
                .with_help_message("This can be found on the dashboard.")
                .prompt()?;

            let res: CloudflareResponse<ZoneDetailsResponse> = (*CLIENT)
                .get(format!("{}/zones/{}", BASE_URL, zone_id))
                .bearer_auth(&api_key)
                .send()?
                .json::<CloudflareResponse<ZoneDetailsResponse>>()?;

            if res.errors.len() != 0 {
                bail!("Cloudflare api returned an error: {:?}", res.errors);
            }

            let conf = Config {
                email: "".into(),
                zones: vec![ZoneInfo {
                    id: zone_id,
                    name: res.result.unwrap().name, // We check for errors earlier.
                }],
            };
            std::fs::write(
                config_dir.join("config.json"),
                serde_json::to_string(&conf)?,
            )?;

            conf
        }
    };

    let domain = Select::new("Select a zone", config.zones).prompt()?;

    let res: CloudflareResponse<Vec<DnsListResponse>> = (*CLIENT)
        .get(format!("{BASE_URL}/zones/{}/dns_records", &domain.id))
        .bearer_auth(&api_key)
        .send()?
        .json()?;

    if res.errors.len() != 0 {
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
        .map(|f| f.clone());

    let body = DnsCreateUpdate {
        name: subdomain,
        id: None,
        proxied: true,
        record_type,
        content: target
    };

    let res: CloudflareResponse<DnsListResponse> = if info.is_some() {
        (*CLIENT)
            .patch(format!(
                "{BASE_URL}/zones/{}/dns_records/{}",
                &domain.id,
                info.unwrap().id
            ))
            .json(&body)
            .bearer_auth(&api_key)
            .send()?
            .json()?
    } else {
        (*CLIENT)
            .post(format!("{BASE_URL}/zones/{}/dns_records", &domain.id))
            .json(&body)
            .bearer_auth(&api_key)
            .send()?
            .json()?
    };

    if res.errors.len() != 0 {
        bail!("Cloudflare api returned an error: {:?}", res.errors);
    }

    println!("Done!");

    Ok(())
}
