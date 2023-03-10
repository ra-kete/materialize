// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License in the LICENSE file at the
// root of this repository, or online at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// BEGIN LINT CONFIG
// DO NOT EDIT. Automatically generated by bin/gen-lints.
// Have complaints about the noise? See the note in misc/python/materialize/cli/gen-lints.py first.
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![warn(clippy::bool_comparison)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::no_effect)]
#![warn(clippy::unnecessary_unwrap)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::todo)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::zero_prefixed_literal)]
#![warn(clippy::borrowed_box)]
#![warn(clippy::deref_addrof)]
#![warn(clippy::double_must_use)]
#![warn(clippy::double_parens)]
#![warn(clippy::extra_unused_lifetimes)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_question_mark)]
#![warn(clippy::needless_return)]
#![warn(clippy::redundant_pattern)]
#![warn(clippy::redundant_slicing)]
#![warn(clippy::redundant_static_lifetimes)]
#![warn(clippy::single_component_path_imports)]
#![warn(clippy::unnecessary_cast)]
#![warn(clippy::useless_asref)]
#![warn(clippy::useless_conversion)]
#![warn(clippy::builtin_type_shadow)]
#![warn(clippy::duplicate_underscore_argument)]
#![warn(clippy::double_neg)]
#![warn(clippy::unnecessary_mut_passed)]
#![warn(clippy::wildcard_in_or_patterns)]
#![warn(clippy::collapsible_if)]
#![warn(clippy::collapsible_else_if)]
#![warn(clippy::crosspointer_transmute)]
#![warn(clippy::excessive_precision)]
#![warn(clippy::overflow_check_conditional)]
#![warn(clippy::as_conversions)]
#![warn(clippy::match_overlapping_arm)]
#![warn(clippy::zero_divided_by_zero)]
#![warn(clippy::must_use_unit)]
#![warn(clippy::suspicious_assignment_formatting)]
#![warn(clippy::suspicious_else_formatting)]
#![warn(clippy::suspicious_unary_op_formatting)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::print_literal)]
#![warn(clippy::same_item_push)]
#![warn(clippy::useless_format)]
#![warn(clippy::write_literal)]
#![warn(clippy::redundant_closure)]
#![warn(clippy::redundant_closure_call)]
#![warn(clippy::unnecessary_lazy_evaluations)]
#![warn(clippy::partialeq_ne_impl)]
#![warn(clippy::redundant_field_names)]
#![warn(clippy::transmutes_expressible_as_ptr_casts)]
#![warn(clippy::unused_async)]
#![warn(clippy::disallowed_methods)]
#![warn(clippy::disallowed_macros)]
#![warn(clippy::disallowed_types)]
#![warn(clippy::from_over_into)]
// END LINT CONFIG

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use secrets::SecretCommand;
use serde::Deserialize;
use utils::{ascii_validator, new_client};

use mz::api::{
    disable_region_environment, enable_region_environment, get_provider_by_region_name,
    get_provider_region_environment, get_region_environment, list_cloud_providers, list_regions,
    CloudProviderRegion,
};
use mz::configuration::{Configuration, Endpoint, WEB_DOCS_URL};
use mz::vault::Vault;
use mz_build_info::{build_info, BuildInfo};
use mz_ore::cli::CliConfig;

use crate::login::{generate_api_token, login_with_browser, login_with_console};
use crate::password::list_passwords;
use crate::region::{print_environment_status, print_region_enabled};
use crate::shell::{check_environment_health, shell};
use crate::utils::run_loading_spinner;

mod login;
mod password;
mod region;
mod secrets;
mod shell;
mod utils;

pub const BUILD_INFO: BuildInfo = build_info!();

static VERSION: Lazy<String> = Lazy::new(|| BUILD_INFO.semver_version().to_string());

/// Command-line interface for Materialize.
#[derive(Debug, clap::Parser)]
#[clap(
    long_about = None,
    version = VERSION.as_str(),
)]
struct Args {
    /// The configuration profile to use.
    #[clap(long, validator = ascii_validator)]
    profile: Option<String>,
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Debug, clap::Subcommand)]
enum Commands {
    /// Show commands to interact with passwords
    AppPassword(AppPasswordCommand),
    /// Open the docs
    Docs,
    /// Login to a profile and make it the active profile
    Login {
        /// Login by typing your email and password
        #[clap(short, long)]
        interactive: bool,

        /// Force reauthentication for the profile
        #[clap(short, long)]
        force: bool,

        #[clap(flatten)]
        vault: Vault,

        /// Override the default API endpoint.
        #[clap(long, hide = true, default_value_t)]
        endpoint: Endpoint,
    },
    /// Show commands to interact with regions
    Region {
        #[clap(subcommand)]
        command: RegionCommand,
    },

    /// Show commands to interact with secrets
    Secret {
        cloud_provider_region: Option<CloudProviderRegion>,

        #[clap(subcommand)]
        command: SecretCommand,
    },

    /// Connect to a region using a SQL shell
    Shell {
        cloud_provider_region: Option<CloudProviderRegion>,
    },
}

#[derive(Debug, clap::Args)]
struct AppPasswordCommand {
    #[clap(subcommand)]
    command: AppPasswordSubcommand,
}

#[derive(Debug, clap::Subcommand)]
enum AppPasswordSubcommand {
    /// Create a password.
    Create {
        /// Name for the password.
        name: String,
    },
    /// List all enabled passwords.
    List,
}

#[derive(Debug, clap::Subcommand)]
enum RegionCommand {
    /// Enable a region.
    Enable {
        cloud_provider_region: CloudProviderRegion,
        #[clap(long, hide = true)]
        version: Option<String>,
        #[clap(long, hide = true)]
        environmentd_extra_arg: Vec<String>,
    },
    /// Disable a region.
    #[clap(hide = true)]
    Disable {
        cloud_provider_region: CloudProviderRegion,
    },
    /// List all enabled regions.
    List,
    /// Display a region's status.
    Status {
        cloud_provider_region: CloudProviderRegion,
    },
}

/// Internal types, struct and enums

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FronteggAppPassword {
    description: String,
    created_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserAPIToken {
    email: String,
    client_id: String,
    secret: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Args = mz_ore::cli::parse_args(CliConfig {
        env_prefix: Some("mz"),
        enable_version_flag: true,
    });
    let client = new_client()?;
    let mut config = Configuration::load(args.profile.as_deref())?;
    let profile_name = config.current_profile();

    match args.command {
        Commands::AppPassword(password_cmd) => {
            let profile = config.get_profile()?;

            let valid_profile = profile.validate(&profile_name, &client).await?;

            match password_cmd.command {
                AppPasswordSubcommand::Create { name } => {
                    let api_token = generate_api_token(
                        profile.endpoint(),
                        &client,
                        valid_profile.frontegg_auth,
                        &name,
                    )
                    .await
                    .with_context(|| "failed to create a new app password")?;

                    println!("{}", api_token)
                }
                AppPasswordSubcommand::List => {
                    let app_passwords = list_passwords(&client, &valid_profile)
                        .await
                        .with_context(|| "failed to retrieve app passwords")?;

                    println!("{0: <24} | {1: <24} ", "Name", "Created At");
                    println!("----------------------------------------------------");

                    app_passwords.iter().for_each(|app_password| {
                        let mut name = app_password.description.clone();

                        if name.len() > 20 {
                            let short_name = name[..20].to_string();
                            name = format!("{:}...", short_name);
                        }

                        println!("{0: <24} | {1: <24}", name, app_password.created_at);
                    })
                }
            }
        }

        Commands::Docs => {
            // Open the browser docs
            open::that(WEB_DOCS_URL).with_context(|| "Opening the browser.")?
        }

        Commands::Login {
            interactive,
            force,
            vault,
            endpoint,
        } => {
            let profile = args.profile.unwrap_or_else(|| "default".into());
            config.update_current_profile(profile.clone());
            let old_profile = config.get_profile();
            if old_profile.is_err() || force {
                let endpoint = old_profile
                    .map(|p| p.endpoint().clone())
                    .unwrap_or(endpoint);

                let (email, api_token) = if interactive {
                    login_with_console(&endpoint, &client).await?
                } else {
                    login_with_browser(&endpoint, &profile).await?
                };

                let token = vault.store(&profile, &email, api_token)?;
                config.create_or_update_profile(endpoint, profile, email, token)
            } else {
                println!("Already logged in. Rerun with --force to reauthenticate.")
            }
        }

        Commands::Region { command } => match command {
            RegionCommand::Enable {
                cloud_provider_region,
                version,
                environmentd_extra_arg,
            } => {
                let mut profile = config.get_profile()?;

                let valid_profile = profile.validate(&profile_name, &client).await?;

                let loading_spinner = run_loading_spinner("Enabling region...".to_string());
                let cloud_provider =
                    get_provider_by_region_name(&client, &valid_profile, &cloud_provider_region)
                        .await
                        .with_context(|| "Retrieving cloud provider.")?;

                let region = enable_region_environment(
                    &client,
                    &cloud_provider,
                    version,
                    environmentd_extra_arg,
                    &valid_profile,
                )
                .await
                .with_context(|| "Enabling region.")?;

                let environment = get_region_environment(&client, &valid_profile, &region)
                    .await
                    .with_context(|| "Retrieving environment data.")?;

                loop {
                    if check_environment_health(&valid_profile, &environment)? {
                        break;
                    }
                }

                loading_spinner.finish_with_message(format!("{cloud_provider_region} enabled"));
                profile.set_default_region(cloud_provider_region);
            }

            RegionCommand::Disable {
                cloud_provider_region,
            } => {
                let profile = config.get_profile()?;

                let valid_profile = profile.validate(&profile_name, &client).await?;

                let loading_spinner = run_loading_spinner("Disabling region...".to_string());
                let cloud_provider =
                    get_provider_by_region_name(&client, &valid_profile, &cloud_provider_region)
                        .await
                        .with_context(|| "Retrieving cloud provider.")?;

                disable_region_environment(&client, &cloud_provider, &valid_profile)
                    .await
                    .with_context(|| "Disabling region.")?;

                loading_spinner.finish_with_message(format!("{cloud_provider_region} disabled"));
            }

            RegionCommand::List => {
                let profile = config.get_profile()?;

                let valid_profile = profile.validate(&profile_name, &client).await?;

                let cloud_providers = list_cloud_providers(&client, &valid_profile)
                    .await
                    .with_context(|| "Retrieving cloud providers.")?;
                let cloud_providers_regions =
                    list_regions(&cloud_providers.data, &client, &valid_profile)
                        .await
                        .with_context(|| "Listing regions.")?;
                cloud_providers_regions
                    .iter()
                    .for_each(|cloud_provider_region| {
                        print_region_enabled(cloud_provider_region);
                    });
            }

            RegionCommand::Status {
                cloud_provider_region,
            } => {
                let profile = config.get_profile()?;

                let valid_profile = profile.validate(&profile_name, &client).await?;

                let environment = get_provider_region_environment(
                    &client,
                    &valid_profile,
                    &cloud_provider_region,
                )
                .await
                .with_context(|| "Retrieving cloud provider region.")?;
                let health = check_environment_health(&valid_profile, &environment)?;

                print_environment_status(&valid_profile, environment, health)
                    .with_context(|| "Printing the status of the environment.")?;
            }
        },

        Commands::Secret {
            cloud_provider_region,
            command,
        } => {
            let profile = config.get_profile()?;

            let cloud_provider_region = match cloud_provider_region {
                Some(cloud_provider_region) => cloud_provider_region,
                None => profile
                    .get_default_region()
                    .context("no region specified and no default region set")?,
            };

            let valid_profile = profile.validate(&profile_name, &client).await?;

            command
                .execute(valid_profile, cloud_provider_region, client)
                .await?
        }

        Commands::Shell {
            cloud_provider_region,
        } => {
            let profile = config.get_profile()?;

            let cloud_provider_region = match cloud_provider_region {
                Some(cloud_provider_region) => cloud_provider_region,
                None => profile
                    .get_default_region()
                    .context("no region specified and no default region set")?,
            };

            let valid_profile = profile.validate(&profile_name, &client).await?;

            shell(client, valid_profile, cloud_provider_region)
                .await
                .with_context(|| "Running shell")?;
        }
    }

    config.close()
}
