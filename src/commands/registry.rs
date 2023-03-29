use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use spin_oci::Client;
use std::{io::Read, path::PathBuf};

use crate::opts::*;

/// Commands for working with OCI registries to distribute applications.
#[derive(Subcommand, Debug)]
pub enum RegistryCommands {
    /// Push a Spin application to a registry.
    Push(Push),
    /// Temporary push operation to Cloud's registry endpoint.
    CloudPush(CloudPush),
    /// Pull a Spin application from a registry.
    Pull(Pull),
    /// Log in to a registry.
    Login(Login),
}

impl RegistryCommands {
    pub async fn run(self) -> Result<()> {
        match self {
            RegistryCommands::Push(cmd) => cmd.run().await,
            RegistryCommands::CloudPush(cmd) => cmd.run().await,
            RegistryCommands::Pull(cmd) => cmd.run().await,
            RegistryCommands::Login(cmd) => cmd.run().await,
        }
    }
}

#[derive(Parser, Debug)]
pub struct CloudPush {
    /// Path to spin.toml
    #[clap(
        name = APP_MANIFEST_FILE_OPT,
        short = 'f',
        long = "file",
    )]
    pub app: Option<PathBuf>,

    /// Ignore server certificate errors
    #[clap(
        name = INSECURE_OPT,
        short = 'k',
        long = "insecure",
        takes_value = false,
    )]
    pub insecure: bool,
}

impl CloudPush {
    pub async fn run(self) -> Result<()> {
        println!("Cloud Push");
        // TODO:
        // - Create a new OCI Registry client
        // - Add the Cloud token in the client's token cache
        // - Attempt to push an application using this client, and expect the Cloud API to be able to validate the Cloud token.
        
        let app_file = self
            .app
            .as_deref()
            .unwrap_or_else(|| DEFAULT_MANIFEST_FILE.as_ref());

        let dir = tempfile::tempdir()?;
        let app = spin_loader::local::from_file(&app_file, Some(dir.path())).await?;        

        spin_oci::Client::cloud_push_test(true, &app, "cloud.local.fermyon.link/application:latest", "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJodHRwOi8vc2NoZW1hcy54bWxzb2FwLm9yZy93cy8yMDA1LzA1L2lkZW50aXR5L2NsYWltcy9uYW1laWRlbnRpZmllciI6ImVjNTJjNzQwLWMyODAtNDBhZi1iNDU1LTcyNDk1OGNjMTA2NCIsImh0dHA6Ly9zY2hlbWFzLnhtbHNvYXAub3JnL3dzLzIwMDUvMDUvaWRlbnRpdHkvY2xhaW1zL2VtYWlsYWRkcmVzcyI6InZhdWdobi5kaWNlQGZlcm15b24uY29tIiwidXNlcm5hbWUiOiJ2ZGljZSIsImh0dHA6Ly9zY2hlbWFzLm1pY3Jvc29mdC5jb20vd3MvMjAwOC8wNi9pZGVudGl0eS9jbGFpbXMvcm9sZSI6IkZyZWVUaWVyIiwic3ViIjoiZWM1MmM3NDAtYzI4MC00MGFmLWI0NTUtNzI0OTU4Y2MxMDY0IiwidW5pcXVlX25hbWUiOiJlYzUyYzc0MC1jMjgwLTQwYWYtYjQ1NS03MjQ5NThjYzEwNjQiLCJqdGkiOiI2YzlmNGZiYy1lZDc0LTRiYzEtYjJkNi05ZmRjMzgyMTdmNjQiLCJleHAiOjE2ODA2NDQxNTgsImlzcyI6ImNsb3VkLmxvY2FsLmZlcm15b24ubGluayIsImF1ZCI6ImNsb3VkLmxvY2FsLmZlcm15b24ubGluayJ9.sXG4uzGlzbWr4YbcPytlxe7rFpn81YdCRYJ7hQj4ZXY".to_string()).await?;
        

        Ok(())
    }
}

#[derive(Parser, Debug)]
pub struct Push {
    /// Path to spin.toml
    #[clap(
        name = APP_MANIFEST_FILE_OPT,
        short = 'f',
        long = "file",
    )]
    pub app: Option<PathBuf>,

    /// Ignore server certificate errors
    #[clap(
        name = INSECURE_OPT,
        short = 'k',
        long = "insecure",
        takes_value = false,
    )]
    pub insecure: bool,

    /// Reference of the Spin application
    #[clap()]
    pub reference: String,
}

impl Push {
    pub async fn run(self) -> Result<()> {
        let app_file = self
            .app
            .as_deref()
            .unwrap_or_else(|| DEFAULT_MANIFEST_FILE.as_ref());

        let dir = tempfile::tempdir()?;
        let app = spin_loader::local::from_file(&app_file, Some(dir.path())).await?;

        let mut client = spin_oci::Client::new(self.insecure, None).await?;
        let digest = client.push(&app, &self.reference).await?;

        match digest {
            Some(digest) => println!("Pushed with digest {digest}"),
            None => println!("Pushed; the registry did not return the digest"),
        };

        Ok(())
    }
}

#[derive(Parser, Debug)]
pub struct Pull {
    /// Ignore server certificate errors
    #[clap(
        name = INSECURE_OPT,
        short = 'k',
        long = "insecure",
        takes_value = false,
    )]
    pub insecure: bool,

    /// Reference of the Spin application
    #[clap()]
    pub reference: String,
}

impl Pull {
    /// Pull a Spin application from an OCI registry
    pub async fn run(self) -> Result<()> {
        let mut client = spin_oci::Client::new(self.insecure, None).await?;
        client.pull(&self.reference).await?;

        Ok(())
    }
}

#[derive(Parser, Debug)]
pub struct Login {
    /// Username for the registry
    #[clap(long = "username", short = 'u')]
    pub username: Option<String>,

    /// Password for the registry
    #[clap(long = "password", short = 'p')]
    pub password: Option<String>,

    /// Take the password from stdin
    #[clap(
        long = "password-stdin",
        takes_value = false,
        conflicts_with = "password"
    )]
    pub password_stdin: bool,

    #[clap()]
    pub server: String,
}

impl Login {
    pub async fn run(self) -> Result<()> {
        let username = match self.username {
            Some(u) => u,
            None => {
                let prompt = "Username";
                loop {
                    let result = dialoguer::Input::<String>::new()
                        .with_prompt(prompt)
                        .interact_text()?;
                    if result.trim().is_empty() {
                        continue;
                    } else {
                        break result;
                    }
                }
            }
        };

        // If the --password-stdin flag is passed, read the password from standard input.
        // Otherwise, if the --password flag was passed with a value, use that value. Finally, if
        // neither was passed, prompt the user to input the password.
        let password = if self.password_stdin {
            let mut buf = String::new();
            let mut stdin = std::io::stdin().lock();
            stdin.read_to_string(&mut buf)?;
            buf
        } else {
            match self.password {
                Some(p) => p,
                None => rpassword::prompt_password("Password: ")?,
            }
        };

        Client::login(&self.server, &username, &password)
            .await
            .context("cannot log in to the registry")?;

        println!(
            "Successfully logged in as {} to registry {}",
            username, &self.server
        );
        Ok(())
    }
}
