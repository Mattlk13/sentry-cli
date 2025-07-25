use anyhow::Result;
use clap::{ArgMatches, Command};

use crate::api::Api;
use crate::config::Config;
use crate::utils::args::ArgExt as _;

pub fn make_command(command: Command) -> Command {
    command
        .about("Delete a release.")
        .allow_hyphen_values(true)
        .version_arg(false)
}

pub fn execute(matches: &ArgMatches) -> Result<()> {
    let config = Config::current();
    let api = Api::current();
    #[expect(clippy::unwrap_used, reason = "legacy code")]
    let version = matches.get_one::<String>("version").unwrap();
    let project = config.get_project(matches).ok();

    if api.authenticated()?.delete_release(
        &config.get_org(matches)?,
        project.as_deref(),
        version,
    )? {
        println!("Deleted release {version}!");
    } else {
        println!("Did nothing. Release with this version ({version}) does not exist.");
    }

    Ok(())
}
