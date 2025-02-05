use std::{collections::HashSet, path::Path, time::Duration};

use clap::Parser;
use eyre::eyre;
use reqwest::Response;
use serde::Serialize;
use tokio::{
    fs::OpenOptions,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    process::Command,
};
use uuid::Uuid;

use crate::{cmds::Commands, dev_env::DetectedLanguage, Cli, RIFF_XDG_PREFIX};

static TELEMETRY_DISTINCT_ID_PATH: &str = "distinct_id";
static TELEMETRY_IDENTIFIER_DESCRIPTION: &str =  "This is a randomly generated version 4 UUID.
Determinate Systems uses this ID to know how many people use the tool and to focus our limited research and development.
This ID is completely random and contains no personally identifiable information about you.
You can delete this file at any time to create a new ID.
You can also disable ID generation; see the documentation on telemetry to see how to do so.";
static TELEMETRY_REMOTE_URL: &str = "https://registry.riff.determinate.systems/telemetry";
pub static TELEMETRY_HEADER_NAME: &str = "X-RIFF-Client-Info";

#[derive(Debug, Serialize)]
pub(crate) struct Telemetry {
    /// Stored in `$XDG_DATA_HOME/riff/distinct_id` as a UUIDv4
    distinct_id: Option<Uuid>,
    system_os: String,
    system_arch: String,
    /// `NAME` from `/etc/os-release`
    os_release_name: Option<String>,
    /// `VERSION_ID` from `/etc/os-release`
    os_release_version_id: Option<String>,
    /// The `CARGO_PKG_VERSION` from a `riff` build
    riff_version: String,
    /// The version output of `nix --version`
    nix_version: Option<String>,
    /// If the exit code of `test -t 0` is 0, then this is true, otherwise false
    is_tty: bool,
    /// The command given to riff (eg "shell")
    subcommand: Option<String>,
    detected_languages: HashSet<DetectedLanguage>,
    in_ci: bool,
}

impl Telemetry {
    pub(crate) async fn from_clap_parse_result(command: Option<&crate::Commands>) -> Self {
        let distinct_id = match distinct_id().await {
            Ok(distinct_id) => Some(distinct_id),
            Err(err) => {
                tracing::debug!(err = %eyre::eyre!(err), "Could get distinct ID for telemetry");
                None
            }
        };

        let system_os = std::env::consts::OS.to_string();
        let system_arch = std::env::consts::ARCH.to_string();
        let riff_version = env!("CARGO_PKG_VERSION").to_string();
        let nix_version = match nix_version().await {
            Ok(nix_version) => nix_version,
            Err(err) => {
                tracing::debug!(err = %eyre::eyre!(err), "Could get `nix --version` for telemetry");
                None
            }
        };
        let os_release: Option<os_release::OsRelease> =
            tokio::task::spawn_blocking(|| os_release::OsRelease::new().ok())
                .await
                .unwrap_or(None);

        let is_tty = atty::is(atty::Stream::Stdout);

        let subcommand = match command {
            Some(Commands::Shell(_)) => Some("shell".to_string()),
            Some(Commands::Run(_)) => Some("run".to_string()),
            Some(Commands::PrintDevEnv(_)) => Some("print-dev-env".to_string()),
            None => None,
        };

        Self {
            distinct_id,
            system_os,
            system_arch,
            os_release_name: os_release.as_ref().map(|x| x.name.clone()),
            os_release_version_id: os_release.as_ref().map(|x| x.version_id.clone()),
            riff_version,
            nix_version,
            is_tty,
            subcommand,
            detected_languages: Default::default(),
            in_ci: is_ci::cached(),
        }
    }

    /// Create a new `Telemetry` without any pre-existing information
    ///
    /// This is not very performant and may do things like re-invoke `nix` or reparse the `$ARG`s.
    pub(crate) async fn new() -> Self {
        let cli = Cli::try_parse().ok().map(|c| c.command);

        Self::from_clap_parse_result(cli.as_ref()).await
    }

    pub(crate) fn with_detected_languages(mut self, languages: &HashSet<DetectedLanguage>) -> Self {
        self.detected_languages = languages.iter().cloned().collect();
        self
    }

    #[tracing::instrument(skip_all)]
    pub(crate) async fn send(&self) -> eyre::Result<Response> {
        let header_data = self.as_header_data()?;
        tracing::trace!(data = %self.redact_header_data(header_data.clone()), "Sending telemetry data to {TELEMETRY_REMOTE_URL}");
        let http_client = reqwest::Client::new();
        let req = http_client
            .post(TELEMETRY_REMOTE_URL)
            .header(TELEMETRY_HEADER_NAME, &header_data)
            .timeout(Duration::from_millis(250));
        let res = req.send().await?;
        tracing::debug!(telemetry = %self.redact_header_data(header_data.clone()), "Sent telemetry data to {TELEMETRY_REMOTE_URL}");
        Ok(res)
    }

    pub(crate) fn as_header_data(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self)
    }

    pub(crate) fn redact_header_data(&self, mut val: String) -> String {
        if let Some(distinct_id) = self.distinct_id {
            let distinct_id_string = distinct_id.to_string();
            val = val.replace(&distinct_id_string, "<redacted>");
        }
        val
    }
}

async fn distinct_id() -> eyre::Result<Uuid> {
    let xdg_dirs = xdg::BaseDirectories::with_prefix(RIFF_XDG_PREFIX)?;
    let distinct_id_path = xdg_dirs.place_config_file(Path::new(TELEMETRY_DISTINCT_ID_PATH))?;

    let mut distinct_id_file = OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(false)
        .create(true) // We do this proactively to avoid the user seeing a non-fatal error later when we freshen the cache.
        .open(distinct_id_path.clone())
        .await?;
    // The first line will be the uuid, the rest will be newlines or `TELEMETRY_IDENTIFIER_DESCRIPTION`
    let mut distinct_id = Default::default();
    distinct_id_file.read_to_string(&mut distinct_id).await?;
    if let Some(len) = distinct_id.find('\n') {
        distinct_id.truncate(len);
        distinct_id = distinct_id.trim().to_string();
    }

    match Uuid::parse_str(&distinct_id) {
        Ok(uuid) => Ok(uuid),
        Err(e) => {
            tracing::debug!("Failed to parse out the distinct_id: {}", e);
            let uuid = Uuid::new_v4();
            tracing::trace!(%uuid, "Writing new distinct ID");
            distinct_id_file.set_len(0).await?;
            distinct_id_file.seek(std::io::SeekFrom::Start(0)).await?;
            distinct_id_file
                .write_all(format!("{uuid}\n\n{TELEMETRY_IDENTIFIER_DESCRIPTION}").as_bytes())
                .await?;
            tracing::debug!(%uuid, "Wrote new distinct ID");
            Ok(uuid)
        }
    }
}

async fn nix_version() -> eyre::Result<Option<String>> {
    let mut command = Command::new("nix");
    command.arg("--version");
    let output = command.output().await;
    match output {
        Ok(output) => {
            if output.status.success() {
                let stdout = output.stdout;
                let stdout_string = std::str::from_utf8(&stdout)?.trim().to_string();
                Ok(Some(stdout_string))
            } else {
                Err(eyre!("`nix --version` failed to run for telemetry"))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::trace!("Could not run `nix --version` due to `EPERM`, this is likely because `nix` is not installed");
            Ok(None)
        }
        Err(err) => Err(err.into()),
    }
}
